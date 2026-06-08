use std::collections::HashMap;
use std::path::{Component, Path};
use std::sync::{Arc, OnceLock};

use regex::Regex;
use rusqlite::types::ValueRef;
use rusqlite::Connection;
use serde_json::{json, Map, Value};

use crate::config::Config;

// Generic command/validation error codes — must match the C++ enum exactly.
const INVALID_FORMAT: i64 = 0;
const NO_COMMAND_SPECIFIED: i64 = 1;
const UNKNOWN_COMMAND: i64 = 2;
const NO_DATABASE_SPECIFIED: i64 = 3;
const ERROR_READING_FROM_CLIENT: i64 = 4;

// SQLite primary result code for misuse (e.g. an empty/comment-only query).
const SQLITE_MISUSE: i64 = 21;

/// Parses requests and dispatches commands. Holds a per-connection cache of open SQLite
/// connections keyed by database name, just like the C++ RequestHandler.
pub struct RequestHandler {
    config: Arc<Config>,
    databases: HashMap<String, Connection>,
    /// Whether this connection has authenticated (only relevant when `config.auth` is set).
    authenticated: bool,
}

impl RequestHandler {
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            config,
            databases: HashMap::new(),
            authenticated: false,
        }
    }

    pub fn handle_request(&mut self, req: &str) -> Value {
        let json = match parse_request(req) {
            Ok(value) => value,
            Err(message) => {
                return json!({
                    "generic_error": INVALID_FORMAT,
                    "message": message,
                    "request": req,
                });
            }
        };

        // When a password is configured, a connection must authenticate before any command
        // is processed. A `{"auth": "..."}` message authenticates the connection; anything
        // else from an unauthenticated client is rejected. Mirrors the C++ RequestHandler.
        if !self.config.auth.is_empty() {
            if let Some(auth) = json.get("auth").and_then(Value::as_str) {
                if auth == self.config.auth {
                    self.authenticated = true;
                    return json!({ "result": "ok" });
                }
            }
            if !self.authenticated {
                return json!({ "result": "error" });
            }
        }

        // Missing or non-string `cmd` -> no command. Top-level errors echo the raw request.
        let cmd = match json.get("cmd").and_then(Value::as_str) {
            Some(cmd) => cmd,
            None => return json!({ "generic_error": NO_COMMAND_SPECIFIED, "request": req }),
        };

        if cmd.eq_ignore_ascii_case("QUERY") {
            self.handle_query(&json)
        } else if cmd.eq_ignore_ascii_case("LIST") {
            self.handle_list()
        } else if cmd.eq_ignore_ascii_case("DELETE_DB") {
            self.handle_delete_db(&json)
        } else {
            json!({ "generic_error": UNKNOWN_COMMAND, "request": req })
        }
    }

    fn handle_query(&mut self, json: &Value) -> Value {
        let db = match json.get("db").and_then(Value::as_str) {
            Some(db) => db.to_string(),
            None => return json!({ "generic_error": NO_DATABASE_SPECIFIED, "request": json }),
        };

        let query = match json.get("query").and_then(Value::as_str) {
            Some(query) => query.to_string(),
            None => {
                return json!({ "generic_error": ERROR_READING_FROM_CLIENT, "request": json });
            }
        };

        if !self.is_safe_database_name(&db) {
            return json!({ "generic_error": NO_DATABASE_SPECIFIED, "request": json });
        }

        match self.run_query(&db, &query) {
            Ok(value) => value,
            Err((code, message)) => json!({
                "error_code": code,
                "error_message": message,
                "query": json,
            }),
        }
    }

    fn run_query(&mut self, db: &str, query: &str) -> Result<Value, (i64, String)> {
        // An empty or whitespace-only query compiles to no statement; report it the same
        // way the C++ server does (SQLITE_MISUSE / "empty query").
        if query.trim().is_empty() {
            return Err((SQLITE_MISUSE, "empty query".to_string()));
        }

        let conn = self.get_connection(db).map_err(map_sqlite_err)?;
        let mut stmt = conn.prepare(query).map_err(map_sqlite_err)?;

        // Column order as written in the SELECT (rows are emitted as objects whose keys
        // serialize alphabetically; this list preserves the original order).
        let columns: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();

        let mut rows_data: Vec<Value> = Vec::new();
        let mut rows = stmt.query([]).map_err(map_sqlite_err)?;
        while let Some(row) = rows.next().map_err(map_sqlite_err)? {
            let mut object = Map::new();
            for (i, name) in columns.iter().enumerate() {
                let value_ref = row.get_ref(i).map_err(map_sqlite_err)?;
                object.insert(name.clone(), value_ref_to_json(value_ref));
            }
            rows_data.push(Value::Object(object));
        }

        Ok(json!({ "columns": columns, "data": rows_data }))
    }

    fn handle_list(&self) -> Value {
        let mut list: Vec<Value> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.config.databases_folder) {
            for entry in entries.flatten() {
                // Follow symlinks (like is_regular_file) and keep only regular files.
                if std::fs::metadata(entry.path())
                    .map(|m| m.is_file())
                    .unwrap_or(false)
                {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if !is_sqlite_sidecar_file(&name) {
                        list.push(Value::String(name));
                    }
                }
            }
        }
        json!({ "list": list })
    }

    fn handle_delete_db(&mut self, json: &Value) -> Value {
        let db = match json.get("db").and_then(Value::as_str) {
            Some(db) => db.to_string(),
            None => return json!({ "generic_error": NO_DATABASE_SPECIFIED, "request": json }),
        };

        if !self.is_safe_database_name(&db) {
            return json!({ "generic_error": NO_DATABASE_SPECIFIED, "request": json });
        }

        let path = self.config.databases_folder.join(&db);
        let removed = std::fs::remove_file(&path).is_ok();
        if removed {
            self.databases.remove(&db);
        }

        json!({ "result": if removed { "ok" } else { "error" } })
    }

    /// Return the cached connection for `db`, opening (and caching) it on first use.
    /// The database file is created on demand inside the configured folder.
    fn get_connection(&mut self, db: &str) -> Result<&Connection, rusqlite::Error> {
        if !self.databases.contains_key(db) {
            let path = self.config.databases_folder.join(db);
            let conn = Connection::open(path)?;
            // Tune every connection for concurrent multi-client access (e.g. a web server
            // plus Celery workers hitting the same database):
            //   - WAL lets readers and a writer work concurrently (persists in the file).
            //   - busy_timeout makes a connection wait for a lock instead of immediately
            //     returning SQLITE_BUSY ("database is locked").
            //   - synchronous=NORMAL is the safe, faster companion to WAL.
            conn.execute_batch(&format!(
                "PRAGMA journal_mode=WAL; PRAGMA busy_timeout={}; PRAGMA synchronous=NORMAL;",
                self.config.busy_timeout_ms
            ))?;
            self.databases.insert(db.to_string(), conn);
        }
        Ok(self.databases.get(db).expect("just inserted"))
    }

    /// A database name is safe only if it is a single, non-traversing path component
    /// that resolves to a direct child of the databases folder. This rejects separators,
    /// absolute paths, "."/".." traversal, and symlinks that escape the folder.
    fn is_safe_database_name(&self, name: &str) -> bool {
        if name.is_empty() {
            return false;
        }

        let path = Path::new(name);
        if path.is_absolute() {
            return false;
        }

        // Require exactly one component, and that it be a normal name (not "."/".."/root).
        let mut components = path.components();
        let first = components.next();
        if components.next().is_some() {
            return false;
        }
        if !matches!(first, Some(Component::Normal(_))) {
            return false;
        }

        // Symlink-escape guard: if the target already exists, confirm it canonicalizes to
        // a direct child of the (canonicalized) databases folder.
        let full = self.config.databases_folder.join(name);
        if full.exists() {
            match (
                std::fs::canonicalize(&self.config.databases_folder),
                std::fs::canonicalize(&full),
            ) {
                (Ok(base), Ok(resolved)) => match resolved.strip_prefix(&base) {
                    Ok(rel) => rel.components().count() == 1,
                    Err(_) => false,
                },
                _ => false,
            }
        } else {
            true
        }
    }
}

