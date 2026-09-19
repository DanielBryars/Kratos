use kratos_control_plane::database::DatabaseSettings;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let settings = DatabaseSettings::from_environment()?
        .ok_or("database environment variables are required for migrations")?;
    let pool = settings.connect().await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(())
}
