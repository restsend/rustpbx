use crate::config::Config;
use crate::handler::call::CallHandlerState;
use anyhow::Result;
use axum::{
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use std::{net::SocketAddr, path::PathBuf};
use tokio::net::TcpListener;
use tower_http::{
    cors::{Any, CorsLayer},
    services::ServeDir,
};
use tracing::info;

pub struct App {
    pub config: Config,
}

pub struct AppBuilder {
    pub config: Option<Config>,
}

impl AppBuilder {
    pub fn new() -> Self {
        Self { config: None }
    }

    pub fn config(mut self, config: Config) -> Self {
        self.config = Some(config);
        self
    }

    pub fn build(self) -> Result<App> {
        let config = self.config.unwrap_or_default();
        Ok(App { config })
    }
}

impl App {
    pub async fn run(self) -> Result<()> {
        // Create router with empty state
        let app = create_router();

        // Bind to address
        let addr: SocketAddr = self.config.http_addr.parse()?;
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

        info!("Starting server on {}", addr);
        match axum::serve(listener, app).await {
            Ok(_) => info!("Server shut down gracefully"),
            Err(e) => {
                tracing::error!("Server error: {}", e);
                return Err(anyhow::anyhow!("Server error: {}", e));
            }
        }

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

fn create_router() -> Router {
    // Create router with empty state
    let router = Router::new();
    let call_state = CallHandlerState::new();

    // Serve static files
    let static_files_service = ServeDir::new("static");

    // CORS configuration to allow cross-origin requests
    let cors = CorsLayer::new()
        .allow_origin([
            "http://localhost:3000".parse().unwrap(),
            "http://127.0.0.1:3000".parse().unwrap(),
            "http://localhost:8080".parse().unwrap(),
            "http://127.0.0.1:8080".parse().unwrap(),
        ])
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
        ])
        .allow_credentials(true);

    // Merge call and WebSocket handlers with static file serving
    let call_routes = crate::handler::router().with_state(call_state);

    router
        .route("/", get(index_handler))
        .nest_service("/static", static_files_service)
        .merge(call_routes)
        .layer(cors)
}
