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
    let directory = std::env::var_os("GOBROWSE_MIGRATIONS_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations"));
    let migrator = sqlx::migrate::Migrator::new(directory).await?;
    let mut connection = pool
        .acquire()
        .await
        .map_err(sqlx::migrate::MigrateError::Execute)?;
    sqlx::query("SELECT pg_advisory_lock(607629720)")
        .execute(&mut *connection)
        .await
        .map_err(sqlx::migrate::MigrateError::Execute)?;
    let result = migrator.run(&mut *connection).await;
    let unlock = sqlx::query("SELECT pg_advisory_unlock(607629720)")
        .execute(&mut *connection)
        .await;
    match result {
        Err(error) => Err(error),
        Ok(()) => unlock
            .map(|_| ())
            .map_err(sqlx::migrate::MigrateError::Execute),
    }
}

pub async fn ready(pool: &PgPool) -> Result<(), AppError> {
    sqlx::query("SELECT 1").execute(pool).await?;
    Ok(())
}
