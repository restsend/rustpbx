use crate::{
    callrecord::{
        CallRecordFormatter, CallRecordManagerBuilder, CallRecordSender, DefaultCallRecordFormatter,
    },
    config::{Config, UserBackendConfig},
    handler::middleware::clientaddr::ClientAddr,
    models::call_record::DatabaseHook,
    proxy::{
        acl::AclModule,
        auth::AuthModule,
        call::CallModule,
        presence::PresenceModule,
        registrar::RegistrarModule,
        server::{SipServer, SipServerBuilder},
        ws::sip_ws_handler,
    },
};

use anyhow::Result;
use axum::{
    Json, Router,
    extract::{State, WebSocketUpgrade},
    http::StatusCode,
    middleware,
    response::{Html, IntoResponse, Response},
    routing::get,
};
use axum_server::tls_rustls::RustlsConfig;
use chrono::{DateTime, Utc};
use sea_orm::DatabaseConnection;
use std::net::SocketAddr;
use std::sync::Arc;
use std::{
    path::Path,
    sync::atomic::{AtomicBool, AtomicU64},
};
use tokio::net::TcpListener;
use tokio::select;
use tokio_util::sync::CancellationToken;
use tower_http::{
    compression::CompressionLayer,
    cors::{AllowOrigin, CorsLayer},
    services::ServeDir,
};
use tracing::{info, warn};

pub struct CoreContext {
    pub config: Arc<Config>,
    pub db: DatabaseConnection,
    pub token: CancellationToken,
    pub callrecord_sender: Option<CallRecordSender>,
    pub callrecord_stats: Option<Arc<crate::callrecord::CallRecordStats>>,
    pub storage: crate::storage::Storage,
}

pub struct AppStateInner {
    pub core: Arc<CoreContext>,
    pub sip_server: SipServer,
    pub total_calls: AtomicU64,
    pub total_failed_calls: AtomicU64,
    pub uptime: DateTime<Utc>,
    pub config_loaded_at: DateTime<Utc>,
    pub config_path: Option<String>,
    pub reload_requested: AtomicBool,
    pub addon_registry: Arc<crate::addons::registry::AddonRegistry>,
    #[cfg(feature = "console")]
    pub console: Option<Arc<crate::console::ConsoleState>>,
}

pub type AppState = Arc<AppStateInner>;

pub struct AppStateBuilder {
    pub config: Option<Config>,
    pub callrecord_sender: Option<CallRecordSender>,
    pub callrecord_formatter: Option<Arc<dyn CallRecordFormatter>>,
    pub cancel_token: Option<CancellationToken>,
    pub proxy_builder: Option<SipServerBuilder>,
    pub config_loaded_at: Option<DateTime<Utc>>,
    pub config_path: Option<String>,
    pub skip_sip_bind: bool,
}

impl AppStateInner {
    pub fn config(&self) -> &Arc<Config> {
        &self.core.config
    }

    pub fn db(&self) -> &DatabaseConnection {
        &self.core.db
    }

    pub fn token(&self) -> &CancellationToken {
        &self.core.token
    }

    pub fn sip_server(&self) -> &SipServer {
        &self.sip_server
    }

    pub fn get_dump_events_file(&self, session_id: &String) -> String {
        let sanitized_id = crate::utils::sanitize_id(session_id);
        let recorder_root = self.config().recorder_path();
        let root = Path::new(&recorder_root);
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
        root.join(format!("{}.events.jsonl", sanitized_id))
            .to_string_lossy()
            .to_string()
    }

    pub fn get_recorder_file(&self, session_id: &String) -> String {
        let sanitized_id = crate::utils::sanitize_id(session_id);
        let recorder_root = self.config().recorder_path();
        let root = Path::new(&recorder_root);
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
        let mut recorder_file = root.join(sanitized_id);
        recorder_file.set_extension("wav");
        recorder_file.to_string_lossy().to_string()
    }
}

impl AppStateBuilder {
    pub fn new() -> Self {
        Self {
            config: None,
            callrecord_sender: None,
            callrecord_formatter: None,
            cancel_token: None,
            proxy_builder: None,
            config_loaded_at: None,
            config_path: None,
            skip_sip_bind: false,
        }
    }

    pub fn with_skip_sip_bind(mut self) -> Self {
        self.skip_sip_bind = true;
        self
    }

