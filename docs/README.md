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
- [Skills](configure/skills.md) — discovering, authoring, and toggling
  `SKILL.md` instruction sets (4 discovery roots, frontmatter contract,
  bundled-file resources, `/skills` picker, `/<skill-name>` force-load).
- [MCP servers](configure/mcp.md) — wiring external Model Context Protocol
  servers (`ignis mcp add|list|get|remove|enable|disable`, the
  `mcp__<server>__<tool>` naming convention, `/mcp` runtime toggle).
- [Telemetry](configure/telemetry.md) — OpenTelemetry exporter setup (OTLP
  endpoint, headers, sampling), the `/telemetry` picker, and what's emitted.
