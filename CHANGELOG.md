# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Switchable `web_search` backend with Brave and Tavily providers, selected via
  `[web_search]` in `config.toml` (`provider` + `api_key`).
- `docs/web-search.md` tutorial for configuring and using web search.
- Project documentation: `README.md`, `CONTRIBUTING.md`, this changelog, and an
  Apache-2.0 `LICENSE`.
- CI workflow (`fmt` + `clippy -D warnings` + tests on Linux/macOS) and a
  cross-platform release workflow triggered by `v*` tags.
- `config.example.toml` template.

### Changed
- Configuration format migrated from YAML to TOML; config is loaded from
  `~/.ignis/config.toml`.

### Removed
- Dead DuckDuckGo HTML-scraping implementation of `web_search` (it had stopped
  returning results due to anti-bot challenges).

### Security
- Removed a real API key that was present in a local (unpushed) commit and
  git-ignored local config files so secrets can no longer be committed.
