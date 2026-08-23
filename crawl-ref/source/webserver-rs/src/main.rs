use clap::Parser;
use tracing_subscriber::EnvFilter;

use webtiles_rs::config::{BindMode, CliOverrides, ServerConfig};
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

    // matches `bind_server_sockets`: resolve the actual (address, port)
    // pairs to listen on, for both plain HTTP and HTTPS - `bind_pairs`
    // config lets an operator bind several addresses at once (e.g. IPv4
    // + IPv6 separately); if unset, fall back to the single
    // bind_address/bind_port (or ssl_address/ssl_port) pair.
    let nonsecure_addrs = if config.bind_nonsecure != BindMode::Disabled {
        resolve_bind_addrs(&config.bind_pairs, &bind_address, bind_port).await?
    } else {
        Vec::new()
    };

    let tls_config = if let (Some(cert), Some(key)) = (&config.ssl_cert_file, &config.ssl_key_file) {
        // rustls 0.23 requires an explicit process-wide crypto provider
        // once more than one provider feature may be compiled in
        // (aws-lc-rs and ring both end up in the dependency tree here) -
        // otherwise `RustlsConfig`/`ServerConfig::builder()` panics.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        Some(axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key).await?)
    } else {
        None
    };
    let ssl_address = if config.ssl_address.is_empty() { "0.0.0.0".to_string() } else { config.ssl_address.clone() };
    let secure_addrs = if tls_config.is_some() {
        resolve_bind_addrs(&config.ssl_bind_pairs, &ssl_address, config.ssl_port).await?
    } else {
        Vec::new()
    };
    let bind_nonsecure = config.bind_nonsecure;

    if nonsecure_addrs.is_empty() && secure_addrs.is_empty() {
        anyhow::bail!(
            "no listening address configured: bind_nonsecure is disabled and \
             ssl_cert_file/ssl_key_file are not both set"
        );
    }
    if bind_nonsecure == BindMode::Redirect && secure_addrs.is_empty() {
        anyhow::bail!("bind_nonsecure=redirect requires ssl_cert_file/ssl_key_file to be configured");
    }

    let state = AppState::new(config, users);
    let games = state.games.clone();
    let router = webtiles_rs::http::build_router(state);

    // matches `https_port`: the (first) secure port, used to build the
    // Location header when non-secure traffic is redirected to HTTPS.
    let https_port_suffix = secure_addrs.first().map(|a| format!(":{}", a.port())).unwrap_or_default();
    let nonsecure_router = if bind_nonsecure == BindMode::Redirect {
        https_redirect_router(https_port_suffix)
    } else {
        router.clone()
    };

    let handle = axum_server::Handle::new();
    let mut server_tasks = Vec::new();

    for addr in nonsecure_addrs {
        tracing::info!(%addr, "listening on http");
        let router = nonsecure_router.clone();
        let handle = handle.clone();
        server_tasks.push(tokio::spawn(async move {
            axum_server::bind(addr).handle(handle).serve(router.into_make_service()).await
        }));
    }
    for addr in secure_addrs {
        tracing::info!(%addr, "listening on https");
        let router = router.clone();
        let handle = handle.clone();
        let tls_config = tls_config.clone().expect("secure_addrs is only non-empty when tls_config is Some");
        server_tasks.push(tokio::spawn(async move {
            axum_server::bind_rustls(addr, tls_config).handle(handle).serve(router.into_make_service()).await
        }));
    }

    tokio::spawn({
        let handle = handle.clone();
        async move {
            shutdown_signal().await;
            handle.graceful_shutdown(Some(std::time::Duration::from_secs(30)));
        }
    });

    for task in server_tasks {
        match task.await {
            Ok(Err(e)) => tracing::error!(error = %e, "listener task failed"),
            Err(e) => tracing::error!(error = %e, "listener task panicked"),
            Ok(Ok(())) => {}
        }
    }

    stop_running_games(&games).await;

    tracing::info!("Bye!");
    Ok(())
}

/// Resolve `pairs` (or, if empty, the single `default_host`/`default_port`)
/// to concrete socket addresses, matching `bind_server_sockets`'s
/// `bind_pairs`-or-single-pair fallback. Hostnames are resolved via async
/// DNS lookup; an empty host string means "all interfaces".
async fn resolve_bind_addrs(
    pairs: &[(String, u16)],
    default_host: &str,
    default_port: u16,
) -> anyhow::Result<Vec<std::net::SocketAddr>> {
    let pairs: Vec<(&str, u16)> = if pairs.is_empty() {
        vec![(default_host, default_port)]
    } else {
        pairs.iter().map(|(h, p)| (h.as_str(), *p)).collect()
    };

    let mut out = Vec::with_capacity(pairs.len());
    for (host, port) in pairs {
        let host = if host.is_empty() { "0.0.0.0" } else { host };
        let addr_str = format!("{host}:{port}");
        let addr = tokio::net::lookup_host(&addr_str)
            .await?
            .next()
            .ok_or_else(|| anyhow::anyhow!("could not resolve bind address {addr_str}"))?;
        out.push(addr);
    }
    Ok(out)
}

/// A router that redirects every request to the same host/path on the
/// (first) HTTPS port, matching `HTTPSRedirectHandler`/`bind_nonsecure:
/// redirect`.
fn https_redirect_router(https_port_suffix: String) -> axum::Router {
    axum::Router::new().fallback(
        move |headers: axum::http::HeaderMap, uri: axum::http::Uri| {
            let https_port_suffix = https_port_suffix.clone();
            async move {
                let host = headers
                    .get(axum::http::header::HOST)
                    .and_then(|v| v.to_str().ok())
                    .map(|h| h.rsplit_once(':').map(|(host, _port)| host).unwrap_or(h))
                    .unwrap_or("localhost");
                axum::response::Redirect::permanent(&format!("https://{host}{https_port_suffix}{uri}"))
            }
        },
    )
}

/// Matches `ws_handler.shutdown()`/`stop_everything`: ask every still-running
/// game to save and quit (`SIGHUP`), then wait for them to actually exit
/// before letting the process itself exit. Without this, `axum`'s graceful
/// shutdown only stops the HTTP/WebSocket layer - it has no idea any DCSS
/// child processes exist, so they'd be orphaned (left running, requiring a
/// manual `kill`) once this process exits, since they're on their own PTY
/// session and don't receive this process's SIGINT/SIGTERM.
async fn stop_running_games(games: &webtiles_rs::game::manager::GameManager) {
    let sessions = games.all_sessions().await;
    if sessions.is_empty() {
        return;
    }
    tracing::info!(count = sessions.len(), "stopping running games before exit");
    for session in &sessions {
        session.request_stop();
    }

    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
    const GRACE_PERIOD: std::time::Duration = std::time::Duration::from_secs(15);
    let deadline = tokio::time::Instant::now() + GRACE_PERIOD;
    while tokio::time::Instant::now() < deadline {
        if games.count().await == 0 {
            return;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    let remaining = games.all_sessions().await;
    if !remaining.is_empty() {
        tracing::warn!(count = remaining.len(), "games did not stop in time, killing forcibly");
        for session in &remaining {
            session.request_kill();
        }
        // give the forced kill a brief moment to land before giving up.
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
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
