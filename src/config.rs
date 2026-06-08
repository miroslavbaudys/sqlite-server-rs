use std::fmt;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};

use clap::Parser;
use serde_json::Value;

/// Default per-connection `busy_timeout` (ms). How long SQLite waits for a lock held by
/// another connection before returning SQLITE_BUSY — important when multiple clients
/// (e.g. a web server and Celery workers) write to the same database.
pub const DEFAULT_BUSY_TIMEOUT_MS: u64 = 5000;

/// Runtime configuration, resolved from either CLI flags or a JSON config file.
#[derive(Debug)]
pub struct Config {
    pub listen_addr: SocketAddr,
    pub workers: usize,
    pub databases_folder: PathBuf,
    pub client_max_packet_size: u32,
    pub busy_timeout_ms: u64,
    /// Optional password; when non-empty, every connection must authenticate before
    /// any command is processed. Empty disables authentication.
    pub auth: String,
    /// Optional list of allowed client networks. When empty, every peer is allowed.
    pub ip_whitelist: Vec<Cidr>,
}

impl Config {
    /// Returns true if `ip` is permitted to connect. An empty whitelist means the
    /// feature is disabled and every client is allowed.
    pub fn is_ip_allowed(&self, ip: IpAddr) -> bool {
        self.ip_whitelist.is_empty() || self.ip_whitelist.iter().any(|net| net.contains(ip))
    }

    /// Human-readable representation of the configured whitelist (for logging).
    pub fn ip_whitelist_repr(&self) -> String {
        if self.ip_whitelist.is_empty() {
            return "(any)".to_string();
        }
        self.ip_whitelist
            .iter()
            .map(Cidr::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// A parsed CIDR network (IPv4 or IPv6) for the IP whitelist. The stored address is
/// canonical (host bits zeroed), so it doubles as a clean display value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    network: IpAddr,
    prefix: u8,
}

impl Cidr {
    /// Parse a CIDR (`10.0.0.0/8`, `2001:db8::/32`) or a bare address (`127.0.0.1`,
    /// treated as `/32`; `::1` as `/128`).
    pub fn parse(entry: &str) -> Result<Self, String> {
        let invalid = || format!("Invalid ip_whitelist entry: {entry}");
        let (addr_str, prefix) = match entry.split_once('/') {
            Some((addr, prefix_str)) => {
                let prefix: u8 = prefix_str.parse().map_err(|_| invalid())?;
                (addr, Some(prefix))
            }
            None => (entry, None),
        };

        let addr: IpAddr = addr_str.parse().map_err(|_| invalid())?;
        let max_prefix = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = prefix.unwrap_or(max_prefix);
        if prefix > max_prefix {
            return Err(invalid());
        }

        Ok(Self {
            network: canonicalize(addr, prefix),
            prefix,
        })
    }

    /// True if `ip` falls inside this network (same family and matching prefix bits).
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.network, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                masked_eq(&net.octets(), &ip.octets(), self.prefix)
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                masked_eq(&net.octets(), &ip.octets(), self.prefix)
            }
            // Different address families never match.
            _ => false,
        }
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix)
    }
}

/// Compare the first `prefix` bits of two equal-length octet slices.
fn masked_eq(a: &[u8], b: &[u8], prefix: u8) -> bool {
    let full_bytes = (prefix / 8) as usize;
    if a[..full_bytes] != b[..full_bytes] {
        return false;
    }
    let remaining_bits = prefix % 8;
    if remaining_bits > 0 {
        let mask = 0xffu8 << (8 - remaining_bits);
        if (a[full_bytes] & mask) != (b[full_bytes] & mask) {
            return false;
        }
    }
    true
}

/// Zero the host bits of `addr` beyond `prefix`, yielding the canonical network address.
fn canonicalize(addr: IpAddr, prefix: u8) -> IpAddr {
    fn mask(octets: &mut [u8], prefix: u8) {
        let prefix = prefix as usize;
        for (i, byte) in octets.iter_mut().enumerate() {
            let bits = i * 8;
            if bits >= prefix {
                *byte = 0;
            } else if bits + 8 > prefix {
                *byte &= 0xffu8 << (bits + 8 - prefix);
            }
        }
    }
    match addr {
        IpAddr::V4(a) => {
            let mut o = a.octets();
            mask(&mut o, prefix);
            IpAddr::V4(o.into())
        }
        IpAddr::V6(a) => {
            let mut o = a.octets();
            mask(&mut o, prefix);
            IpAddr::V6(o.into())
        }
    }
}

