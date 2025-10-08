use crate::{
    call::ActiveCallRef,
    callrecord::{CallRecordManagerBuilder, CallRecordSender},
    config::Config,
    handler::middleware::clientaddr::ClientAddr,
    media::engine::StreamEngine,
    proxy::{
        acl::AclModule,
        auth::AuthModule,
        call::CallModule,
        registrar::RegistrarModule,
        server::{SipServer, SipServerBuilder},
        ws::sip_ws_handler,
    },
    useragent::{UserAgent, invitation::FnCreateInvitationHandler},
};

use anyhow::Result;
use axum::{
    Router,
    extract::WebSocketUpgrade,
    response::{Html, IntoResponse, Response},
    routing::get,
};
use chrono::{DateTime, Utc};
use std::sync::Arc;
use std::{collections::HashMap, net::SocketAddr};
use std::{path::Path, sync::atomic::AtomicU64};
use tokio::select;
use tokio::{net::TcpListener, sync::Mutex};
use tokio_util::sync::CancellationToken;
use tower_http::{
    cors::{AllowOrigin, CorsLayer},
    services::ServeDir,
};
use tracing::{debug, info, warn};

pub struct AppStateInner {
    pub config: Arc<Config>,
    pub useragent: Option<Arc<UserAgent>>,
    pub sip_server: Option<SipServer>,
    pub token: CancellationToken,
    pub active_calls: Arc<Mutex<HashMap<String, ActiveCallRef>>>,
    pub stream_engine: Arc<StreamEngine>,
    pub callrecord_sender: Option<CallRecordSender>,
    pub total_calls: AtomicU64,
    pub total_failed_calls: AtomicU64,
    pub uptime: DateTime<Utc>,
    #[cfg(feature = "console")]
    pub console: Option<Arc<crate::console::ConsoleState>>,
}

pub type AppState = Arc<AppStateInner>;

pub struct AppStateBuilder {
    pub config: Option<Config>,
    pub useragent: Option<Arc<UserAgent>>,
    pub stream_engine: Option<Arc<StreamEngine>>,
    pub callrecord_sender: Option<CallRecordSender>,
    pub cancel_token: Option<CancellationToken>,
    pub proxy_builder: Option<SipServerBuilder>,
    pub create_invitation_handler: Option<FnCreateInvitationHandler>,
}

impl AppStateInner {
    pub fn get_dump_events_file(&self, session_id: &String) -> String {
        let root = Path::new(&self.config.recorder_path);
        if !root.exists() {
            match std::fs::create_dir_all(root) {
                Ok(_) => {
                    info!("created dump events root: {}", root.to_string_lossy());
                }
                Err(e) => {
                    warn!(
                        "Failed to create dump events root: {} {}",
                        e,
                        root.to_string_lossy()
                    );
                }
            }
        }
        root.join(format!("{}.events.jsonl", session_id))
            .to_string_lossy()
            .to_string()
    }

    pub fn get_recorder_file(&self, session_id: &String) -> String {
        let root = Path::new(&self.config.recorder_path);
        if !root.exists() {
            match std::fs::create_dir_all(root) {
                Ok(_) => {
                    info!("created recorder root: {}", root.to_string_lossy());
                }
                Err(e) => {
                    warn!(
                        "Failed to create recorder root: {} {}",
                        e,
                        root.to_string_lossy()
                    );
                }
            }
        }
        let recorder_file = root.join(session_id);
        if recorder_file.extension().is_none() {
            recorder_file
                .with_extension("wav")
                .to_string_lossy()
                .to_string()
        } else {
            recorder_file.to_string_lossy().to_string()
        }
    }
}

impl AppStateBuilder {
    pub fn new() -> Self {
        Self {
            config: None,
            useragent: None,
            stream_engine: None,
            callrecord_sender: None,
            cancel_token: None,
            proxy_builder: None,
            create_invitation_handler: None,
        }
    }

    pub fn with_config(mut self, config: Config) -> Self {
        self.config = Some(config);
        self
    }

    pub fn with_useragent(mut self, useragent: Option<Arc<UserAgent>>) -> Self {
        self.useragent = useragent;
        self
    }

    pub fn with_stream_engine(mut self, stream_engine: Arc<StreamEngine>) -> Self {
        self.stream_engine = Some(stream_engine);
        self
    }

    pub fn with_callrecord_sender(mut self, sender: CallRecordSender) -> Self {
        self.callrecord_sender = Some(sender);
        self
    }

    pub fn with_proxy_builder(mut self, builder: SipServerBuilder) -> Self {
        self.proxy_builder = Some(builder);
        self
    }

    pub fn with_cancel_token(mut self, token: CancellationToken) -> Self {
        self.cancel_token = Some(token);
        self
    }

