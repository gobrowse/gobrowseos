use secrecy::ExposeSecret;
use sqlx::{PgPool, postgres::PgPoolOptions};

use crate::{config::DatabaseSettings, error::AppError};

pub async fn connect(settings: &DatabaseSettings) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(settings.max_connections)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect(settings.url.expose_secret())
        .await
}

pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!().run(pool).await
}

pub async fn ready(pool: &PgPool) -> Result<(), AppError> {
    sqlx::query("SELECT 1").execute(pool).await?;
    Ok(())
}
