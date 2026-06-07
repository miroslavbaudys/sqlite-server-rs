use std::sync::Arc;

use tokio::net::TcpListener;

use crate::config::Config;
use crate::connection;

/// Accept connections until a shutdown signal arrives. Mirrors NetworkWorker/ListenSocket:
/// each accepted socket is handled by its own task, and SIGINT/SIGTERM stop the loop.
pub async fn run(config: Config) -> std::io::Result<()> {
    let config = Arc::new(config);
    let listener = TcpListener::bind(config.listen_addr).await?;

    println!(
        "sqlite-server listening on {} (workers: {}, databases: {}, max packet: {} bytes)",
        config.listen_addr,
        config.workers,
        config.databases_folder.display(),
        config.client_max_packet_size,
    );

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _peer)) => {
                        let config = Arc::clone(&config);
                        tokio::spawn(async move {
                            // Connection errors are normal (clients disconnect); drop quietly.
                            let _ = connection::handle(stream, config).await;
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