/// SQLite keeps transient state in sidecar files (-wal, -shm, -journal); exclude them from LIST.
fn is_sqlite_sidecar_file(name: &str) -> bool {
    name.ends_with("-wal") || name.ends_with("-shm") || name.ends_with("-journal")
}

/// Encode a SQLite value as JSON, matching the C++ mapping:
/// INTEGER -> number, FLOAT -> number, TEXT -> string, NULL -> null,
/// BLOB -> "X'<lowercase-hex>'".
fn value_ref_to_json(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(n) => json!(n),
        ValueRef::Real(f) => match serde_json::Number::from_f64(f) {
            Some(number) => Value::Number(number),
            None => Value::Null, // non-finite floats have no JSON representation
        },
        ValueRef::Text(bytes) => Value::String(String::from_utf8_lossy(bytes).into_owned()),
        ValueRef::Blob(bytes) => Value::String(format!("X'{}'", to_hex(bytes))),
    }
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

/// Map a rusqlite error to (primary SQLite result code, message), matching what the C++
/// server reports via sqlite3_errcode / sqlite3_errmsg.
fn map_sqlite_err(err: rusqlite::Error) -> (i64, String) {
    match err {
        rusqlite::Error::SqliteFailure(ffi_err, message) => {
            let primary_code = (ffi_err.extended_code & 0xff) as i64;
            let message = message.unwrap_or_else(|| ffi_err.to_string());
            (primary_code, message)
        }
        // Non-SQLite errors (e.g. encoding issues) fall back to SQLITE_ERROR (1).
        other => (1, other.to_string()),
    }
}

