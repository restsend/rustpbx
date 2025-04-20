use crate::useragent::UserAgent;
use crate::{config::Config, handler::call::ActiveCallRef};
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
    pub recorder_root: String,
    pub llm_proxy: Option<String>,
}

pub type AppState = Arc<AppStateInner>;

pub struct App {
    pub state: AppState,
}

pub struct AppBuilder {
    pub config: Option<Config>,
    pub useragent: Option<Arc<UserAgent>>,
}

impl AppStateInner {
    pub fn new(useragent: Arc<UserAgent>) -> Self {
        Self {
            config: Arc::new(Config::default()),
            useragent,
            token: CancellationToken::new(),
            active_calls: Arc::new(Mutex::new(HashMap::new())),
            recorder_root: std::env::var("RECORDER_ROOT")
                .unwrap_or_else(|_| "/tmp/recorder".to_string()),
            llm_proxy: std::env::var("LLM_PROXY").ok(),
        }
    }
    pub fn get_recorder_file(&self, session_id: &String) -> String {
        let root = Path::new(&self.recorder_root);
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
impl AppBuilder {
    pub fn new() -> Self {
        Self {
            config: None,
            useragent: None,
        }
    }

    pub fn config(mut self, config: Config) -> Self {
        self.config = Some(config);
        self
    }

    pub fn useragent(mut self, useragent: Arc<UserAgent>) -> Self {
        self.useragent = Some(useragent);
        self
    }

    pub async fn build(self) -> Result<App> {
        let config = Arc::new(self.config.unwrap_or_default());
        let token = CancellationToken::new();

        let useragent = if let Some(ua) = self.useragent {
            ua
        } else {
            let external_ip = config.sip.as_ref().and_then(|s| s.external_ip.clone());
            let ua_builder = crate::useragent::UserAgentBuilder::new()
                .external_ip(external_ip)
                .rtp_start_port(12000);

            Arc::new(ua_builder.build().await?)
        };

        let recorder_root =
            std::env::var("RECORDER_ROOT").unwrap_or_else(|_| "/tmp/recorder".to_string());
        let llm_proxy = std::env::var("LLM_PROXY").ok();

        Ok(App {
            state: Arc::new(AppStateInner {
                config,
                useragent,
                token,
                active_calls: Arc::new(Mutex::new(HashMap::new())),
                recorder_root,
                llm_proxy,
            }),
        })
    }
}

impl App {
    pub async fn run(self) -> Result<()> {
        // Get components ready
        let ua = self.state.useragent.clone();
        let token = self.state.token.child_token();

        // Create router with empty state
        let app = create_router(self.state.clone());

        // Bind to address
        let addr: SocketAddr = self.state.config.http_addr.parse()?;
        info!("Attempting to bind to {}", addr);

        // Start HTTP server with Axum 0.8.1
        let listener = match TcpListener::bind(addr).await {
            Ok(l) => {
                info!("Successfully bound to {}", addr);
                l
            }
            Err(e) => {
                tracing::error!("Failed to bind to {}: {}", addr, e);
                return Err(anyhow::anyhow!("Failed to bind to {}: {}", addr, e));
            }
        };

        // Run HTTP server and SIP server in parallel
        info!("Starting server on {}", addr);

        let http_task = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        );

        tokio::select! {
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

        // Clean shutdown
        self.state.useragent.stop();

        Ok(())
    }
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
