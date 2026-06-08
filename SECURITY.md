# Security Policy

## Supported Versions

Security fixes are provided for the latest release and the `main` branch.

| Version   | Supported          |
|-----------|--------------------|
| 0.1.x     | :white_check_mark: |
| < 0.1     | :x:                |

## Reporting a Vulnerability

**Please do not report security issues through public GitHub issues, pull
requests, or discussions.**

Report vulnerabilities privately through GitHub's private vulnerability
reporting:

1. Go to the [**Security**](../../security/advisories/new) tab of this repository.
2. Click **"Report a vulnerability"**.
3. Fill in the advisory form.

When reporting, please include as much of the following as you can:

- A description of the issue and its potential impact.
- The affected version or commit hash.
- Step-by-step instructions to reproduce, ideally with a minimal request
  (the `cmd` / `db` / `query` payload) or proof of concept.
- Any suggested remediation.

### What to expect

- **Acknowledgement:** within 7 days.
- **Assessment & updates:** we will confirm the issue, determine its severity,
  and keep you informed of progress.
- **Disclosure:** we follow coordinated disclosure. Please give us reasonable
  time to release a fix before any public disclosure. Credit will be given to
  reporters who wish to be named.

## Scope and deployment notes

`sqlite-server-rs` is designed to run **within a trusted network boundary**.
Please keep the following in mind before assessing or deploying it:

- **No transport encryption.** Traffic — including the optional auth password — is
  sent in clear text. Do not expose the server directly to untrusted networks or the
  public internet; place it behind a firewall, private network, or a TLS-terminating
  proxy.
- **Optional, defence-in-depth access controls.** An optional password (`--auth` /
  `"auth"`) requires each connection to authenticate before running commands, and an
  optional IP allow-list (`--ip-whitelist` / `"ip_whitelist"`, CIDR or bare addresses)
  drops connections from other peers at accept time. Both are off by default. They
  reduce exposure but, without TLS, do not protect the password on the wire and are
  **not** a replacement for network-level isolation. The whitelist trusts the direct
  peer address, so terminate any NAT/proxy accordingly.
- **Arbitrary SQL execution is by design.** Any client that can reach the socket (and
  authenticate, if a password is set) sends SQL that is executed against the target
  database and may create or delete database files. Reports about a client being able to
  run SQL are expected behaviour, not vulnerabilities, unless they cross a boundary the
  server is meant to enforce.
- **In scope:** anything that lets a request escape its intended limits — e.g.
  reading, writing, or deleting files **outside** the configured databases
  folder (path traversal), crashes triggered by crafted requests (panics, or
  memory-safety issues in the bundled SQLite C library reachable from a request),
  or denial of service reachable below the configured `client-max-packet-size`.
- **Dependencies:** the server links the bundled SQLite amalgamation (via
  `rusqlite`) and a small set of Rust crates. Vulnerabilities in those that are
  reachable through this server are in scope; please mention the affected
  dependency and version.

Thank you for helping keep `sqlite-server-rs` and its users safe.
