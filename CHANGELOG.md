# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.1] - 2026-08-27
### Added
- Added a lightweight SVG pulse favicon to the web dashboard as an inline Data URI.

## [0.2.0] - 2026-08-27
### Added
- **Dynamic Sidebar Dashboard**: The web UI now features a sidebar layout with categorized metrics (Compute, Memory, Storage, Network, System).
- **Per-Interface Network Charts**: Network traffic is now dynamically split into individual charts for each interface (e.g., `eth0`, `wlan0`).
- **Model Context Protocol (MCP)**: Added an embedded HTTP SSE MCP Server for LLM agents to fetch system status and historical metrics.
- **MCP Security**: Automatically generates a 32-character secure random token (`mcp_token`) on the first package installation to protect MCP endpoints.
- **New Metrics**: Added support for 5m/15m load averages, swap usage, and individual network interface RX/TX tracking.

### Changed
- **Default Port**: Changed the default web server binding from `127.0.0.1:3000` to `localhost:6972`.
- Refactored `src/main.rs` into modular components (`config.rs`, `db.rs`, `metrics.rs`, `web.rs`, `mcp.rs`).
- Enhanced `.readout` visual elements in the web dashboard for a cleaner, compact appearance.
