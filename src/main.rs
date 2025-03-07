use anyhow::Result;
use clap::Parser;
use tracing::info;
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
    let cli = config::Cli::parse();
    let mut app_builder = app::AppBuilder::new();

    if let Some(conf) = cli.conf {
        let config = config::Config::load(&conf).expect("Failed to load config");
        app_builder = app_builder.config(config);
    }

    let app = app_builder.build().expect("Failed to build app");

    info!("Starting rustpbx on {}", app.config.http_addr);
    app.run().await
}
