# sqlite-server-rs

A Rust port of [sqlite-server](https://github.com/miroslavbaudys/sqlite-server) — a small, multi-threaded TCP server
that exposes [SQLite](https://sqlite.org) databases over a length-prefixed JSON protocol.
Clients open a socket, send a JSON request, and receive a JSON response. The protocol is
**wire-compatible** with the original C++ server, so existing clients (including the
reference Python client) work unchanged.

- **Transport:** raw TCP, one 4-byte little-endian length header + UTF-8 JSON body per message.
- **Commands:** `QUERY`, `LIST`, `DELETE_DB`.
- **Concurrency:** a Tokio multi-threaded runtime with a configurable worker count.
- **Storage:** one SQLite database file per name, kept in a configured folder.

---

## Building

The build needs only the Rust toolchain (`cargo`/`rustc`). SQLite is compiled from source
via `rusqlite`'s `bundled` feature, so there is no system SQLite dependency.

```sh
cargo build --release      # -> target/release/sqlite-server
```

| Dependency | Purpose |
|------------|---------|
| `tokio` | async runtime + worker-thread pool (analogue of Boost.Asio) |
| `serde_json` | JSON parsing / serialization |
| `rusqlite` (`bundled`) | SQLite engine, compiled from source |
| `clap` | CLI parsing |
| `regex` | lenient JSON key-repair fallback |

---

## Running

```sh
./target/release/sqlite-server --port 3333 --databases-folder ./data
```

### Command-line options

| Option | Default | Description |
|--------|---------|-------------|
| `-h, --help` | — | Show usage and exit |
| `-v, --version` | — | Show git branch + commit and exit |
| `-c, --config <path>` | — | Load settings from a JSON config file (see below) |
| `--ip <ip>` | `localhost` | Listen address |
| `-p, --port <port>` | `3333` | Listen port |
| `-d, --databases-folder <dir>` | `sqlite` | Folder holding the database files (must exist) |
| `-w, --workers <n>` | CPU cores | Number of worker threads |
| `--client-max-packet-size <bytes>` | `16777216` (16 MiB) | Max request size; larger requests close the connection |

### Config file

When `--config` is given, all settings come from the JSON file and the other flags are ignored:

```json
{
  "client_max_packet_size": 16777216,
  "workers": 4,
  "listen_ip": "127.0.0.1",
  "listen_port": 3333,
  "databases_folder": "./data"
}
```

The databases folder **must already exist** — the server will not create it (individual
database files inside it are created on demand). Shutdown is graceful on `SIGINT`/`SIGTERM`.

### Running as a service (systemd)

The server runs in the foreground, logs to stdout, and shuts down cleanly on `SIGTERM`
(the signal systemd sends by default), so it works well as a `Type=simple` service.

**1. Install the binary and create a dedicated user + data directory:**

```sh
cargo build --release
sudo install -m 0755 target/release/sqlite-server /usr/local/bin/sqlite-server
sudo useradd --system --no-create-home --shell /usr/sbin/nologin sqlite-server
sudo mkdir -p /var/lib/sqlite-server
sudo chown sqlite-server:sqlite-server /var/lib/sqlite-server
```

**2. Create `/etc/systemd/system/sqlite-server.service`:**

```ini
[Unit]
Description=sqlite-server-rs (SQLite over TCP/JSON)
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/sqlite-server --ip 127.0.0.1 --port 3333 --databases-folder /var/lib/sqlite-server
User=sqlite-server
Group=sqlite-server
Restart=on-failure
RestartSec=2

# Hardening — the protocol has no authentication or TLS, so keep it bound to
# localhost (or a trusted interface) and lock the process down.
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
PrivateDevices=true
ProtectControlGroups=true
ProtectKernelModules=true
ProtectKernelTunables=true
RestrictAddressFamilies=AF_INET AF_INET6
ReadWritePaths=/var/lib/sqlite-server

[Install]
WantedBy=multi-user.target
```

**3. Enable and start it:**

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now sqlite-server
```

**4. Check status and follow logs:**

```sh
systemctl status sqlite-server
journalctl -u sqlite-server -f
```

To use a config file instead of flags, point `ExecStart` at
`... --config /etc/sqlite-server/config.json` and add that path to `ReadOnlyPaths=`.

> **Tip:** on systemd ≥ 235 you can skip the manual `useradd`/`mkdir` by using
> `DynamicUser=yes` together with `StateDirectory=sqlite-server` (which creates and owns
> `/var/lib/sqlite-server` for you) and pointing `--databases-folder` at it.

---

## Communication protocol

### Framing

Every message — in both directions — is a 4-byte little-endian `uint32` length `N`,
followed by `N` bytes of UTF-8 JSON. Read the 4 bytes, decode the length, then read
exactly that many bytes.

### Connection lifecycle

A connection is **persistent**: requests on a single connection are processed
sequentially (send a request, read its full response, then send the next).

### Request format

A request is a JSON object with a `cmd` field (matched case-insensitively) plus
command-specific fields.

| Command | Required fields | Purpose |
|---------|-----------------|---------|
| `QUERY` | `db`, `query` | Run a single SQL statement against database `db` |
| `LIST` | — | List available database files |
| `DELETE_DB` | `db` | Delete the database file `db` |

> Tolerance: lightly-malformed JSON with unquoted keys (e.g. `{cmd:"LIST"}`) is repaired
> before parsing.

### `QUERY`

```json
{ "cmd": "QUERY", "db": "mydb", "query": "SELECT id, name FROM users WHERE id = 1" }
```

Success — `columns` is the column list in `SELECT` order, `data` is an array of row objects:

```json
{ "columns": ["id", "name"], "data": [ { "id": 1, "name": "Alice" } ] }
```

Statements with no result set (`INSERT`, `CREATE`, …) return `{ "columns": [], "data": [] }`.

#### Value encoding

| SQLite type | JSON representation |
|-------------|---------------------|
| INTEGER | number (64-bit) |
| FLOAT | number |
| TEXT | string |
| NULL | `null` |
| BLOB | string `"X'<hex>'"` (lowercase), e.g. `"X'00ff'"` |

Each row is a JSON object whose keys are serialized **alphabetically** — use the top-level
`columns` array when you need the original `SELECT` order.

### `LIST`

```json
{ "cmd": "LIST" }
```

Returns the regular files in the databases folder, excluding SQLite sidecar files
(`-wal`, `-shm`, `-journal`): `{ "list": ["mydb", "sales", "logs"] }`.

### `DELETE_DB`

```json
{ "cmd": "DELETE_DB", "db": "mydb" }
```

Response: `{ "result": "ok" }` if the file was removed, `{ "result": "error" }` otherwise.

### Database name rules

`db` must be a **plain file name inside the configured folder**. Path separators, absolute
paths, `.`/`..` traversal, and symlinks pointing outside the folder are rejected. A
non-existent database is created on first `QUERY`.

### Error responses

Command/validation errors carry a numeric `generic_error` code and echo the `request`:

| Code | Name | Meaning |
|------|------|---------|
| 0 | `INVALID_FORMAT` | Request body was not valid JSON (also includes a `message`) |
| 1 | `NO_COMMAND_SPECIFIED` | Missing `cmd` |
| 2 | `UNKNOWN_COMMAND` | `cmd` is not a supported command |
| 3 | `NO_DATABASE_SPECIFIED` | Missing `db`, or `db` is not a safe/valid name |
| 4 | `ERROR_READING_FROM_CLIENT` | Missing `query` on a `QUERY` request |

SQL errors carry the SQLite primary result code, its message, and the original request:

```json
{ "error_code": 1, "error_message": "no such table: ghosts",
  "query": { "cmd": "QUERY", "db": "mydb", "query": "SELECT * FROM ghosts" } }
```

An empty/whitespace query is reported as `error_code` 21 (`SQLITE_MISUSE`), `"empty query"`.

---

## Rust client

A reference client lives at [`examples/rust/client.rs`](examples/rust/client.rs). It
handles framing, client-side `?` / `?N` parameter binding with escaping, and typed
`QueryResult` / `Row` wrappers (including `X'..'` BLOB decoding). It depends only on
`serde_json` and `regex`. Run the demo against a live server:

```sh
# terminal 1
mkdir -p /tmp/sqlite-data
cargo run -- --databases-folder /tmp/sqlite-data

# terminal 2
cargo run --example client
```

Usage sketch:

```rust
let mut db = Sqlite::connect("mydb")?;                  // 127.0.0.1:3333 by default
db.query("CREATE TABLE IF NOT EXISTS users(id INTEGER, name TEXT)", &[])?;
db.query("INSERT INTO users VALUES (?, ?)", &[1.into(), "Alice".into()])?;  // ? is escaped

let result = db.query("SELECT id, name FROM users WHERE id = ?", &[1.into()])?;
for row in result.rows() {
    println!("{:?} {:?}", row.get_i64("id"), row.get_str("name"));
}

let n = db.query("SELECT COUNT(*) AS n FROM users", &[])?.scalar_i64().unwrap_or(0);
let dbs = db.list()?;                                   // LIST
```

Highlights:

- **Parameter binding** — `?` and `?N` placeholders escaped client-side via the `Param`
  enum (`i64`/`f64`/`&str`/`bool`/`Vec<u8>`/`Option<_>` all `.into()` it); `Vec<u8>`
  becomes an `X'..'` BLOB literal.
- **`QueryResult`** — `.rows()`, `.columns()` (true `SELECT` order), `.first()`,
  `.scalar()` / `.scalar_i64()`, `.len()`, and `.server_error()`.
- **`Row`** — typed accessors `get_i64` / `get_f64` / `get_str` / `get` / `is_null`, plus
  `blob("col")` to decode a BLOB column back to `Vec<u8>`.

## Python client

A ready-to-use, dependency-free client lives at
[`examples/python/sqlite.py`](examples/python/sqlite.py) (standard library only). It
handles framing, client-side `?` parameter binding with escaping, and a typed result
wrapper. Copy it into your project and use it directly:

```python
from sqlite import Sqlite

# Connects to 127.0.0.1:3333 by default (edit _SQLITE_IP / _SQLITE_PORT to change).
with Sqlite("mydb") as db:
    db.send_query("CREATE TABLE IF NOT EXISTS users(id INTEGER, name TEXT)")
    db.send_query("INSERT INTO users VALUES(?, ?)", [1, "Alice"])   # ? params are escaped

    result = db.query("SELECT id, name FROM users WHERE id = ?", [1])
    for row in result:
        print(row.id, row.name)          # rows support both row["id"] and row.id

    n = db.query("SELECT COUNT(*) AS n FROM users").scalar()   # first column of first row -> 1
```

Highlights:

- **Parameter binding** — `?` and `?N` placeholders are escaped client-side; `bytes`
  values become `X'..'` BLOB literals.
- **`QueryResult`** — iterable/sized/truthy; `.first()`, `.scalar()`, `.column(name)`,
  `.rows`, `.columns` (true `SELECT` order). Never `None`.
- **`Row`** — a `dict` subclass with attribute access (`row.name`) and `row.blob("col")`
  to decode a BLOB column back to `bytes`.

### Minimal raw protocol

If you would rather speak the protocol directly, the framing is just a 4-byte
little-endian length plus JSON:

```python
import json, socket, struct

def call(host, port, request):
    payload = json.dumps(request).encode("utf-8")
    with socket.create_connection((host, port)) as sock:
        sock.sendall(struct.pack("<I", len(payload)) + payload)   # 4-byte LE length + body

        def recv_exactly(n):
            buf = b""
            while len(buf) < n:
                chunk = sock.recv(n - len(buf))
                if not chunk:
                    raise ConnectionError("connection closed")
                buf += chunk
            return buf

        size = struct.unpack("<I", recv_exactly(4))[0]
        return json.loads(recv_exactly(size).decode("utf-8"))

print(call("127.0.0.1", 3333, {"cmd": "LIST"}))
print(call("127.0.0.1", 3333,
           {"cmd": "QUERY", "db": "mydb", "query": "SELECT 1 AS one"}))
```

---

## Project layout

| File | Responsibility |
|------|----------------|
| `src/main.rs` | CLI/config parsing, builds the runtime, starts the server |
| `src/config.rs` | Configuration from CLI flags or a JSON config file |
| `src/server.rs` | Accept loop + `SIGINT`/`SIGTERM` graceful shutdown |
| `src/connection.rs` | Per-connection framing (4-byte length + JSON) |
| `src/handler.rs` | Request parsing, command dispatch, response building, DB cache |
| `tests/protocol.rs` | End-to-end protocol tests against the built binary |
| `build.rs` | Embeds git branch/commit for `--version` |
| `examples/rust/client.rs` | Reference Rust client + runnable demo (`cargo run --example client`) |
| `examples/python/sqlite.py` | Reference Python client (standard library only) |

---

## License

Released under the MIT License.
