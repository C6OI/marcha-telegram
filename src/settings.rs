use config::{Config, ConfigError};
use serde::Deserialize;
use url::Url;

#[allow(unused)]
#[derive(Debug, Deserialize, Clone)]
pub struct Settings {
    pub telegram: TelegramSettings,
    pub server: ServerSettings,
    pub database: DatabaseSettings,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TelegramSettings {
    pub token: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServerSettings {
    pub url: Url,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DatabaseSettings {
    pub pool: deadpool_postgres::Config,
}

impl Settings {
    pub fn parse() -> Result<Self, ConfigError> {
        let settings = Config::builder()
            .add_source(config::Environment::default())
            .add_source(config::File::with_name("config/settings"))
            .add_source(config::File::with_name("config/local").required(false))
            .build()?;

        settings.try_deserialize()
    }
}
