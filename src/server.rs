use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::sync::Semaphore;

use crate::config::Config;
use crate::connection;

/// Accept connections until a shutdown signal arrives. Mirrors NetworkWorker/ListenSocket:
/// each accepted socket is handled by its own task, and SIGINT/SIGTERM stop the loop.
pub async fn run(config: Config) -> std::io::Result<()> {
    let config = Arc::new(config);
    // Cap concurrent (blocking) SQLite execution to `workers`, so the process does not
    // spawn an unbounded number of pool threads under load. Shared across all connections.
    let query_limiter = Arc::new(Semaphore::new(config.workers.max(1)));
    let listener = TcpListener::bind(config.listen_addr).await?;
    // Report the actually-bound address: with port 0 the OS assigns an ephemeral port,
    // so config.listen_addr would not reflect the real one.
    let local_addr = listener.local_addr()?;

    println!(
        "sqlite-server listening on {} (workers: {}, databases: {}, max packet: {} bytes, auth: {}, ip whitelist: {})",
        local_addr,
        config.workers,
        config.databases_folder.display(),
        config.client_max_packet_size,
        if config.auth.is_empty() { "disabled" } else { "enabled" },
        config.ip_whitelist_repr(),
    );

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, peer)) => {
                        // Reject peers outside the configured whitelist before building any
                        // per-connection state. Dropping `stream` here closes the socket.
                        if !config.is_ip_allowed(peer.ip()) {
                            eprintln!("Connection rejected, not in ip whitelist: {}", peer.ip());
                            continue;
                        }
                        let config = Arc::clone(&config);
                        let query_limiter = Arc::clone(&query_limiter);
                        tokio::spawn(async move {
                            // Connection errors are normal (clients disconnect); drop quietly.
                            let _ = connection::handle(stream, config, query_limiter).await;
                        });
                    }
                    Err(err) => eprintln!("Accept error: {err}"),
                }
            }
            _ = &mut shutdown => {
                println!("Server shutdown...");
                break;
            }
        }
    }

    Ok(())
}

/// Resolve when the process receives SIGINT or SIGTERM (Ctrl-C on non-unix).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = sigint.recv() => {}
            _ = sigterm.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
