use crate::{
    callrecord::{CallRecordManager, CallRecordManagerBuilder},
    config::Config,
    handler::call::ActiveCallRef,
    media::engine::StreamEngine,
    proxy::{
        auth::AuthModule,
        ban::BanModule,
        call::CallModule,
        cdr::CdrModule,
        mediaproxy::MediaProxyModule,
        registrar::RegistrarModule,
        server::{SipServer, SipServerBuilder},
    },
    useragent::UserAgent,
};
use anyhow::Result;
use axum::{
    response::{Html, IntoResponse},
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
    pub proxy: Arc<SipServer>,
    pub token: CancellationToken,
    pub active_calls: Arc<Mutex<HashMap<String, ActiveCallRef>>>,
    pub stream_engine: Arc<StreamEngine>,
    pub callrecord: Arc<CallRecordManager>,
}

pub type AppState = Arc<AppStateInner>;

pub struct AppStateBuilder {
    pub config: Option<Config>,
    pub useragent: Option<Arc<UserAgent>>,
    pub stream_engine: Option<Arc<StreamEngine>>,
    pub proxy: Option<Arc<SipServer>>,
    pub callrecord: Option<Arc<CallRecordManager>>,
}

impl AppStateInner {
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
            proxy: None,
            callrecord: None,
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
    pub fn with_proxy(mut self, proxy: Arc<SipServer>) -> Self {
        self.proxy = Some(proxy);
        self
    }
    pub fn with_stream_engine(mut self, stream_engine: Arc<StreamEngine>) -> Self {
        self.stream_engine = Some(stream_engine);
        self
    }
    pub fn with_callrecord(mut self, callrecord: Arc<CallRecordManager>) -> Self {
        self.callrecord = Some(callrecord);
        self
    }
    pub async fn build(self) -> Result<AppState> {
        let config = Arc::new(self.config.unwrap_or_default());
        let token = CancellationToken::new();
        let _ = crate::media::cache::set_cache_dir(&config.media_cache_path);

        let useragent = if let Some(ua) = self.useragent {
            ua
        } else {
            let config = config.ua.clone();
            let ua_builder = crate::useragent::UserAgentBuilder::new()
                .with_token(token.child_token())
                .with_config(config);
            Arc::new(ua_builder.build().await?)
        };
        let stream_engine = self.stream_engine.unwrap_or_default();
        let proxy = if let Some(proxy) = self.proxy {
            proxy
        } else {
            let proxy_config = Arc::new(config.proxy.clone().unwrap_or_default());
            let mut proxy_builder =
                SipServerBuilder::new(proxy_config.clone()).with_cancel_token(token.child_token());

            proxy_builder = proxy_builder
                .register_module("ban", BanModule::create)
                .register_module("auth", AuthModule::create)
                .register_module("registrar", RegistrarModule::create)
                .register_module("cdr", CdrModule::create)
                .register_module("mediaproxy", MediaProxyModule::create)
                .register_module("call", CallModule::create);

            let proxy = match proxy_builder.build().await {
                Ok(p) => p,
                Err(e) => {
                    warn!("Failed to build proxy: {}", e);
                    return Err(anyhow::anyhow!("Failed to build proxy: {}", e));
                }
            };
            Arc::new(proxy)
        };

        let callrecord = if let Some(callrecord) = self.callrecord {
            callrecord
        } else {
            Arc::new(
                CallRecordManagerBuilder::new()
                    .with_cancel_token(token.child_token())
                    .with_config(config.call_record.clone().unwrap_or_default())
                    .build(),
            )
        };

        Ok(Arc::new(AppStateInner {
            config,
            useragent,
            token,
            active_calls: Arc::new(Mutex::new(HashMap::new())),
            stream_engine,
            proxy,
            callrecord,
        }))
    }
}

pub async fn run(state: AppState) -> Result<()> {
    let ua = state.useragent.clone();
    let proxy = state.proxy.clone();
    let token = state.token.clone();

    let app = create_router(state.clone());
    let addr: SocketAddr = state.config.http_addr.parse()?;
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("Failed to bind to {}: {}", addr, e);
            return Err(anyhow::anyhow!("Failed to bind to {}: {}", addr, e));
        }
    };

    let http_task = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
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
        proxy_result = proxy.serve() => {
            if let Err(e) = proxy_result {
                tracing::error!("Proxy server error: {}", e);
                return Err(anyhow::anyhow!("Proxy server error: {}", e));
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
