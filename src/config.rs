use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};

use clap::Parser;
use serde_json::Value;

/// Runtime configuration, resolved from either CLI flags or a JSON config file.
#[derive(Debug)]
pub struct Config {
    pub listen_addr: SocketAddr,
    pub workers: usize,
    pub databases_folder: PathBuf,
    pub client_max_packet_size: u32,
}

#[derive(Parser, Debug)]
#[command(
    name = "sqlite-server",
    about = "sqlite-server (SQLite over TCP/JSON)",
    // We print git branch/commit ourselves on -v, like the C++ server, so disable
    // clap's built-in -V/--version handling.
    disable_version_flag = true
)]
struct Args {
    /// Display the version (git branch + commit) and exit
    #[arg(short = 'v', long = "version")]
    version: bool,

    /// Load all settings from a JSON config file (other flags are then ignored)
    #[arg(short = 'c', long = "config")]
    config: Option<String>,

    /// Listen IP
    #[arg(long = "ip", default_value = "localhost")]
    ip: String,

    /// Listen port
    #[arg(short = 'p', long = "port", default_value_t = 3333)]
    port: u16,

    /// Folder holding the database files (must exist)
    #[arg(short = 'd', long = "databases-folder", default_value = "sqlite")]
    databases_folder: String,

    /// Number of worker threads (defaults to the number of CPU cores)
    #[arg(short = 'w', long = "workers")]
    workers: Option<usize>,

    /// Max request size in bytes; larger requests close the connection
    #[arg(long = "client-max-packet-size", default_value_t = 16 * 1024 * 1024)]
    client_max_packet_size: u32,
}

/// Parse configuration. Returns `Ok(None)` when --version was handled (nothing to run).
pub fn parse() -> Result<Option<Config>, String> {
    let args = Args::parse();

    if args.version {
        println!(
            "GIT Branch: {}\nGIT Commit hash: {}",
            env!("GIT_BRANCH"),
            env!("GIT_COMMIT_HASH")
        );
        return Ok(None);
    }

    // When --config is given, every setting comes from the file (CLI flags ignored),
    // matching the C++ behaviour.
    let config = match args.config {
        Some(path) => from_file(&path)?,
        None => Config {
            listen_addr: resolve_endpoint(&args.ip, args.port),
            workers: args.workers.unwrap_or_else(default_workers),
            databases_folder: resolve_database_path(&args.databases_folder)?,
            client_max_packet_size: args.client_max_packet_size,
        },
    };

    Ok(Some(config))
}

fn from_file(path: &str) -> Result<Config, String> {
    if !Path::new(path).exists() {
        return Err(format!("Config file does not exist in: {path}"));
    }

    let text = std::fs::read_to_string(path).map_err(|e| format!("Config read error: {e}"))?;
    let json: Value =
        serde_json::from_str(&text).map_err(|e| format!("Config parse error: {e}"))?;

    let listen_ip = require_str(&json, "listen_ip")?;
    let listen_port = require_u64(&json, "listen_port")? as u16;

    Ok(Config {
        listen_addr: resolve_endpoint(&listen_ip, listen_port),
        workers: require_u64(&json, "workers")? as usize,
        databases_folder: resolve_database_path(&require_str(&json, "databases_folder")?)?,
        client_max_packet_size: require_u64(&json, "client_max_packet_size")? as u32,
    })
}

fn require_str(json: &Value, key: &str) -> Result<String, String> {
    json.get(key)
        .ok_or_else(|| format!("Missing key: {key} in config"))?
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("Key {key} must be a string"))
}

fn require_u64(json: &Value, key: &str) -> Result<u64, String> {
    json.get(key)
        .ok_or_else(|| format!("Missing key: {key} in config"))?
        .as_u64()
        .ok_or_else(|| format!("Key {key} must be a non-negative number"))
}

