use anyhow::Result;
use clap::Parser;
use dotenv::dotenv;
use rustpbx::{app::AppStateBuilder, config::Config};
use std::fs::File;
use tokio::select;
use tracing::{info, level_filters::LevelFilter};

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
    dotenv().ok();
    let cli = Cli::parse();

    let config = cli
        .conf
        .map(|conf| Config::load(&conf).expect("Failed to load config"))
        .unwrap_or_default();

    let mut log_fmt = tracing_subscriber::fmt();
    if let Some(ref level) = config.log_level {
        if let Ok(lv) = level.as_str().parse::<LevelFilter>() {
            log_fmt = log_fmt.with_max_level(lv);
        }
    }
    log_fmt = log_fmt.with_file(true).with_line_number(true);
    if let Some(ref log_file) = config.log_file {
        let file = File::create(log_file).expect("Failed to create log file");
        let (non_blocking, guard) = tracing_appender::non_blocking(file);
        std::mem::forget(guard);
        log_fmt
            .with_ansi(false)
            .with_writer(non_blocking)
            .try_init()
            .ok();
    } else {
        log_fmt.try_init().ok();
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