/// Parse a request, repairing lightly-malformed JSON with unquoted keys
/// (e.g. SQLiteStudio's `{cmd:"LIST"}`) before a second parse attempt.
fn parse_request(req: &str) -> Result<Value, String> {
    if let Ok(value) = serde_json::from_str::<Value>(req) {
        return Ok(value);
    }

    let repaired = key_repair_regex().replace_all(req, "\"${2}\":");
    serde_json::from_str::<Value>(&repaired).map_err(|e| e.to_string())
}

fn key_repair_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(['"])?([a-zA-Z0-9]+)(['"])?:"#).unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ssrs-unit-{nanos}-{id}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn handler_in(dir: &Path) -> RequestHandler {
        let config = Config {
            listen_addr: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
            workers: 1,
            databases_folder: dir.to_path_buf(),
            client_max_packet_size: 16 * 1024 * 1024,
            busy_timeout_ms: 5000,
            auth: String::new(),
            ip_whitelist: Vec::new(),
        };
        RequestHandler::new(Arc::new(config))
    }

    #[test]
    fn hex_encoding_is_lowercase() {
        assert_eq!(to_hex(&[0x00, 0xff, 0x10, 0xab]), "00ff10ab");
        assert_eq!(to_hex(&[]), "");
    }

    #[test]
    fn sidecar_files_are_detected() {
        assert!(is_sqlite_sidecar_file("mydb-wal"));
        assert!(is_sqlite_sidecar_file("mydb-shm"));
        assert!(is_sqlite_sidecar_file("mydb-journal"));
        assert!(!is_sqlite_sidecar_file("mydb"));
        assert!(!is_sqlite_sidecar_file("mydb.db"));
    }

    #[test]
    fn parse_request_handles_valid_repair_and_invalid() {
        // Valid JSON.
        let v = parse_request(r#"{"cmd":"LIST"}"#).unwrap();
        assert_eq!(v["cmd"], json!("LIST"));

        // Unquoted keys are repaired (SQLiteStudio style).
        let v = parse_request(r#"{cmd:"LIST"}"#).unwrap();
        assert_eq!(v["cmd"], json!("LIST"));

        // Unrepairable garbage.
        assert!(parse_request("not json at all {{{").is_err());
    }

    #[test]
    fn blob_pointer_value_mapping() {
        // ValueRef -> JSON mapping for each SQLite type.
        assert_eq!(value_ref_to_json(ValueRef::Null), Value::Null);
        assert_eq!(value_ref_to_json(ValueRef::Integer(42)), json!(42));
        assert_eq!(value_ref_to_json(ValueRef::Real(2.5)), json!(2.5));
        assert_eq!(value_ref_to_json(ValueRef::Text(b"hi")), json!("hi"));
        assert_eq!(
            value_ref_to_json(ValueRef::Blob(&[0x00, 0xff])),
            json!("X'00ff'")
        );
        // Non-finite reals have no JSON representation -> null.
        assert_eq!(
            value_ref_to_json(ValueRef::Real(f64::INFINITY)),
            Value::Null
        );
    }

    #[test]
    fn database_name_safety() {
        let dir = temp_dir();
        let handler = handler_in(&dir);

        assert!(handler.is_safe_database_name("mydb.db"));
        assert!(handler.is_safe_database_name("sales"));

        assert!(!handler.is_safe_database_name(""));
        assert!(!handler.is_safe_database_name("."));
        assert!(!handler.is_safe_database_name(".."));
        assert!(!handler.is_safe_database_name("../escape"));
        assert!(!handler.is_safe_database_name("sub/dir"));
        assert!(!handler.is_safe_database_name("/etc/passwd"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn query_value_encoding_and_column_order() {
        let dir = temp_dir();
        let mut handler = handler_in(&dir);

        let created = handler.handle_request(
            r#"{"cmd":"QUERY","db":"t.db","query":"CREATE TABLE x(a INTEGER, b TEXT, c REAL, d BLOB)"}"#,
        );
        assert_eq!(created, json!({"columns": [], "data": []}));

        handler.handle_request(
            r#"{"cmd":"QUERY","db":"t.db","query":"INSERT INTO x VALUES (1, 'hi', 2.5, X'00ff')"}"#,
        );
        handler.handle_request(
            r#"{"cmd":"QUERY","db":"t.db","query":"INSERT INTO x VALUES (2, NULL, NULL, NULL)"}"#,
        );

        // Columns preserve SELECT order even when not alphabetical.
        let res = handler.handle_request(
            r#"{"cmd":"QUERY","db":"t.db","query":"SELECT d, c, b, a FROM x ORDER BY a"}"#,
        );
        assert_eq!(res["columns"], json!(["d", "c", "b", "a"]));

        let row0 = &res["data"][0];
        assert_eq!(row0["a"], json!(1));
        assert_eq!(row0["b"], json!("hi"));
        assert_eq!(row0["c"], json!(2.5));
        assert_eq!(row0["d"], json!("X'00ff'"));
        assert_eq!(res["data"][1]["b"], Value::Null);

        // Row object keys must serialize alphabetically (wire-compat with nlohmann::json).
        let serialized = serde_json::to_string(row0).unwrap();
        let pos_a = serialized.find("\"a\"").unwrap();
        let pos_b = serialized.find("\"b\"").unwrap();
        let pos_c = serialized.find("\"c\"").unwrap();
        let pos_d = serialized.find("\"d\"").unwrap();
        assert!(pos_a < pos_b && pos_b < pos_c && pos_c < pos_d);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn list_and_delete_db() {
        let dir = temp_dir();
        let mut handler = handler_in(&dir);

        handler
            .handle_request(r#"{"cmd":"QUERY","db":"a.db","query":"CREATE TABLE z(i INTEGER)"}"#);

        let list = handler.handle_request(r#"{"cmd":"LIST"}"#);
        let names: Vec<&str> = list["list"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(names.contains(&"a.db"));
        assert!(!names.iter().any(|n| n.ends_with("-wal")));

        assert_eq!(
            handler.handle_request(r#"{"cmd":"DELETE_DB","db":"a.db"}"#),
            json!({"result": "ok"})
        );
        // Already gone.
        assert_eq!(
            handler.handle_request(r#"{"cmd":"DELETE_DB","db":"a.db"}"#),
            json!({"result": "error"})
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn command_is_case_insensitive() {
        let dir = temp_dir();
        let mut handler = handler_in(&dir);
        assert!(handler
            .handle_request(r#"{"cmd":"list"}"#)
            .get("list")
            .is_some());
        assert!(handler
            .handle_request(r#"{"cmd":"LiSt"}"#)
            .get("list")
            .is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn generic_and_sql_error_codes() {
        let dir = temp_dir();
        let mut handler = handler_in(&dir);

        // INVALID_FORMAT (0) with a message.
        let r = handler.handle_request("garbage {{{");
        assert_eq!(r["generic_error"], json!(0));
        assert!(r.get("message").is_some());

        // NO_COMMAND_SPECIFIED (1).
        assert_eq!(
            handler.handle_request(r#"{"db":"a.db"}"#)["generic_error"],
            json!(1)
        );
        // UNKNOWN_COMMAND (2).
        assert_eq!(
            handler.handle_request(r#"{"cmd":"FROB"}"#)["generic_error"],
            json!(2)
        );
        // NO_DATABASE_SPECIFIED (3) — missing db.
        assert_eq!(
            handler.handle_request(r#"{"cmd":"QUERY","query":"SELECT 1"}"#)["generic_error"],
            json!(3)
        );
        // NO_DATABASE_SPECIFIED (3) — path traversal.
        assert_eq!(
            handler.handle_request(r#"{"cmd":"QUERY","db":"../x","query":"SELECT 1"}"#)
                ["generic_error"],
            json!(3)
        );
        // ERROR_READING_FROM_CLIENT (4) — missing query.
        assert_eq!(
            handler.handle_request(r#"{"cmd":"QUERY","db":"a.db"}"#)["generic_error"],
            json!(4)
        );

        // Empty query -> SQLITE_MISUSE (21) "empty query".
        let r = handler.handle_request(r#"{"cmd":"QUERY","db":"a.db","query":"   "}"#);
        assert_eq!(r["error_code"], json!(21));
        assert_eq!(r["error_message"], json!("empty query"));

        // SQL error carries the primary code and message.
        let r =
            handler.handle_request(r#"{"cmd":"QUERY","db":"a.db","query":"SELECT * FROM ghosts"}"#);
        assert_eq!(r["error_code"], json!(1));
        assert!(r["error_message"]
            .as_str()
            .unwrap()
            .contains("no such table"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn connections_use_wal_and_busy_timeout() {
        let dir = temp_dir();
        let mut handler = handler_in(&dir);

        // Opening the db applies the pragmas; verify they took effect.
        handler
            .handle_request(r#"{"cmd":"QUERY","db":"p.db","query":"CREATE TABLE t(i INTEGER)"}"#);

        let mode =
            handler.handle_request(r#"{"cmd":"QUERY","db":"p.db","query":"PRAGMA journal_mode"}"#);
        assert_eq!(mode["data"][0]["journal_mode"], json!("wal"));

        let timeout =
            handler.handle_request(r#"{"cmd":"QUERY","db":"p.db","query":"PRAGMA busy_timeout"}"#);
        assert_eq!(timeout["data"][0]["timeout"], json!(5000));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn auth_gate_requires_handshake() {
        let dir = temp_dir();
        let config = Config {
            listen_addr: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
            workers: 1,
            databases_folder: dir.to_path_buf(),
            client_max_packet_size: 16 * 1024 * 1024,
            busy_timeout_ms: 5000,
            auth: "s3cret".to_string(),
            ip_whitelist: Vec::new(),
        };
        let mut handler = RequestHandler::new(Arc::new(config));

        // A command before authenticating is rejected.
        assert_eq!(
            handler.handle_request(r#"{"cmd":"LIST"}"#),
            json!({"result": "error"})
        );
        // Wrong password is rejected and does not authenticate.
        assert_eq!(
            handler.handle_request(r#"{"auth":"nope"}"#),
            json!({"result": "error"})
        );
        assert_eq!(
            handler.handle_request(r#"{"cmd":"LIST"}"#),
            json!({"result": "error"})
        );
        // Correct password authenticates the connection.
        assert_eq!(
            handler.handle_request(r#"{"auth":"s3cret"}"#),
            json!({"result": "ok"})
        );
        // Now commands work.
        assert!(handler
            .handle_request(r#"{"cmd":"LIST"}"#)
            .get("list")
            .is_some());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn connection_cache_persists_across_requests() {
        let dir = temp_dir();
        let mut handler = handler_in(&dir);

        // A temp table lives only for the lifetime of a single connection; seeing it on a
        // later request proves the same cached connection is reused.
        handler.handle_request(
            r#"{"cmd":"QUERY","db":"c.db","query":"CREATE TEMP TABLE tmp(i INTEGER)"}"#,
        );
        handler
            .handle_request(r#"{"cmd":"QUERY","db":"c.db","query":"INSERT INTO tmp VALUES (7)"}"#);
        let res =
            handler.handle_request(r#"{"cmd":"QUERY","db":"c.db","query":"SELECT i FROM tmp"}"#);
        assert_eq!(res["data"][0]["i"], json!(7));

        std::fs::remove_dir_all(&dir).ok();
    }
}