    pub fn with_config(mut self, mut config: Config) -> Self {
        if config.ensure_recording_defaults() {
            warn!(
                "recorder_format=ogg requires compiling with the 'opus' feature; falling back to wav"
            );
        }
        self.config = Some(config);
        if self.config_loaded_at.is_none() {
            self.config_loaded_at = Some(Utc::now());
        }
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

    pub fn with_config_metadata(mut self, path: Option<String>, loaded_at: DateTime<Utc>) -> Self {
        self.config_path = path;
        self.config_loaded_at = Some(loaded_at);
        self
    }

    pub async fn build(self) -> Result<AppState> {
        let config: Arc<Config> = Arc::new(self.config.unwrap_or_default());
        let storage_config = config.storage.clone().unwrap_or_default();
        let storage = crate::storage::Storage::new(&storage_config)?;

        let token = self
            .cancel_token
            .unwrap_or_else(|| CancellationToken::new());
        let config_loaded_at = self.config_loaded_at.unwrap_or_else(|| Utc::now());
        let config_path = self.config_path.clone();
        let db_conn = crate::models::create_db(&config.database_url).await?;

        let addon_registry = Arc::new(crate::addons::registry::AddonRegistry::new());
        let callrecord_formatter = if let Some(formatter) = self.callrecord_formatter {
            formatter
        } else {
            let formatter = if let Some(ref callrecord) = config.callrecord {
                DefaultCallRecordFormatter::new_with_config(callrecord)
            } else {
                DefaultCallRecordFormatter::default()
            };
            Arc::new(formatter)
        };

        let mut callrecord_stats = None;
        let mut callrecord_manager = None;
        let callrecord_sender = if let Some(sender) = self.callrecord_sender {
            Some(sender)
        } else if let Some(ref callrecord) = config.callrecord {
            let mut builder = CallRecordManagerBuilder::new()
                .with_cancel_token(token.child_token())
                .with_config(callrecord.clone())
                .with_formatter(callrecord_formatter.clone())
                .with_hook(Box::new(DatabaseHook {
                    db: db_conn.clone(),
                }));

            for hook in addon_registry.get_call_record_hooks(&config, &db_conn) {
                builder = builder.with_hook(hook);
            }

            let manager = builder.build();
            let sender = manager.sender.clone();
            callrecord_stats = Some(manager.stats.clone());
            callrecord_manager = Some(manager);
            Some(sender)
        } else {
            None
        };

        #[cfg(feature = "console")]
        let console_state = match config.console.clone() {
            Some(console_config) => Some(
                crate::console::ConsoleState::initialize(
                    callrecord_formatter,
                    db_conn.clone(),
                    console_config,
                )
                .await?,
            ),
            None => None,
        };

        let core = Arc::new(CoreContext {
            config: config.clone(),
            db: db_conn.clone(),
            token: token.clone(),
            callrecord_sender: callrecord_sender.clone(),
            callrecord_stats: callrecord_stats.clone(),
            storage: storage.clone(),
        });

        let sip_server = match self.proxy_builder {
            Some(builder) => builder.build().await,
            None => {
                let mut proxy_config = config.proxy.clone();
                for backend in proxy_config.user_backends.iter_mut() {
                    if let UserBackendConfig::Extension { database_url, .. } = backend {
                        if database_url.is_none() {
                            *database_url = Some(config.database_url.clone());
                        }
                    }
                }
                if proxy_config.recording.is_none() {
                    proxy_config.recording = config.recording.clone();
                }

                proxy_config.ensure_recording_defaults();
                let proxy_config = Arc::new(proxy_config);
                let call_record_hooks = addon_registry.get_call_record_hooks(&config, &db_conn);

                #[allow(unused_mut)]
                let mut builder = SipServerBuilder::new(proxy_config.clone())
                    .with_cancel_token(core.token.child_token())
                    .with_callrecord_sender(core.callrecord_sender.clone())
                    .with_rtp_config(config.rtp_config())
                    .with_database_connection(core.db.clone())
                    .with_call_record_hooks(call_record_hooks)
                    .with_storage(core.storage.clone())
                    .with_sipflow_config(config.sipflow.clone())
                    .with_no_bind(self.skip_sip_bind)
                    .register_module("acl", AclModule::create)
                    .register_module("auth", AuthModule::create)
                    .register_module("presence", PresenceModule::create)
                    .register_module("registrar", RegistrarModule::create)
                    .register_module("call", CallModule::create);

                builder = addon_registry.apply_proxy_server_hooks(builder, core.clone());
                builder.build().await
            }
        }?;

        let app_state = Arc::new(AppStateInner {
            core: core.clone(),
            sip_server,
            total_calls: AtomicU64::new(0),
            total_failed_calls: AtomicU64::new(0),
            uptime: chrono::Utc::now(),
            config_loaded_at,
            config_path,
            reload_requested: AtomicBool::new(false),
            addon_registry: addon_registry.clone(),
            #[cfg(feature = "console")]
            console: console_state,
        });

        if let Some(mut manager) = callrecord_manager {
            tokio::spawn(async move {
                manager.serve().await;
            });
        }

        // Initialize addons
        if let Err(e) = addon_registry.initialize_all(app_state.clone()).await {
            tracing::error!("Failed to initialize addons: {}", e);
        }

        #[cfg(feature = "console")]
        {
            if let Some(ref console_state) = app_state.console {
                console_state.set_sip_server(Some(app_state.sip_server().get_inner()));
                console_state.set_app_state(Some(Arc::downgrade(&app_state)));
            }
        }

        Ok(app_state)
    }
}

pub async fn run(state: AppState, mut router: Router) -> Result<()> {
    let token = state.token().clone();
    let addr: SocketAddr = state.config().http_addr.parse()?;
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("Failed to bind to {}: {}", addr, e);
            return Err(anyhow::anyhow!("Failed to bind to {}: {}", addr, e));
        }
    };

    if let Some(ref ws_handler) = state.sip_server().inner.proxy_config.ws_handler {
        info!(
            "Registering WebSocket handler to sip server: {}",
            ws_handler
        );
        let endpoint_ref = state.sip_server().inner.endpoint.inner.clone();
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

    // Check for HTTPS config
    let mut ssl_config = None;
    if let (Some(cert), Some(key)) = (
        &state.config().ssl_certificate,
        &state.config().ssl_private_key,
    ) {
        ssl_config = Some((cert.clone(), key.clone()));
    } else {
        // Auto-detect from config/certs
        let cert_dir = std::path::Path::new("config/certs");
        if cert_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(cert_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|s| s.to_str()) == Some("crt") {
                        let key_path = path.with_extension("key");
                        if key_path.exists() {
                            ssl_config = Some((
                                path.to_string_lossy().to_string(),
                                key_path.to_string_lossy().to_string(),
                            ));
                            break;
                        }
                    }
                }
            }
        }
    }

    let https_config = if let Some((cert, key)) = ssl_config {
        match RustlsConfig::from_pem_file(&cert, &key).await {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::error!("Failed to load SSL certs: {}", e);
                None
            }
        }
    } else {
        None
    };

    let https_addr = if https_config.is_some() {
        let addr_str = state
            .config()
            .https_addr
            .as_deref()
            .unwrap_or("0.0.0.0:8443");
        match addr_str.parse::<SocketAddr>() {
            Ok(a) => Some(a),
            Err(e) => {
                tracing::error!("Invalid HTTPS address {}: {}", addr_str, e);
                None
            }
        }
    } else {
        None
    };

    if let Some(addr) = https_addr {
        info!("HTTPS enabled on {}", addr);
    }

    let http_task = axum::serve(
        listener,
        router
            .clone()
            .into_make_service_with_connect_info::<SocketAddr>(),
    );

    let https_router = if https_addr.is_some() {
        router.layer(middleware::from_fn(
            |mut req: axum::extract::Request, next: axum::middleware::Next| async move {
                req.headers_mut()
                    .insert("x-forwarded-proto", "https".parse().unwrap());
                next.run(req).await
            },
        ))
    } else {
        router
    };

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
        https_result = async {
            if let (Some(config), Some(addr)) = (https_config, https_addr) {
                 axum_server::bind_rustls(addr, config)
                    .serve(https_router.into_make_service_with_connect_info::<SocketAddr>())
                    .await
            } else {
                std::future::pending().await
            }
        } => {
             match https_result {
                Ok(_) => info!("HTTPS Server shut down gracefully"),
                Err(e) => {
                    tracing::error!("HTTPS Server error: {}", e);
                    return Err(anyhow::anyhow!("HTTPS Server error: {}", e));
                }
            }
        }
        prx_result = state.sip_server().serve()  => {
            if let Err(e) = prx_result {
                tracing::error!("Sip server error: {}", e);
            }
        }
        _ = token.cancelled() => {
            info!("Application shutting down due to cancellation");
        }
    }
    Ok(())
}