    pub async fn build(mut self) -> Result<AppState> {
        let config: Arc<Config> = Arc::new(self.config.unwrap_or_default());
        let token = self
            .cancel_token
            .unwrap_or_else(|| CancellationToken::new());
        let _ = crate::media::cache::set_cache_dir(&config.media_cache_path);

        let useragent = if let Some(ua) = self.useragent {
            Some(ua)
        } else {
            let config = config.ua.clone();
            if let Some(config) = config {
                let invite_handler = self.create_invitation_handler.take();
                let ua_builder = crate::useragent::UserAgentBuilder::new()
                    .with_cancel_token(token.child_token())
                    .with_create_invitation_handler(invite_handler)
                    .with_config(Some(config));
                Some(Arc::new(ua_builder.build().await?))
            } else {
                None
            }
        };
        let stream_engine = self.stream_engine.unwrap_or_default();

        let callrecord_sender = if let Some(sender) = self.callrecord_sender {
            Some(sender)
        } else if let Some(ref callrecord) = config.callrecord {
            let mut callrecord_manager = CallRecordManagerBuilder::new()
                .with_cancel_token(token.child_token())
                .with_config(callrecord.clone())
                .build();
            let sender = callrecord_manager.sender.clone();
            tokio::spawn(async move {
                callrecord_manager.serve().await;
            });
            Some(sender)
        } else {
            None
        };

        #[cfg(feature = "console")]
        let console_state = match config.console.clone() {
            Some(console_config) => {
                Some(crate::console::ConsoleState::initialize(console_config).await?)
            }
            None => None,
        };

        let sip_server = match self.proxy_builder {
            Some(builder) => builder.build().await.ok(),
            None => {
                if let Some(proxy_config) = config.proxy.clone() {
                    let proxy_config = Arc::new(proxy_config);
                    let builder = SipServerBuilder::new(proxy_config)
                        .with_cancel_token(token.child_token())
                        .with_callrecord_sender(callrecord_sender.clone())
                        .with_rtp_config(config.rtp_config())
                        .register_module("acl", AclModule::create)
                        .register_module("auth", AuthModule::create)
                        .register_module("registrar", RegistrarModule::create)
                        .register_module("call", CallModule::create);
                    builder.build().await.ok()
                } else {
                    None
                }
            }
        };

        let app_state = Arc::new(AppStateInner {
            config,
            useragent,
            sip_server,
            token,
            active_calls: Arc::new(Mutex::new(HashMap::new())),
            stream_engine,
            callrecord_sender,
            total_calls: AtomicU64::new(0),
            total_failed_calls: AtomicU64::new(0),
            uptime: chrono::Utc::now(),
            #[cfg(feature = "console")]
            console: console_state,
        });

        Ok(app_state)
    }
}

pub async fn run(state: AppState) -> Result<()> {
    let token = state.token.clone();
    let mut router = create_router(state.clone());
    let addr: SocketAddr = state.config.http_addr.parse()?;
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("Failed to bind to {}: {}", addr, e);
            return Err(anyhow::anyhow!("Failed to bind to {}: {}", addr, e));
        }
    };

    if let Some(ref sip_server) = state.sip_server {
        if let Some(ref ws_handler) = sip_server.inner.proxy_config.ws_handler {
            info!(
                "Registering WebSocket handler to sip server: {}",
                ws_handler
            );
            let endpoint_ref = sip_server.inner.endpoint.inner.clone();
            let token = token.clone();
            router = router.route(
                ws_handler,
                axum::routing::get(
                    async move |client_ip: ClientAddr, ws: WebSocketUpgrade| -> Response {
                        let token = token.clone();
                        ws.protocols(["sip"]).on_upgrade(async move |socket| {
                            sip_ws_handler(token, client_ip, socket, endpoint_ref.clone()).await
                        })
                    },
                ),
            );
        }
    }

    let http_task = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    );
    select! {
        http_result = http_task => {
            match http_result {
                Ok(_) => info!("Server shut down gracefully"),
                Err(e) => {
                    tracing::error!("Server error: {}", e);
                    return Err(anyhow::anyhow!("Server error: {}", e));
                }
            }
        }
        ua_result = async {
            if let Some(useragent) = &state.useragent {
                useragent.serve().await
            } else {
                token.cancelled().await;
                Ok(())
            }
        } => {
            if let Err(e) = ua_result {
                tracing::error!("User agent server error: {}", e);
                return Err(anyhow::anyhow!("User agent server error: {}", e));
            }
        }
        prx_result = async {
            if let Some(sip_server) = &state.sip_server {
                sip_server.serve().await
            } else {
                token.cancelled().await;
                Ok(())
            }
        }  => {
            if let Err(e) = prx_result {
                tracing::error!("Sip server error: {}", e);
            }
        }
        _ = token.cancelled() => {
            info!("Application shutting down due to cancellation");
        }
    }

    state.useragent.as_ref().map(|ua| {
        ua.stop();
    });
    Ok(())
}

// Index page handler
async fn index_handler(client_ip: ClientAddr) -> impl IntoResponse {
    match std::fs::read_to_string("static/index.html") {
        Ok(content) => Html(content).into_response(),
        Err(e) => {
            debug!(
                client_ip = format!("{}", client_ip),
                "Failed to read index.html: {}", e
            );
            Html("<html><body><h1>Error loading page</h1></body></html>").into_response()
        }
    }
}

fn create_router(state: AppState) -> Router {
    let router = Router::new();
    // check if static/index.html exists
    if !std::path::Path::new("static/index.html").exists() {
        tracing::error!("static/index.html does not exist");
    }
    // Serve static files
    let static_files_service = ServeDir::new("static");

    // CORS configuration to allow cross-origin requests
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::any())
        .allow_methods([
            axum::http::Method::GET,
            axum::http::Method::POST,
            axum::http::Method::PUT,
            axum::http::Method::DELETE,
            axum::http::Method::OPTIONS,
        ])
        .allow_headers([
            axum::http::header::CONTENT_TYPE,
            axum::http::header::AUTHORIZATION,
            axum::http::header::ACCEPT,
            axum::http::header::ORIGIN,
        ]);

    // Merge call and WebSocket handlers with static file serving
    let call_routes = crate::handler::router(state.clone()).with_state(state.clone());

    let mut router = router
        .route("/", get(index_handler))
        .nest_service("/static", static_files_service)
        .merge(call_routes)
        .layer(cors);

    #[cfg(feature = "console")]
    if let Some(console_state) = state.console.clone() {
        router = router.merge(crate::console::handlers::router(console_state));
    }

    router
}
