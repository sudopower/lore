use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

const CONNECT_ATTEMPTS: u32 = 30;
const CONNECT_DELAY: Duration = Duration::from_secs(2);

/// Connects to Postgres with at most 10 connections. Retries 30 times, 2s apart (each
/// attempt waits up to 3s for a connection),
/// so a process that starts before the database is ready does not crash-loop.
pub async fn connect(url: &str) -> Result<PgPool> {
    let mut attempt = 1;
    loop {
        match PgPoolOptions::new()
            .max_connections(10)
            .acquire_timeout(Duration::from_secs(3))
            .connect(url)
            .await
        {
            Ok(pool) => return Ok(pool),
            Err(e) if attempt < CONNECT_ATTEMPTS => {
                tracing::warn!(
                    attempt,
                    max_attempts = CONNECT_ATTEMPTS,
                    error = %e,
                    "database connection failed, retrying"
                );
                attempt += 1;
                tokio::time::sleep(CONNECT_DELAY).await;
            }
            Err(e) => {
                return Err(e).context(format!(
                    "could not connect to the database after {CONNECT_ATTEMPTS} attempts"
                ));
            }
        }
    }
}

/// Applies the embedded migrations.
pub async fn migrate(pool: &PgPool) -> Result<()> {
    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .context("applying migrations")?;
    Ok(())
}