/// Parse a comma-separated list of CIDR/address entries (CLI form), trimming whitespace
/// and skipping empties.
fn parse_whitelist_csv(s: &str) -> Result<Vec<Cidr>, String> {
    s.split(',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(Cidr::parse)
        .collect()
}

/// Parse the optional `ip_whitelist` JSON array (config-file form). Absent => empty list.
fn parse_whitelist_json(json: &Value) -> Result<Vec<Cidr>, String> {
    match json.get("ip_whitelist") {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                let s = item.as_str().ok_or_else(|| {
                    "Config key ip_whitelist must contain only strings".to_string()
                })?;
                Cidr::parse(s)
            })
            .collect(),
        Some(_) => Err("Config key ip_whitelist must be an array of CIDR strings".to_string()),
    }
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

    /// Require clients to authenticate with this password (empty disables auth)
    #[arg(short = 'a', long = "auth", default_value = "")]
    auth: String,

    /// Comma-separated IPs/CIDRs allowed to connect, e.g. 127.0.0.1,10.0.0.0/8
    /// (empty allows all)
    #[arg(long = "ip-whitelist", default_value = "")]
    ip_whitelist: String,

    /// Folder holding the database files (must exist)
    #[arg(short = 'd', long = "databases-folder", default_value = "sqlite")]
    databases_folder: String,

    /// Number of worker threads (defaults to the number of CPU cores)
    #[arg(short = 'w', long = "workers")]
    workers: Option<usize>,

    /// Max request size in bytes; larger requests close the connection
    #[arg(long = "client-max-packet-size", default_value_t = 16 * 1024 * 1024)]
    client_max_packet_size: u32,

    /// Per-connection SQLite busy_timeout in milliseconds (lock-wait before SQLITE_BUSY)
    #[arg(long = "busy-timeout", default_value_t = DEFAULT_BUSY_TIMEOUT_MS)]
    busy_timeout_ms: u64,
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
            busy_timeout_ms: args.busy_timeout_ms,
            auth: args.auth,
            ip_whitelist: parse_whitelist_csv(&args.ip_whitelist)?,
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

    // Optional so that config files written for the C++ server (which has no such key)
    // still load; falls back to the default when absent.
    let busy_timeout_ms = match json.get("busy_timeout_ms") {
        Some(v) => v
            .as_u64()
            .ok_or_else(|| "Key busy_timeout_ms must be a non-negative number".to_string())?,
        None => DEFAULT_BUSY_TIMEOUT_MS,
    };

    // Optional: absent => feature disabled. Keeps configs written for either server loading.
    let auth = match json.get("auth") {
        Some(v) => v
            .as_str()
            .ok_or_else(|| "Key auth must be a string".to_string())?
            .to_string(),
        None => String::new(),
    };
    let ip_whitelist = parse_whitelist_json(&json)?;

    Ok(Config {
        listen_addr: resolve_endpoint(&listen_ip, listen_port),
        workers: require_u64(&json, "workers")? as usize,
        databases_folder: resolve_database_path(&require_str(&json, "databases_folder")?)?,
        client_max_packet_size: require_u64(&json, "client_max_packet_size")? as u32,
        busy_timeout_ms,
        auth,
        ip_whitelist,
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
            r#"{{"client_max_packet_size":1024,"workers":3,"listen_ip":"127.0.0.1","listen_port":4567,"busy_timeout_ms":1234,"databases_folder":{}}}"#,
            serde_json::to_string(dir.to_str().unwrap()).unwrap()
        );
        let path = write_config(&dir, &body);

        let config = from_file(path.to_str().unwrap()).unwrap();
        assert_eq!(config.client_max_packet_size, 1024);
        assert_eq!(config.workers, 3);
        assert_eq!(config.listen_addr.port(), 4567);
        assert!(config.listen_addr.is_ipv4());
        assert!(config.databases_folder.is_absolute());
        assert_eq!(config.busy_timeout_ms, 1234);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cidr_parse_and_contains_ipv4() {
        let net = Cidr::parse("10.0.0.0/8").unwrap();
        assert!(net.contains("10.1.2.3".parse().unwrap()));
        assert!(!net.contains("11.0.0.1".parse().unwrap()));
        // Display is canonical (host bits zeroed).
        assert_eq!(Cidr::parse("10.1.2.3/8").unwrap().to_string(), "10.0.0.0/8");
    }

    #[test]
    fn cidr_bare_address_is_single_host() {
        let net = Cidr::parse("127.0.0.1").unwrap();
        assert_eq!(net.to_string(), "127.0.0.1/32");
        assert!(net.contains("127.0.0.1".parse().unwrap()));
        assert!(!net.contains("127.0.0.2".parse().unwrap()));
    }

    #[test]
    fn cidr_ipv6_and_cross_family() {
        let net = Cidr::parse("2001:db8::/32").unwrap();
        assert!(net.contains("2001:db8::1".parse().unwrap()));
        assert!(!net.contains("2001:dead::1".parse().unwrap()));
        // An IPv4 address never matches an IPv6 network (and vice versa).
        assert!(!net.contains("10.0.0.1".parse().unwrap()));
    }

    #[test]
    fn cidr_rejects_invalid() {
        assert!(Cidr::parse("not.an.ip/8").is_err());
        assert!(Cidr::parse("10.0.0.0/33").is_err());
        assert!(Cidr::parse("::1/129").is_err());
    }

    #[test]
    fn is_ip_allowed_empty_allows_all() {
        let dir = temp_dir();
        let body = format!(
            r#"{{"client_max_packet_size":1,"workers":1,"listen_ip":"127.0.0.1","listen_port":3333,"databases_folder":{}}}"#,
            serde_json::to_string(dir.to_str().unwrap()).unwrap()
        );
        let config = from_file(write_config(&dir, &body).to_str().unwrap()).unwrap();
        assert!(config.auth.is_empty());
        assert!(config.ip_whitelist.is_empty());
        assert!(config.is_ip_allowed("8.8.8.8".parse().unwrap()));
        assert_eq!(config.ip_whitelist_repr(), "(any)");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn from_file_parses_auth_and_whitelist() {
        let dir = temp_dir();
        let body = format!(
            r#"{{"client_max_packet_size":1,"workers":1,"listen_ip":"127.0.0.1","listen_port":3333,"auth":"s3cret","ip_whitelist":["127.0.0.1","10.0.0.0/8"],"databases_folder":{}}}"#,
            serde_json::to_string(dir.to_str().unwrap()).unwrap()
        );
        let config = from_file(write_config(&dir, &body).to_str().unwrap()).unwrap();
        assert_eq!(config.auth, "s3cret");
        assert!(config.is_ip_allowed("127.0.0.1".parse().unwrap()));
        assert!(config.is_ip_allowed("10.9.9.9".parse().unwrap()));
        assert!(!config.is_ip_allowed("192.168.0.1".parse().unwrap()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn from_file_rejects_bad_whitelist() {
        let dir = temp_dir();
        // Not an array.
        let body = format!(
            r#"{{"client_max_packet_size":1,"workers":1,"listen_ip":"127.0.0.1","listen_port":3333,"ip_whitelist":"127.0.0.1","databases_folder":{}}}"#,
            serde_json::to_string(dir.to_str().unwrap()).unwrap()
        );
        let err = from_file(write_config(&dir, &body).to_str().unwrap()).unwrap_err();
        assert!(err.contains("must be an array"), "got: {err}");

        // Invalid CIDR entry.
        let body = format!(
            r#"{{"client_max_packet_size":1,"workers":1,"listen_ip":"127.0.0.1","listen_port":3333,"ip_whitelist":["nope/8"],"databases_folder":{}}}"#,
            serde_json::to_string(dir.to_str().unwrap()).unwrap()
        );
        let err = from_file(write_config(&dir, &body).to_str().unwrap()).unwrap_err();
        assert!(err.contains("Invalid ip_whitelist entry"), "got: {err}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn from_file_busy_timeout_defaults_when_absent() {
        // A config written for the C++ server (no busy_timeout_ms key) must still load.
        let dir = temp_dir();
        let body = format!(
            r#"{{"client_max_packet_size":1024,"workers":1,"listen_ip":"127.0.0.1","listen_port":3333,"databases_folder":{}}}"#,
            serde_json::to_string(dir.to_str().unwrap()).unwrap()
        );
        let path = write_config(&dir, &body);

        let config = from_file(path.to_str().unwrap()).unwrap();
        assert_eq!(config.busy_timeout_ms, DEFAULT_BUSY_TIMEOUT_MS);

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
