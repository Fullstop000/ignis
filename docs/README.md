# Ignis docs

Reference material for using and configuring [ignis](../README.md). The
[top-level README](../README.md) covers install, quickstart, and the high-level
feature list — these pages go deeper.

## Usage

How to drive ignis day to day.

- [Commands](usage/commands.md) — every built-in TUI slash command (`/resume`,
  `/clear`, `/compact`, `/copy`, `/model`, `/skills`, `/mcp`, `/afk`,
  `/telemetry`, `/sessions`) plus the `/<skill-name>` force-load form, with a
  global keybindings table.

## Configure

How to customize ignis through `~/.ignis/config.toml` and the in-TUI pickers.

- [Permissions](configure/permissions.md) — the `allow` / `ask` / `deny` rule
  grammar, the built-in safety floor, mode precedence (default · hands-free ·
  fully-unattended), and the `/afk` runtime toggle.
- [Telemetry](configure/telemetry.md) — OpenTelemetry exporter setup (OTLP
  endpoint, headers, sampling), the `/telemetry` picker, and what's emitted.