/// Resolve `ip:port` to a socket address, preferring IPv4 (matching the C++ resolver),
/// falling back to 127.0.0.1:3333 if resolution fails.
fn resolve_endpoint(ip: &str, port: u16) -> SocketAddr {
    let fallback = SocketAddr::from(([127, 0, 0, 1], 3333));
    match (ip, port).to_socket_addrs() {
        Ok(addrs) => {
            let addrs: Vec<SocketAddr> = addrs.collect();
            addrs
                .iter()
                .find(|a| a.is_ipv4())
                .or_else(|| addrs.first())
                .copied()
                .unwrap_or(fallback)
        }
        Err(_) => fallback,
    }
}

/// Make the databases folder absolute and confirm it exists. The folder is never
/// created by the server (individual database files inside it are created on demand).
fn resolve_database_path(path: &str) -> Result<PathBuf, String> {
    let p = Path::new(path);
    let absolute = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| format!("Cannot resolve current directory: {e}"))?
            .join(p)
    };

    if !absolute.exists() {
        return Err(format!(
            "Databases folder does not exist in: {}",
            absolute.display()
        ));
    }
    Ok(absolute)
}

fn default_workers() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ssrs-config-{nanos}-{id}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_config(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("config.json");
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn resolve_endpoint_prefers_ipv4_numeric() {
        let addr = resolve_endpoint("127.0.0.1", 3333);
        assert!(addr.is_ipv4());
        assert_eq!(addr.port(), 3333);
    }

    #[test]
    fn resolve_endpoint_falls_back_when_unresolvable() {
        // A host containing an interior NUL fails locally in CString::new before any DNS
        // lookup, so this is deterministic and offline (a bogus hostname can't be used:
        // some resolvers hijack unknown names instead of failing).
        let addr = resolve_endpoint("bad\0host", 1234);
        assert_eq!(addr, SocketAddr::from(([127, 0, 0, 1], 3333)));
    }

    #[test]
    fn resolve_database_path_makes_absolute_when_existing() {
        let dir = temp_dir();
        let resolved = resolve_database_path(dir.to_str().unwrap()).unwrap();
        assert!(resolved.is_absolute());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_database_path_errors_when_missing() {
        let err = resolve_database_path("/no/such/folder/ssrs-xyz").unwrap_err();
        assert!(err.contains("does not exist"), "got: {err}");
    }

    #[test]
    fn from_file_parses_all_keys() {
        let dir = temp_dir();
        let body = format!(
            r#"{{"client_max_packet_size":1024,"workers":3,"listen_ip":"127.0.0.1","listen_port":4567,"databases_folder":{}}}"#,
            serde_json::to_string(dir.to_str().unwrap()).unwrap()
        );
        let path = write_config(&dir, &body);

        let config = from_file(path.to_str().unwrap()).unwrap();
        assert_eq!(config.client_max_packet_size, 1024);
        assert_eq!(config.workers, 3);
        assert_eq!(config.listen_addr.port(), 4567);
        assert!(config.listen_addr.is_ipv4());
        assert!(config.databases_folder.is_absolute());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn from_file_missing_file() {
        let err = from_file("/no/such/path/ssrs-config.json").unwrap_err();
        assert!(err.contains("Config file does not exist"), "got: {err}");
    }

    #[test]
    fn from_file_invalid_json() {
        let dir = temp_dir();
        let path = write_config(&dir, "{ this is not json");
        let err = from_file(path.to_str().unwrap()).unwrap_err();
        assert!(err.contains("Config parse error"), "got: {err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn from_file_missing_key() {
        let dir = temp_dir();
        let path = write_config(&dir, r#"{"workers":1}"#);
        let err = from_file(path.to_str().unwrap()).unwrap_err();
        assert!(err.contains("Missing key"), "got: {err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn from_file_wrong_value_type() {
        let dir = temp_dir();
        // workers as a string instead of a number.
        let body = format!(
            r#"{{"client_max_packet_size":1024,"workers":"three","listen_ip":"127.0.0.1","listen_port":4567,"databases_folder":{}}}"#,
            serde_json::to_string(dir.to_str().unwrap()).unwrap()
        );
        let path = write_config(&dir, &body);
        let err = from_file(path.to_str().unwrap()).unwrap_err();
        assert!(err.contains("must be a non-negative number"), "got: {err}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
