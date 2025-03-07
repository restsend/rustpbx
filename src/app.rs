use crate::config::Config;
use anyhow::Result;

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
        Ok(())
    }
}
