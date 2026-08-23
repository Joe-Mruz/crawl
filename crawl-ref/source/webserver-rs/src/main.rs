use clap::Parser;
use tracing_subscriber::EnvFilter;

use webtiles_rs::config::{CliOverrides, ServerConfig};
use webtiles_rs::state::AppState;
use webtiles_rs::userdb::UserDb;

#[derive(Parser)]
#[command(about = "Dungeon Crawl WebTiles server (Rust)")]
struct Args {
    /// Directory containing config.yml/games.d (defaults to ../webserver,
    /// matching the Python server's default layout so both can share one
    /// configuration during migration).
    #[arg(long, default_value = "../webserver")]
    server_path: std::path::PathBuf,

    #[command(flatten)]
    overrides: CliOverrides,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let args = Args::parse();

    let mut config = ServerConfig::load(&args.server_path)?;
    config.apply_cli_overrides(&args.overrides);

    let users = UserDb::open(&config.password_db, &config.settings_db)?;
    let bind_address = if config.bind_address.is_empty() {
        "0.0.0.0".to_string()
    } else {
        config.bind_address.clone()
    };
    let bind_port = config.bind_port;

    let state = AppState::new(config, users);
    let router = webtiles_rs::http::build_router(state);

    let addr = format!("{bind_address}:{bind_port}");
    tracing::info!(%addr, "DCSS WebTiles server (Rust) starting");
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    tracing::info!("Bye!");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("Received shutdown signal, beginning shutdown.");
}
