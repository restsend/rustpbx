use anyhow::Result;
use clap::Parser;
use dotenv::dotenv;
use rustpbx::{app::AppStateBuilder, config::Config};
use std::fs::File;
use tokio::select;
use tracing::{info, level_filters::LevelFilter};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "A versatile SIP PBX server implemented in Rust"
)]
struct Cli {
    /// Path to the configuration file
    #[clap(long, help = "Path to the configuration file (TOML format)")]
    conf: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");
    sqlx::any::install_default_drivers();

    dotenv().ok();
    let cli = Cli::parse();

    let config = cli
        .conf
        .map(|conf| Config::load(&conf).expect("Failed to load config"))
        .unwrap_or_default();

    let mut env_filter = EnvFilter::from_default_env();
    if let Some(Ok(level)) = config
        .log_level
        .as_ref()
        .map(|level| level.parse::<LevelFilter>())
    {
        env_filter = env_filter.add_directive(level.into());
    }

    if let Some(ref log_file) = config.log_file {
        let file = File::create(log_file).expect("Failed to create log file");
        let (non_blocking, guard) = tracing_appender::non_blocking(file);
        std::mem::forget(guard);

        let file_layer = tracing_subscriber::fmt::layer()
            .with_file(true)
            .with_line_number(true)
            .with_target(true)
            .with_ansi(false)
            .with_writer(non_blocking);

        tracing_subscriber::registry()
            .with(env_filter)
            .with(file_layer)
            .init();
    } else {
        let fmt_layer = tracing_subscriber::fmt::layer()
            .with_file(true)
            .with_line_number(true)
            .with_target(true);

        tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt_layer)
            .init();
    }

    let state_builder = AppStateBuilder::new().config(config);
    let state = state_builder.build().await.expect("Failed to build app");

    info!("Starting rustpbx on {}", state.config.http_addr);
    select! {
        _ = rustpbx::app::run(state) => {}
        _ = tokio::signal::ctrl_c() => {
            info!("Received CTRL+C, shutting down");
        }
    }
    Ok(())
}
