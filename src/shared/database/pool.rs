//! Database connection pool management

use sqlx::{PgPool, postgres::PgPoolOptions};
use std::time::Duration;

/// Database pool configuration
#[derive(Debug, Clone)]
pub struct PoolConfig {
    pub database_url: String,
    pub max_connections: u32,
    pub min_connections: u32,
    pub acquire_timeout: Duration,
    pub idle_timeout: Duration,
    pub max_lifetime: Duration,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            database_url: std::env::var("DATABASE_URL").unwrap_or_else(|_| {
                "postgresql://postgres:postgres@localhost:5432/thalmus".to_string()
            }),
            max_connections: 20,
            min_connections: 2,
            acquire_timeout: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(600),
            max_lifetime: Duration::from_secs(1800),
        }
    }
}

/// Create a database connection pool with the given configuration
pub async fn create_pool(config: &PoolConfig) -> crate::Result<PgPool> {
    tracing::info!(
        database_url = %config.database_url,
        max_connections = config.max_connections,
        "Creating database connection pool"
    );

    let pool = PgPoolOptions::new()
        .max_connections(config.max_connections)
        .min_connections(config.min_connections)
        .acquire_timeout(config.acquire_timeout)
        .idle_timeout(Some(config.idle_timeout))
        .max_lifetime(Some(config.max_lifetime))
        .connect(&config.database_url)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Failed to create database pool");
            e
        })?;

    tracing::info!("Database pool created successfully");

    Ok(pool)
}

/// Run database migrations
pub async fn run_migrations(pool: &PgPool) -> crate::Result<()> {
    tracing::info!("Running database migrations");

    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Failed to run migrations");
            e
        })?;

    tracing::info!("Database migrations completed successfully");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_pool_config_default() {
        let config = PoolConfig::default();
        assert!(config.database_url.contains("postgres"));
        assert_eq!(config.max_connections, 20);
        assert_eq!(config.min_connections, 2);
    }
}