// ICE servers handler
async fn iceservers_handler(State(state): State<AppState>) -> impl IntoResponse {
    let ice_servers = state.config().ice_servers.clone().unwrap_or_default();
    Json(ice_servers).into_response()
}

pub fn create_router(state: AppState) -> Router {
    let mut router = Router::new();

    // Serve static files
    let static_files_service = ServeDir::new("static");

    // If static/index.html exists, serve it at the root
    if std::path::Path::new("static/index.html").exists() {
        router = router.route(
            "/",
            get(|| async {
                match tokio::fs::read_to_string("static/index.html").await {
                    Ok(content) => Html(content).into_response(),
                    Err(_) => StatusCode::NOT_FOUND.into_response(),
                }
            }),
        );
    }

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
    let call_routes = crate::handler::ami_router(state.clone()).with_state(state.clone());
    #[allow(unused_mut)]
    let mut router = router
        .route(
            "/iceservers",
            get(iceservers_handler).with_state(state.clone()),
        )
        .merge(state.addon_registry.get_routers(state.clone()))
        .nest_service("/static", static_files_service)
        .merge(call_routes)
        .layer(cors);

    #[cfg(feature = "console")]
    if let Some(console_state) = state.console.clone() {
        router = router.merge(crate::console::router(console_state));
    }

    let access_log_skip_paths = Arc::new(state.config().http_access_skip_paths.clone());

    router = router.layer(middleware::from_fn_with_state(
        access_log_skip_paths,
        crate::handler::middleware::request_log::log_requests,
    ));

    if state.config().http_gzip {
        router = router.layer(CompressionLayer::new().gzip(true));
    }

    router
}
