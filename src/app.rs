use crate::{
    callrecord::{CallRecordManagerBuilder, CallRecordSender},
    config::{Config, ProxyConfig},
    handler::{call::ActiveCallRef, middleware::clientaddr::ClientAddr},
    media::engine::StreamEngine,
    proxy::{
        acl::AclModule,
        auth::AuthModule,
        call::CallModule,
        registrar::RegistrarModule,
        server::{SipServer, SipServerBuilder},
        ws::sip_ws_handler,
    },
    useragent::{invitation::create_invite_handler, UserAgent},
};
use anyhow::Result;
use axum::{
    extract::WebSocketUpgrade,
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use futures::lock::Mutex;
use std::path::Path;
use std::sync::Arc;
use std::{collections::HashMap, net::SocketAddr};
use tokio::net::TcpListener;
use tokio::select;
use tokio_util::sync::CancellationToken;
use tower_http::{
    cors::{AllowOrigin, CorsLayer},
    services::ServeDir,
};
use tracing::{info, warn};

pub struct AppStateInner {
    pub config: Arc<Config>,
    pub useragent: Arc<UserAgent>,
    pub token: CancellationToken,
    pub active_calls: Arc<Mutex<HashMap<String, ActiveCallRef>>>,
    pub stream_engine: Arc<StreamEngine>,
    pub callrecord_sender: tokio::sync::Mutex<Option<CallRecordSender>>,
}

pub type AppState = Arc<AppStateInner>;

pub struct AppStateBuilder {
    pub config: Option<Config>,
    pub useragent: Option<Arc<UserAgent>>,
    pub stream_engine: Option<Arc<StreamEngine>>,
    pub callrecord_sender: Option<CallRecordSender>,
    pub cancel_token: Option<CancellationToken>,
}

impl AppStateInner {
    pub fn get_dump_events_file(&self, session_id: &String) -> String {
        let root = Path::new(&self.config.recorder_path);
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
        root.join(session_id)
            .with_extension("wav")
            .to_string_lossy()
            .to_string()
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
        }
    }

    pub fn config(mut self, config: Config) -> Self {
        self.config = Some(config);
        self
    }

    pub fn with_useragent(mut self, useragent: Arc<UserAgent>) -> Self {
        self.useragent = Some(useragent);
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

    pub fn with_cancel_token(mut self, token: CancellationToken) -> Self {
        self.cancel_token = Some(token);
        self
    }

    pub async fn build(self) -> Result<AppState> {
        let config: Arc<Config> = Arc::new(self.config.unwrap_or_default());
        let token = self
            .cancel_token
            .unwrap_or_else(|| CancellationToken::new());
        let _ = crate::media::cache::set_cache_dir(&config.media_cache_path);

        let useragent = if let Some(ua) = self.useragent {
            ua
        } else {
            let config = config.ua.clone();
            let invite_handler = config
                .as_ref()
                .and_then(|c| c.handler.as_ref())
                .map(|c| create_invite_handler(c))
                .flatten();
            let ua_builder = crate::useragent::UserAgentBuilder::new()
                .with_cancel_token(token.child_token())
                .with_invitation_handler(invite_handler)
                .with_config(config);
            Arc::new(ua_builder.build().await?)
        };
        let stream_engine = self.stream_engine.unwrap_or_default();

        Ok(Arc::new(AppStateInner {
            config,
            useragent,
            token,
            active_calls: Arc::new(Mutex::new(HashMap::new())),
            stream_engine,
            callrecord_sender: tokio::sync::Mutex::new(self.callrecord_sender),
        }))
    }
}

pub async fn build_sip_server(
    token: CancellationToken,
    config: ProxyConfig,
    callrecord_sender: Option<CallRecordSender>,
) -> Result<SipServer> {
    let proxy_config = Arc::new(config);
    let mut proxy_builder = SipServerBuilder::new(proxy_config)
        .with_cancel_token(token.child_token())
        .with_callrecord_sender(callrecord_sender);

    proxy_builder = proxy_builder
        .register_module("acl", AclModule::create)
        .register_module("auth", AuthModule::create)
        .register_module("registrar", RegistrarModule::create)
        .register_module("call", CallModule::create);

    let proxy = match proxy_builder.build().await {
        Ok(p) => p,
        Err(e) => {
            warn!("Failed to build proxy: {}", e);
            return Err(anyhow::anyhow!("Failed to build proxy: {}", e));
        }
    };
    Ok(proxy)
}

pub async fn run(state: AppState) -> Result<()> {
    let ua = state.useragent.clone();
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

    if let Some(ref callrecord) = state.config.callrecord {
        let mut callrecord_sender = state.callrecord_sender.lock().await;
        if callrecord_sender.is_none() {
            let mut callrecord_manager = CallRecordManagerBuilder::new()
                .with_cancel_token(token.child_token())
                .with_config(callrecord.clone())
                .build();
            callrecord_sender.replace(callrecord_manager.sender.clone());

            tokio::spawn(async move {
                callrecord_manager.serve().await;
            });
        }
    }

    if let Some(ref proxy) = state.config.proxy {
        let token_proxy = token.clone();
        let proxy_config = proxy.clone();
        let callrecord_sender = state.callrecord_sender.lock().await.clone();
        let sip_server = match build_sip_server(token_proxy, proxy_config, callrecord_sender).await
        {
            Ok(p) => p,
            Err(e) => {
                warn!("Proxy server error: {}", e);
                return Err(anyhow::anyhow!("Proxy server error: {}", e));
            }
        };

        if let Some(ref ws_handler) = proxy.ws_handler {
            info!(
                "Registering WebSocket handler to sip server: {}",
                ws_handler
            );
            let endpoint_ref = sip_server.inner.endpoint.inner.clone();
            let token = sip_server.inner.cancel_token.clone();
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

        tokio::spawn(async move {
            info!("Proxy server started");
            match sip_server.serve().await {
                Ok(_) => {}
                Err(e) => {
                    warn!("Proxy server error: {}", e);
                }
            }
        });
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
        ua_result = ua.serve() => {
            if let Err(e) = ua_result {
                tracing::error!("User agent server error: {}", e);
                return Err(anyhow::anyhow!("User agent server error: {}", e));
            }
        }
        _ = token.cancelled() => {
            info!("Application shutting down due to cancellation");
        }
    }
    token.cancel();
    state.useragent.stop();
    Ok(())
}

// Index page handler
async fn index_handler() -> impl IntoResponse {
    match std::fs::read_to_string("static/index.html") {
        Ok(content) => Html(content).into_response(),
        Err(e) => {
            tracing::error!("Failed to read index.html: {}", e);
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
    let call_routes = crate::handler::router().with_state(state);

    router
        .route("/", get(index_handler))
        .nest_service("/static", static_files_service)
        .merge(call_routes)
        .layer(cors)
}
