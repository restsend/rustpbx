use anyhow::Result;
use chrono::Utc;
use clap::{Parser, Subcommand};
use dotenv::dotenv;
use rustpbx::{app::AppStateBuilder, config::Config, preflight, version};
use std::path::Path;
use std::sync::atomic::Ordering;
use tokio::time::{Duration, sleep};
use tracing::{info, level_filters::LevelFilter};
use tracing_subscriber::{
    EnvFilter, fmt::time::LocalTime, layer::SubscriberExt, util::SubscriberInitExt,
};

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
    #[clap(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Validate configuration and exit without starting the server
    CheckConfig,
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
        Config::load(path).expect("Failed to load config")
    } else {
        Config::default()
    };

    println!("{}", version::get_version_info());

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

    let mut env_filter = EnvFilter::from_default_env();
    if let Some(Ok(level)) = config
        .log_level
        .as_ref()
        .map(|level| level.parse::<LevelFilter>())
    {
        env_filter = env_filter.add_directive(level.into());
    }

    let tokio_console_enabled = cli.tokio_console.is_some()
        || std::env::var_os("TOKIO_CONSOLE").is_some()
        || std::env::var_os("TOKIO_CONSOLE_BIND").is_some();
    let mut console_layer = None;
    if tokio_console_enabled {
        use console_subscriber::ServerAddr;
        let mut builder = console_subscriber::ConsoleLayer::builder()
            .retention(std::time::Duration::from_secs(60));
        if let Some(addr) = &cli.tokio_console {
            if let Ok(sock) = addr.parse() {
                builder = builder.server_addr(ServerAddr::Tcp(sock));
            } else {
                builder = builder.server_addr(Path::new(addr));
            }
        } else {
            builder = builder.server_addr(Path::new("/tmp/tokio-console"));
        }
        console_layer = Some(builder.spawn());
    }
    let mut file_layer = None;
    let mut guard_holder = None;
    let mut fmt_layer = None;
    if let Some(ref log_file) = config.log_file {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_file)
            .expect("Failed to open log file");
        let (non_blocking, guard) = tracing_appender::non_blocking(file);
        guard_holder = Some(guard);
        file_layer = Some(
            tracing_subscriber::fmt::layer()
                .with_timer(LocalTime::rfc_3339())
                .with_file(true)
                .with_line_number(true)
                .with_target(true)
                .with_ansi(false)
                .with_writer(non_blocking),
        );
    } else {
        fmt_layer = Some(
            tracing_subscriber::fmt::layer()
                .with_timer(LocalTime::rfc_3339())
                .with_file(true)
                .with_line_number(true)
                .with_target(true),
        );
    }

    if let Some(console_layer) = console_layer {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(console_layer)
            .try_init()?;
    } else if let Some(file_layer) = file_layer {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(file_layer)
            .try_init()?;
    } else if let Some(fmt_layer) = fmt_layer {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt_layer)
            .try_init()?;
    }

    let _ = guard_holder; // keep the guard alive

    let mut cached_config = Some(config);
    let mut next_config_path = config_path.clone();

    loop {
        let config = if let Some(cfg) = cached_config.take() {
            cfg
        } else if let Some(ref path) = next_config_path {
            Config::load(path)
                .map_err(|err| anyhow::anyhow!("Failed to load config from {}: {}", path, err))?
        } else {
            Config::default()
        };

        let state_builder = AppStateBuilder::new()
            .with_config(config)
            .with_config_metadata(next_config_path.clone(), Utc::now());
        let state = state_builder.build().await.expect("Failed to build app");

        info!("starting rustpbx on {}", state.config.http_addr);

        let mut app_future = Box::pin(rustpbx::app::run(state.clone()));

        #[cfg(unix)]
        let mut sigterm_stream = {
            use tokio::signal::unix::{SignalKind, signal};
            signal(SignalKind::terminate()).expect("failed to install signal handler")
        };

        let mut reload_requested = false;

        #[cfg(unix)]
        {
            tokio::select! {
                result = &mut app_future => {
                    if let Err(err) = result {
                        return Err(err);
                    }
                    reload_requested = state.reload_requested.load(Ordering::Relaxed);
                }
                _ = tokio::signal::ctrl_c() => {
                    info!("received CTRL+C, shutting down");
                    state.token.cancel();
                    let _ = app_future.await;
                }
                _ = sigterm_stream.recv() => {
                    info!("received SIGTERM, shutting down");
                    state.token.cancel();
                    let _ = app_future.await;
                }
            }
        }

        #[cfg(not(unix))]
        {
            tokio::select! {
                result = &mut app_future => {
                    if let Err(err) = result {
                        return Err(err);
                    }
                    reload_requested = state.reload_requested.load(Ordering::Relaxed);
                }
                _ = tokio::signal::ctrl_c() => {
                    info!("received CTRL+C, shutting down");
                    state.token.cancel();
                    let _ = app_future.await;
                }
            }
        }

        if reload_requested {
            info!("Reload requested; restarting with updated configuration");
            next_config_path = state.config_path.clone();
            cached_config = None;
            sleep(Duration::from_millis(300)).await;
            continue;
        }

        break;
    }

    Ok(())
}
