// This crate is entirely safe Rust; make any future `unsafe` a compile error.
#![forbid(unsafe_code)]

mod config;
mod connection;
mod handler;
mod server;

fn main() {
    // Parse CLI / config file. `Ok(None)` means --help/--version already printed.
    let config = match config::parse() {
        Ok(Some(config)) => config,
        Ok(None) => return,
        Err(err) => {
            eprintln!("Config error:\n\t{err}");
            std::process::exit(1);
        }
    };

    // A multi-threaded runtime with a fixed worker count is the analogue of the C++
    // io_context driven by a pool of `workers` threads.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(config.workers.max(1))
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    if let Err(err) = runtime.block_on(server::run(config)) {
        eprintln!("Server error: {err}");
        std::process::exit(1);
    }
}
