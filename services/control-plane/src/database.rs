use std::{env, time::Duration};

use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};

const REQUIRED_SETTINGS: [&str; 4] = [
    "KRATOS_DATABASE_HOST",
    "KRATOS_DATABASE_PORT",
    "KRATOS_DATABASE_NAME",
    "KRATOS_DATABASE_USER",
];

#[derive(Debug, thiserror::Error)]
pub enum DatabaseConfigError {
    #[error("database configuration is incomplete; missing {0}")]
    Missing(&'static str),
    #[error("KRATOS_DATABASE_PORT is not a valid TCP port")]
    InvalidPort,
}

#[derive(Debug)]
pub struct DatabaseSettings {
    host: String,
    port: u16,
    database: String,
    username: String,
}

impl DatabaseSettings {
    /// Reads the complete database configuration, or returns `None` when it is wholly absent.
    ///
    /// # Errors
    ///
    /// Returns an error when configuration is partial or the port is invalid.
    pub fn from_environment() -> Result<Option<Self>, DatabaseConfigError> {
        let present = REQUIRED_SETTINGS.map(|name| env::var(name).ok());
        if present.iter().all(Option::is_none) {
            return Ok(None);
        }
        let value = |index: usize| {
            present[index]
                .clone()
                .filter(|item| !item.is_empty())
                .ok_or(DatabaseConfigError::Missing(REQUIRED_SETTINGS[index]))
        };
        Ok(Some(Self {
            host: value(0)?,
            port: value(1)?
                .parse()
                .map_err(|_| DatabaseConfigError::InvalidPort)?,
            database: value(2)?,
            username: value(3)?,
        }))
    }

    /// Opens the bounded application connection pool.
    ///
    /// # Errors
    ///
    /// Returns the underlying `SQLx` error when `PostgreSQL` cannot be reached or authenticated.
    pub async fn connect(&self) -> Result<PgPool, sqlx::Error> {
        let options = PgConnectOptions::new()
            .host(&self.host)
            .port(self.port)
            .database(&self.database)
            .username(&self.username);
        PgPoolOptions::new()
            .min_connections(0)
            .max_connections(5)
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(options)
            .await
    }
}
