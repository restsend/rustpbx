use crate::config::Config;
use crate::handler::call::CallHandlerState;
use anyhow::Result;
use axum::Router;
use std::net::SocketAddr;
use tokio::net::TcpListener;
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

fn create_router() -> Router {
    // Create router with empty state
    let router = Router::new();
    let call_state = CallHandlerState::new();
    // Merge call and WebSocket handlers
    let call_routes = crate::handler::router().with_state(call_state);
    router.merge(call_routes)
}
