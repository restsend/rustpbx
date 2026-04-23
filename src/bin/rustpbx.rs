use anyhow::Result;
use chrono::Utc;
use clap::{Parser, Subcommand};
use dotenvy::dotenv;
use rustpbx::{
    app::{AppStateBuilder, create_router},
    config::Config,
    handler::middleware::request_log::AccessLogEventFormat,
    observability, preflight, version,
};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::time::{Duration, sleep};
use tracing::info;
use tracing_subscriber::{
    EnvFilter, fmt::time::LocalTime, layer::SubscriberExt, util::SubscriberInitExt,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LogRotationMode {
    Never,
    Hourly,
    Daily,
}

impl LogRotationMode {
    fn from_config(value: &str) -> Self {
        match value.to_lowercase().as_str() {
            "hourly" => Self::Hourly,
            "daily" => Self::Daily,
            _ => Self::Never,
        }
    }

    fn period_key(self, now: chrono::DateTime<chrono::Local>) -> Option<String> {
        match self {
            Self::Hourly => Some(now.format("%Y-%m-%d-%H").to_string()),
            Self::Daily => Some(now.format("%Y-%m-%d").to_string()),
            Self::Never => None,
        }
    }
}

#[derive(Clone)]
struct ActiveFileRollingWriter {
    inner: Arc<parking_lot::Mutex<ActiveFileRollingWriterInner>>,
}

struct ActiveFileRollingWriterInner {
    base_path: PathBuf,
    mode: LogRotationMode,
    current_period: Option<String>,
    file: Option<File>,
}

impl ActiveFileRollingWriter {
    fn new(base_path: impl Into<PathBuf>, mode: LogRotationMode) -> io::Result<Self> {
        let mut inner = ActiveFileRollingWriterInner {
            base_path: base_path.into(),
            mode,
            current_period: None,
            file: None,
        };
        inner.ensure_ready()?;
        Ok(Self {
            inner: Arc::new(parking_lot::Mutex::new(inner)),
        })
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for ActiveFileRollingWriter {
    type Writer = ActiveFileRollingWriteGuard;

    fn make_writer(&'a self) -> Self::Writer {
        ActiveFileRollingWriteGuard {
            inner: self.inner.clone(),
        }
    }
}

struct ActiveFileRollingWriteGuard {
    inner: Arc<parking_lot::Mutex<ActiveFileRollingWriterInner>>,
}

impl ActiveFileRollingWriteGuard {
    fn from_writer(writer: ActiveFileRollingWriter) -> Self {
        Self {
            inner: writer.inner,
        }
    }
}

impl Write for ActiveFileRollingWriteGuard {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut inner = self.inner.lock();
        inner.ensure_ready()?;
        if let Some(file) = inner.file.as_mut() {
            file.write(buf)
        } else {
            Err(io::Error::other("log writer is unavailable"))
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut inner = self.inner.lock();
        if let Some(file) = inner.file.as_mut() {
            file.flush()
        } else {
            Ok(())
        }
    }
}

impl ActiveFileRollingWriterInner {
    fn ensure_ready(&mut self) -> io::Result<()> {
        let now = chrono::Local::now();
        if self.mode == LogRotationMode::Never {
            if self.file.is_none() {
                self.file = Some(self.open_active_file()?);
            }
            return Ok(());
        }

        let next_period = self.mode.period_key(now).unwrap_or_default();
        let needs_rotate = self.current_period.as_deref() != Some(next_period.as_str());
        if needs_rotate {
            self.rotate(next_period)?;
        } else if self.file.is_none() {
            self.file = Some(self.open_active_file()?);
        }
        Ok(())
    }

    fn rotate(&mut self, next_period: String) -> io::Result<()> {
        if let Some(mut old_file) = self.file.take() {
            old_file.flush()?;
        }

        if let Some(ref old_period) = self.current_period
            && self.base_path.exists() {
                let archived = self.next_archive_path(old_period);
                fs::rename(&self.base_path, archived)?;
            }

        self.file = Some(self.open_active_file()?);
        self.current_period = Some(next_period);
        Ok(())
    }

    fn open_active_file(&self) -> io::Result<File> {
        if let Some(parent) = self.base_path.parent()
            && !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.base_path)
    }

    fn next_archive_path(&self, period: &str) -> PathBuf {
        let parent = self.base_path.parent().unwrap_or_else(|| Path::new("."));
        let base_name = self
            .base_path
            .file_name()
            .map(|v| v.to_string_lossy().into_owned())
            .unwrap_or_else(|| String::from("rustpbx.log"));

        let mut candidate = parent.join(format!("{}.{}", base_name, period));
        if !candidate.exists() {
            return candidate;
        }

        for idx in 1.. {
            candidate = parent.join(format!("{}.{}.{}", base_name, period, idx));
            if !candidate.exists() {
                return candidate;
            }
        }

        unreachable!("archive path selection should always terminate")
    }
}

#[derive(Parser, Debug)]
#[command(
    author,
    version = version::get_short_version(),
    about = "A versatile SIP PBX server implemented in Rust",
    long_about = version::get_version_info()
)]
struct Cli {
    /// Path to the configuration file
    #[clap(
        long,
        global = true,
        help = "Path to the configuration file (TOML format)"
    )]
    conf: Option<String>,
    #[clap(
        long,
        global = true,
        help = "Tokio console server address, e.g. /tmp/tokio-console or 127.0.0.1:5556"
    )]
    tokio_console: Option<String>,
    #[cfg(feature = "console")]
    #[clap(
        long,
        global = true,
        requires = "super_password",
        help = "Create or update a console super user before starting the server"
    )]
    super_username: Option<String>,
    #[cfg(feature = "console")]
    #[clap(
        long,
        global = true,
        requires = "super_username",
        help = "Password for the console super user"
    )]
    super_password: Option<String>,
    #[cfg(feature = "console")]
    #[clap(
        long,
        global = true,
        requires = "super_username",
        help = "Email for the console super user (defaults to username@localhost)"
    )]
    super_email: Option<String>,
    #[clap(
        long,
        global = true,
        help = "Skip running database migrations on startup"
    )]
    skip_migrate: bool,
    #[clap(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Validate configuration and exit without starting the server
    CheckConfig,
    /// Initialize with fixture data (extensions, routes, wholesale demo data)
    Fixtures,
}

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");
    sqlx::any::install_default_drivers();

    dotenv().ok();
    let cli = Cli::parse();

    let config_path = cli.conf.clone();
    let config = if let Some(ref path) = config_path {
        println!("Loading config from: {}", path);
        Config::load(path).expect("Failed to load config")
    } else {
        println!("Loading default config");
        Config::default()
    };

    println!("Start at {}", Utc::now());
    println!("{}", version::get_version_info());

    if matches!(cli.command, Some(Commands::Fixtures)) {
        let state = AppStateBuilder::new()
            .with_config(config.clone())
            .with_config_metadata(config_path.clone(), Utc::now())
            .with_skip_sip_bind()
            .build()
            .await
            .expect("Failed to build app state for fixtures");

        state
            .addon_registry
            .initialize_all(state.clone())
            .await
            .expect("Failed to initialize addons for fixtures");

        rustpbx::fixtures::run_fixtures(state).await?;
        println!("Fixtures initialized successfully.");
        return Ok(());
    }

    if matches!(cli.command, Some(Commands::CheckConfig)) {
        match preflight::validate_start(&config).await {
            Ok(_) => {
                println!("Configuration is valid; all required sockets are available.");
                return Ok(());
            }
            Err(err) => {
                eprintln!("Configuration validation failed:");
                for issue in err.issues {
                    eprintln!("- {}: {}", issue.field, issue.message);
                }
                std::process::exit(1);
            }
        }
    }

    #[cfg(feature = "console")]
    if let Some(super_username) = cli.super_username.as_deref() {
        let super_password = cli
            .super_password
            .as_deref()
            .expect("super_password is required when super_username is provided");
        let super_email = cli
            .super_email
            .as_deref()
            .map(|email| email.to_string())
            .unwrap_or_else(|| format!("{}@localhost", super_username));

        let db = rustpbx::models::create_db(&config.database_url)
            .await
            .expect("Failed to create or connect to database");

        rustpbx::models::user::Model::upsert_super_user(
            &db,
            super_username,
            &super_email,
            super_password,
        )
        .await
        .expect("Failed to create or update super user");
        println!(
            "Console super user '{}' ensured with email '{}'",
            super_username, super_email
        );
        return Ok(());
    }

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        let mut env_filter = if let Some(level) = config.log_level.as_deref() {
            EnvFilter::new(level)
        } else {
            EnvFilter::new("info")
        };

        // Suppress noisy third-party crates to warn level by default when
        // the user did not provide an explicit RUST_LOG override.
        for noisy in &["hyper_util", "rustls", "sqlx"] {
            if let Ok(d) = format!("{}=warn", noisy).parse() {
                env_filter = env_filter.add_directive(d);
            }
        }

        env_filter
    });

    // Install the hot-swappable reload layer BEFORE the subscriber is built.
    // The commercial TelemetryAddon will inject an OTel layer into this slot
    // during addon initialization.
    let otel_reload_layer = observability::init_reload_layer();

    let tokio_console_enabled = cli.tokio_console.is_some()
        || std::env::var_os("TOKIO_CONSOLE").is_some()
        || std::env::var_os("TOKIO_CONSOLE_BIND").is_some();
    let mut console_layer = None;
    if tokio_console_enabled {
        use console_subscriber::ServerAddr;
        let mut builder = console_subscriber::ConsoleLayer::builder()
            .retention(std::time::Duration::from_secs(60));
        if let Some(addr) = &cli.tokio_console {
            builder = match addr.parse::<SocketAddr>() {
                Ok(sock) => builder.server_addr(ServerAddr::Tcp(sock)),
                Err(_) => {
                    #[cfg(unix)]
                    {
                        builder.server_addr(ServerAddr::Unix(PathBuf::from(addr)))
                    }
                    #[cfg(not(unix))]
                    {
                        tracing::warn!(
                            "tokio-console unix socket path '{}' is not supported on this target, falling back to 127.0.0.1:6669",
                            addr
                        );
                        builder.server_addr(ServerAddr::Tcp(
                            "127.0.0.1:6669".parse().expect("valid socket addr"),
                        ))
                    }
                }
            };
        } else {
            builder = {
                #[cfg(unix)]
                {
                    builder.server_addr(ServerAddr::Unix(PathBuf::from("/tmp/tokio-console")))
                }
                #[cfg(not(unix))]
                {
                    builder.server_addr(ServerAddr::Tcp(
                        "127.0.0.1:6669".parse().expect("valid socket addr"),
                    ))
                }
            };
        }
        console_layer = Some(builder.spawn());
    }
    let mut file_layer = None;
    let mut guard_holder = None;
    let mut fmt_layer = None;
    if let Some(ref log_file) = config.log_file {
        let log_path = PathBuf::from(log_file);
        let mode = LogRotationMode::from_config(&config.log_rotation);
        let appender = ActiveFileRollingWriter::new(log_path, mode)?;
        let (non_blocking, guard) = tracing_appender::non_blocking(
            ActiveFileRollingWriteGuard::from_writer(appender),
        );
        guard_holder = Some(guard);
        file_layer = Some(
            tracing_subscriber::fmt::layer()
                .with_timer(LocalTime::rfc_3339())
                .event_format(AccessLogEventFormat::new(LocalTime::rfc_3339()))
                .with_ansi(false)
                .with_writer(non_blocking),
        );
    } else {
        fmt_layer = Some(
            tracing_subscriber::fmt::layer()
                .with_timer(LocalTime::rfc_3339())
                .event_format(AccessLogEventFormat::new(LocalTime::rfc_3339())),
        );
    }
    // Every branch receives the same OTel reload layer so that the commercial
    // TelemetryAddon can inject a live OTel tracing layer later, regardless of
    // which logging backend was chosen.
    if let Some(console_layer) = console_layer {
        tracing_subscriber::registry()
            .with(otel_reload_layer)
            .with(env_filter)
            .with(console_layer)
            .try_init()?;
    } else if let Some(file_layer) = file_layer {
        tracing_subscriber::registry()
            .with(otel_reload_layer)
            .with(env_filter)
            .with(file_layer)
            .try_init()?;
    } else if let Some(fmt_layer) = fmt_layer {
        tracing_subscriber::registry()
            .with(otel_reload_layer)
            .with(env_filter)
            .with(fmt_layer)
            .try_init()?;
    }

    let _ = guard_holder; // keep the guard alive

    let mut cached_config = Some(config);
    let mut next_config_path = config_path.clone();
    let mut retry_count = 0;
    let max_retries = 10;
    let retry_interval = Duration::from_secs(5);

    loop {
        let config = if let Some(cfg) = cached_config.take() {
            cfg
        } else if let Some(ref path) = next_config_path {
            match Config::load(path) {
                Ok(cfg) => cfg,
                Err(err) => {
                    retry_count += 1;
                    if retry_count > max_retries {
                        return Err(anyhow::anyhow!(
                            "Failed to load config from {} after {} retries: {}",
                            path,
                            max_retries,
                            err
                        ));
                    }
                    tracing::error!(
                        "Failed to load config from {} (retry {}/{}): {}. Retrying in {:?}...",
                        path,
                        retry_count,
                        max_retries,
                        err,
                        retry_interval
                    );
                    sleep(retry_interval).await;
                    continue;
                }
            }
        } else {
            Config::default()
        };

        let mut state_builder = AppStateBuilder::new()
            .with_config(config.clone())
            .with_config_metadata(next_config_path.clone(), Utc::now());
        if cli.skip_migrate {
            state_builder = state_builder.with_skip_migrate();
        }

        let (app_reload_requested, app_config_path) = {
            let state = match state_builder.build().await {
                Ok(state) => state,
                Err(err) => {
                    retry_count += 1;
                    if retry_count > max_retries {
                        return Err(anyhow::anyhow!(
                            "Failed to build app after {} retries: {}",
                            max_retries,
                            err
                        ));
                    }
                    tracing::error!(
                        "Failed to build app (retry {}/{}): {}. Retrying in {:?}...",
                        retry_count,
                        max_retries,
                        err,
                        retry_interval
                    );
                    sleep(retry_interval).await;
                    cached_config = Some(config);
                    continue;
                }
            };

            info!("starting rustpbx on {}", state.config().http_addr);
            let router = create_router(state.clone());
            let mut app_future = Box::pin(rustpbx::app::run(state.clone(), router));

            #[cfg(unix)]
            let mut sigterm_stream = {
                use tokio::signal::unix::{SignalKind, signal};
                signal(SignalKind::terminate()).expect("failed to install signal handler")
            };

            let mut reload_requested = false;
            let mut app_exit_err = None;

            #[cfg(unix)]
            {
                tokio::select! {
                    result = &mut app_future => {
                        if let Err(err) = result {
                            app_exit_err = Some(err);
                        } else {
                            reload_requested = state.reload_requested.load(Ordering::Relaxed);
                        }
                    }
                    _ = tokio::signal::ctrl_c() => {
                        info!("received CTRL+C, shutting down");
                        state.token().cancel();
                        let _ = app_future.await;
                    }
                    _ = sigterm_stream.recv() => {
                        info!("received SIGTERM, shutting down");
                        state.token().cancel();
                        let _ = app_future.await;
                    }
                }
            }

            #[cfg(not(unix))]
            {
                tokio::select! {
                    result = &mut app_future => {
                        if let Err(err) = result {
                            app_exit_err = Some(err);
                        } else {
                            reload_requested = state.reload_requested.load(Ordering::Relaxed);
                        }
                    }
                    _ = tokio::signal::ctrl_c() => {
                        info!("received CTRL+C, shutting down");
                        state.token().cancel();
                        let _ = app_future.await;
                    }
                }
            }

            if let Some(err) = app_exit_err {
                retry_count += 1;
                if retry_count > max_retries {
                    return Err(anyhow::anyhow!(
                        "Application failed after {} retries: {}",
                        max_retries,
                        err
                    ));
                }
                tracing::error!(
                    "Application error (retry {}/{}): {}. Retrying in {:?}...",
                    retry_count,
                    max_retries,
                    err,
                    retry_interval
                );
                sleep(retry_interval).await;
                cached_config = Some(config);
                continue;
            }
            (reload_requested, state.config_path.clone())
        };

        if app_reload_requested {
            info!("Reload requested; restarting with updated configuration");
            next_config_path = app_config_path;
            cached_config = None;
            retry_count = 0;

            sleep(Duration::from_secs(3)).await; // give some time for sockets to be released
            continue;
        }

        break;
    }

    // Flush any buffered OTel spans before the process exits.
    #[cfg(feature = "addon-telemetry")]
    rustpbx::addons::telemetry::TelemetryAddon::shutdown();

    Ok(())
}
