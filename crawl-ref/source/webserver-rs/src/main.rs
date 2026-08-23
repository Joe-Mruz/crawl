use clap::Parser;
use tracing_subscriber::EnvFilter;

use webtiles_rs::config::{CliOverrides, ServerConfig};
use webtiles_rs::state::AppState;
use webtiles_rs::userdb::UserDb;

#[derive(Parser)]
#[command(about = "Dungeon Crawl WebTiles server (Rust)")]
struct Args {
    /// Directory containing config.yml/games.d. Defaults to the
    /// `webserver/` directory next to this binary's `webserver-rs/`
    /// checkout (i.e. resolved relative to the executable, matching
    /// Python's `os.path.dirname(os.path.abspath(__file__))` - NOT the
    /// current working directory).
    #[arg(long)]
    server_path: Option<std::path::PathBuf>,

    #[command(flatten)]
    overrides: CliOverrides,
}

/// `<dir containing this binary>/../../../webserver`, i.e. sibling of the
/// `webserver-rs` checkout this was built from (`target/{debug,release}/`
/// is always two levels below `webserver-rs/`). Falls back to `./webserver`
/// (relative to the CWD) if the executable's path can't be determined.
fn default_server_path() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent()?.parent()?.parent()?.parent().map(|p| p.join("webserver")))
        .unwrap_or_else(|| std::path::PathBuf::from("webserver"))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let args = Args::parse();
    let server_path = args.server_path.clone().unwrap_or_else(default_server_path);

    let mut config = ServerConfig::load(&server_path)?;
    config.apply_cli_overrides(&args.overrides);
    tracing::info!(server_path = %server_path.display(), games = config.games.len(), "configuration loaded");
    if config.games.is_empty() {
        tracing::warn!(
            server_path = %server_path.display(),
            "no games configured (checked config.yml/games.d under server_path) - \
             the client will show no Play options; pass --server-path if this looks wrong"
        );
    }

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
