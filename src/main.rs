use anyhow::Result;
use app::AppBuilder;
use clap::Parser;
use config::{Cli, Config};
use std::fs::File;
use tokio::select;
use tracing::{info, level_filters::LevelFilter};
mod app;
mod config;
mod console;
mod error;
mod handler;
mod media;
mod proxy;
mod useragent;

#[tokio::main]
async fn main() -> Result<()> {
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

    if let Some(ref log_file) = config.log_file {
        let file = File::create(log_file).expect("Failed to create log file");
        let (non_blocking, _guard) = tracing_appender::non_blocking(file);
        log_fmt.with_writer(non_blocking).try_init().ok();
    } else {
        log_fmt.try_init().ok();
    }

    let app_builder = AppBuilder::new().config(config);
    let app = app_builder.build().expect("Failed to build app");

    info!("Starting rustpbx on {}", app.config.http_addr);
    select! {
        _ = app.run() => {}
        _ = tokio::signal::ctrl_c() => {
            info!("Received CTRL+C, shutting down");
        }
    }
    Ok(())
}
