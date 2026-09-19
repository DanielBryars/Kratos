use std::{env, time::Duration};

use kratos_control_plane::{database::DatabaseSettings, migration::grant_application_privileges};
use sqlx::PgPool;
use tokio::time::sleep;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let settings = DatabaseSettings::from_environment()?
        .ok_or("database environment variables are required for migrations")?;
    let application_user = env::var("KRATOS_APPLICATION_DATABASE_USER")
        .map_err(|_| "KRATOS_APPLICATION_DATABASE_USER is required for migrations")?;
    let pool = connect_with_retry(&settings).await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    grant_application_privileges(&pool, &application_user).await?;
    Ok(())
}

async fn connect_with_retry(settings: &DatabaseSettings) -> Result<PgPool, sqlx::Error> {
    const ATTEMPTS: usize = 10;
    for attempt in 1..=ATTEMPTS {
        match settings.connect().await {
            Ok(pool) => return Ok(pool),
            Err(error) if attempt == ATTEMPTS => return Err(error),
            Err(_) => sleep(Duration::from_secs(1)).await,
        }
    }
    unreachable!("the bounded connection loop always returns")
}
