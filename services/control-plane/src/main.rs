use std::{env, net::SocketAddr, path::PathBuf};

use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

use kratos_control_plane::database::DatabaseSettings;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("kratos_control_plane=info,tower_http=info")),
        )
        .init();

    let port = env::var("PORT")
        .unwrap_or_else(|_| "8080".to_owned())
        .parse::<u16>()?;
    let address = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = TcpListener::bind(address).await?;

    let web_root = env::var_os("KRATOS_WEB_ROOT").map(PathBuf::from);
    let database = match DatabaseSettings::from_environment()? {
        Some(settings) => Some(settings.connect().await?),
        None => None,
    };

    info!(%address, database_enabled = database.is_some(), "Kratos control plane listening");
    axum::serve(listener, kratos_control_plane::app(web_root, database))
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!(%error, "failed to install shutdown signal handler");
    }
}
