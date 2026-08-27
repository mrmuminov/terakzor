# Terakzor

Terakzor is a single-binary, local system metrics monitor written in Rust.


## Features

- Collects CPU usage, RAM used, disk used, network interface traffic (RX/TX), swap usage, uptime, and load averages.
- Provides a dynamic dashboard with a sidebar layout and categorized charts (Compute, Memory, Storage, Network, System).
- Exposes a standard JSON metrics API and a Model Context Protocol (MCP) server for LLM agents.
- Stores metrics locally with embedded Stoolap and no external database service.
- Drains queued metrics during graceful shutdown before closing the database.

## Requirements

- Rust toolchain and Cargo.
- No external database service.

## Build and Run

Build the release binary:

```bash
cargo build --release
```

The binary is written to `target/release/terakzor`.

For development, run:

```bash
cargo run
```

The dashboard is available by default at `http://localhost:6972/`.

<img width="2329" height="1393" alt="image" src="https://github.com/user-attachments/assets/23218ad2-26e7-446e-8093-53f7e74e5cfe" />



## Model Context Protocol (MCP)

Terakzor ships with an embedded MCP server over HTTP (SSE transport) designed for AI agents (like Claude Desktop). This allows LLMs to interactively read local system telemetry.

- **Endpoint**: `GET /mcp/sse` (Agent opens an SSE stream, which redirects it to `POST /mcp/messages`).
- **Authentication**: Requires an `Authorization: Bearer <TOKEN>` header. The token is generated automatically on first installation (for Linux packages) or can be manually set via `mcp_token` in `terakzor.toml`.
- **Tools Provided**:
  - `get_current_status`: Real-time system vitals.
  - `get_historical_metrics`: Fetch recent history for any specific metric.
  - `get_host_info`: OS, kernel, memory, and CPU core count.

To test the MCP endpoint manually:
```bash
curl -N -H "Authorization: Bearer $(grep mcp_token /etc/terakzor/terakzor.toml | cut -d'"' -f2)" http://localhost:6972/mcp/sse
```
<img width="1387" height="913" alt="image" src="https://github.com/user-attachments/assets/8d876795-f375-4247-955a-d4fa5acf4788" />


## Dashboard and API

- `GET /` serves the dashboard.
- `GET /api/metrics` returns metric samples from the last 24 hours.
- `GET /assets/uplot-1.6.32/uPlot.min.css` serves the embedded chart stylesheet.
- `GET /assets/uplot-1.6.32/uPlot.iife.min.js` serves the embedded chart script.

The dashboard embeds and serves uPlot 1.6.32 locally, so chart rendering does not require external network access. See `THIRD_PARTY_NOTICES` for its MIT license notice.

Example response:

```json
{ "samples": [{ "timestamp": 0, "cpu_percent": 0.0 }] }
```

Each sample includes `timestamp` plus available enabled metric fields from `cpu_percent`, `ram_used_bytes`, `disk_used_bytes`, `uptime_seconds`, and `load_average_1m`. Fields may be absent when disabled or unavailable.

## Configuration

The sample configuration file is `terakzor.toml`.

- `listen_address` defaults to `localhost:6972`.
- `collection_interval_seconds` defaults to `60` and must be positive.
- `retention_days` defaults to `7` and must be positive.
- `[metrics]` enables or disables `cpu_percent`, `ram_used_bytes`, `disk_used_bytes`, `uptime_seconds`, and `load_average_1m`.

```toml
listen_address = "localhost:6972"
collection_interval_seconds = 60
retention_days = 7

[metrics]
cpu_percent = true
ram_used_bytes = true
disk_used_bytes = true
uptime_seconds = true
load_average_1m = true
```

## Config File Lookup

Terakzor selects a configuration file in this order:

1. `--config <path>`; the path must be a valid regular file.
2. `TERAKZOR_CONFIG`; the path must be a valid regular file.
3. `./terakzor.toml`.
4. User configuration:
   - Linux: `~/.config/terakzor/terakzor.toml`
   - macOS: `~/Library/Application Support/terakzor/terakzor.toml`
   - Windows: `%APPDATA%\terakzor\terakzor.toml`
5. `/etc/terakzor/terakzor.toml` on non-Windows systems.
6. Built-in defaults.

An explicit invalid or missing `--config` or `TERAKZOR_CONFIG` path is an error. A bare `--config` is also an error.

## Storage and Retention

Metrics are stored with embedded Stoolap in the local application data directory:

- Linux: `${XDG_DATA_HOME:-~/.local/share}/terakzor/terakzor.db`
- macOS: `~/Library/Application Support/terakzor/terakzor.db`
- Windows: `%LOCALAPPDATA%\\terakzor\\terakzor.db`

The packaged Linux service sets `XDG_DATA_HOME=/var/lib`, so its database is
`/var/lib/terakzor/terakzor.db`. Existing databases in a previous working
directory are not moved automatically. The default retention period is 7 days;
expired data is removed at startup and then daily.

On SIGINT or SIGTERM, where supported, Terakzor stops gracefully: it drains queued metrics before closing the database.

## Linux Packages

Version tags publish Linux packages for `x86_64` and `aarch64` when supported
by the target distribution:

- Debian 9+ and Ubuntu 16.04+: `.deb`
- RHEL 7+ compatible systems: `.rpm`
- Alpine 3.5+: `.apk`
- Current Arch Linux: `.pkg.tar.zst`

Download the package matching the system architecture from the GitHub Release,
then install it with the native package manager:

```bash
sudo apt install ./terakzor-<version>-amd64.deb
sudo dnf install ./terakzor-<version>-amd64.rpm
sudo apk add --allow-untrusted ./terakzor-<version>-amd64.apk
sudo pacman -U ./terakzor-<version>-amd64.pkg.tar.zst
```

Installation creates the `terakzor` service account, installs the default
configuration at `/etc/terakzor/terakzor.toml`, creates persistent state at
`/var/lib/terakzor`, enables the service, and starts it. Systemd-based systems
use `systemctl`; Alpine uses OpenRC:

```bash
sudo systemctl status terakzor
sudo systemctl restart terakzor
sudo rc-service terakzor status
sudo rc-service terakzor restart
```

The service always loads `/etc/terakzor/terakzor.toml` explicitly. Package
upgrades preserve local configuration changes and may leave a package-manager
replacement file such as `.rpmnew`, `.apk-new`, or `.pacnew` for review.
Removing a package stops and disables the service while retaining the service
account and `/var/lib/terakzor` metrics database. Package managers normally
retain modified configuration files; an untouched packaged default may be
removed.

## Development

Run the development validation suite with:

```bash
cargo test --all-targets
```

## GitHub Builds

GitHub Actions runs for numeric SemVer tags. It checks formatting, linting, and
tests; builds static Linux binaries and native macOS/Windows binaries; then
installs the x86_64 Linux packages in Debian 9, Ubuntu 16.04, CentOS 7, Alpine
3.5, and Arch Linux containers. It publishes the Linux packages and
`SHA256SUMS` on the GitHub Release.

## License

Terakzor is licensed under the Apache License 2.0. See [LICENSE](LICENSE).

## Platform Notes

On Windows, sysinfo reports the load average as `0`.
