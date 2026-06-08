# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.1] - 2026-06-08

### Changed
- Bound concurrent (blocking) SQLite execution to the configured `--workers` count with a
  shared semaphore, instead of letting Tokio spawn up to `max_blocking_threads` (512) pool
  threads under load. Excess requests now wait for a permit rather than each holding an OS
  thread that busy-waits on SQLite's single write lock — mirroring the C++ server's fixed
  thread pool. Since SQLite serializes writes, this bounds thread/memory use under bursts
  of concurrent queries with no loss of throughput; reads still parallelize up to
  `--workers` under WAL.

## [0.1.0] - 2026-06-08

Initial release: a Rust port of the C++
[sqlite-server](https://github.com/miroslavbaudys/sqlite-server), wire- and
config-compatible with it (same protocol, same JSON config keys, same on-disk files).

### Added
- Length-prefixed JSON protocol over TCP with `QUERY`, `LIST`, and `DELETE_DB` commands,
  including the lenient parser that repairs unquoted keys (e.g. `{cmd:"LIST"}`).
- Tokio multi-threaded runtime with a configurable worker count.
- Bundled SQLite via `rusqlite` (compiled from source, no system dependency); one
  database file per name inside a configured folder, created on demand.
- Auto-tuned connections for concurrent multi-client access
  (`journal_mode=WAL`, `busy_timeout`, `synchronous=NORMAL`).
- **Optional password authentication** (`--auth` / `"auth"`): each connection must send
  `{"auth":"<password>"}` before any command; per-connection state. Disabled by default.
- **Optional IP whitelist** (`--ip-whitelist` / `"ip_whitelist"`): CIDRs and bare
  addresses (IPv4 and IPv6); non-whitelisted peers are dropped at accept time. Disabled
  by default.
- Reference Rust and Python example clients (both perform the auth handshake).
- Unit tests and end-to-end protocol tests; GitHub Actions CI.
- `#![forbid(unsafe_code)]` across the crate and example.

[0.1.1]: https://github.com/miroslavbaudys/sqlite-server-rs/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/miroslavbaudys/sqlite-server-rs/releases/tag/v0.1.0
