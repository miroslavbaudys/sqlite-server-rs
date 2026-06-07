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
}

impl RequestHandler {
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            config,
            databases: HashMap::new(),
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
