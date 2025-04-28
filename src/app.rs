use crate::event::EventSender;
use crate::handler::{call, CallOption};
use crate::media::{
    processor::Processor,
    track::{tts::TtsHandle, Track},
};
use crate::synthesis::SynthesisOption;
use crate::useragent::UserAgent;
use crate::TrackId;
use crate::{config::Config, handler::call::ActiveCallRef};
use anyhow::Result;
use axum::{
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use futures::lock::Mutex;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
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

// Define hook types
pub type PrepareTrackHook = Box<
    dyn Fn(
            &dyn Track,
            CancellationToken,
            EventSender,
            CallOption,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<Box<dyn Processor>>>> + Send>>
        + Send
        + Sync,
>;

pub type CreateTtsTrackHook = Box<
    dyn Fn(
            CancellationToken,
            TrackId,
            Option<String>,
            SynthesisOption,
        ) -> Pin<Box<dyn Future<Output = Result<(TtsHandle, Box<dyn Track>)>> + Send>>
        + Send
        + Sync,
>;

pub struct AppStateInner {
    pub config: Arc<Config>,
    pub useragent: Arc<UserAgent>,
    pub token: CancellationToken,
    pub active_calls: Arc<Mutex<HashMap<String, ActiveCallRef>>>,
    pub prepare_track_hook: PrepareTrackHook,
    pub create_tts_track_hook: CreateTtsTrackHook,
}

pub type AppState = Arc<AppStateInner>;

pub struct AppStateBuilder {
    pub config: Option<Config>,
    pub useragent: Option<Arc<UserAgent>>,
    pub prepare_track_hook: Option<PrepareTrackHook>,
    pub create_tts_track_hook: Option<CreateTtsTrackHook>,
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
            prepare_track_hook: None,
            create_tts_track_hook: None,
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

    pub fn prepare_track_hook(mut self, hook: PrepareTrackHook) -> Self {
        self.prepare_track_hook = Some(hook);
        self
    }

    pub fn create_tts_track_hook(mut self, hook: CreateTtsTrackHook) -> Self {
        self.create_tts_track_hook = Some(hook);
        self
    }

    pub async fn build(self) -> Result<AppState> {
        let config = Arc::new(self.config.unwrap_or_default());
        let token = CancellationToken::new();
        let _ = crate::media::cache::set_cache_dir(&config.media_cache_path);

        let useragent = if let Some(ua) = self.useragent {
            ua
        } else {
            let config = config.sip.clone();
            let ua_builder = crate::useragent::UserAgentBuilder::new()
                .with_token(token.child_token())
                .with_config(config);
            Arc::new(ua_builder.build().await?)
        };

        Ok(Arc::new(AppStateInner {
            config,
            useragent,
            token,
            active_calls: Arc::new(Mutex::new(HashMap::new())),
            prepare_track_hook: self
                .prepare_track_hook
                .unwrap_or_else(|| Box::new(call::ActiveCall::prepare_track_hook)),
            create_tts_track_hook: self
                .create_tts_track_hook
                .unwrap_or_else(|| Box::new(call::ActiveCall::create_tts_track)),
        }))
    }
}

pub async fn run(state: AppState) -> Result<()> {
    let ua = state.useragent.clone();
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
