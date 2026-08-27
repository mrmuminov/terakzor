# Terakzor

Terakzor is a single-binary, local system metrics monitor written in Rust.

## Features

- Collects CPU usage, RAM used, disk used, uptime, and one-minute load average.
- Serves a local dashboard and JSON metrics API.
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

The dashboard is available by default at `http://127.0.0.1:3000/`.

## Dashboard and API

- `GET /` serves the dashboard.
- `GET /api/metrics` returns metric samples from the last 24 hours.

Example response:

```json
{ "samples": [{ "timestamp": 0, "cpu_percent": 0.0 }] }
```

Each sample includes `timestamp` plus available enabled metric fields from `cpu_percent`, `ram_used_bytes`, `disk_used_bytes`, `uptime_seconds`, and `load_average_1m`. Fields may be absent when disabled or unavailable.

## Configuration

The sample configuration file is `terakzor.toml`.

- `listen_address` defaults to `127.0.0.1:3000`.
- `collection_interval_seconds` defaults to `60` and must be positive.
- `retention_days` defaults to `7` and must be positive.
- `[metrics]` enables or disables `cpu_percent`, `ram_used_bytes`, `disk_used_bytes`, `uptime_seconds`, and `load_average_1m`.

```toml
listen_address = "127.0.0.1:3000"
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

Metrics are stored in `terakzor.db` in the current directory using embedded Stoolap. The default retention period is 7 days. Expired data is removed at startup and then daily.

On SIGINT or SIGTERM, where supported, Terakzor stops gracefully: it drains queued metrics before closing the database.

## Development

Run the development validation suite with:

```bash
cargo test --all-targets
```

## Platform Notes

On Windows, sysinfo reports the load average as `0`.
