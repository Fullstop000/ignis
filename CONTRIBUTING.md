# Contributing to Ignis

Thanks for your interest in improving Ignis! This guide covers the workflow and
the quality bar for changes.

## Development setup

You need a stable Rust toolchain (`rustup` recommended).

```bash
git clone https://github.com/Fullstop000/ignis.git
cd ignis
cargo build
```

To run the agent locally, create `~/.ignis/config.toml` from
`config.example.toml` and add an API key.

## The quality gate

Every change must pass all three checks — CI enforces them, so run them locally
first:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Zero compiler warnings and zero clippy warnings are required.

## Workflow

1. Create a branch off `master` (e.g. `feat/web-search-cache`, `fix/resume-crash`).
2. Make your change, keeping it tightly scoped (see the guidelines below).
3. Add or update tests for the behavior you changed.
4. Update `CHANGELOG.md` under the `Unreleased` section.
5. Open a pull request. Fill in the PR template and make sure CI is green.
6. A maintainer reviews and merges — please don't merge your own PR.

## Commit messages

Use [Conventional Commits](https://www.conventionalcommits.org/): `feat:`,
`fix:`, `docs:`, `chore:`, `refactor:`, `test:`, etc.

## Coding guidelines

- **Simplicity / YAGNI.** Add the minimum code that solves the problem. Avoid
  speculative abstractions, compatibility shims, and unrelated cleanups.
- **Keep dependencies minimal.** Only add a crate if it earns its place.
- **Single binary.** No external runtime dependencies.
- **Fail loudly.** Surface errors clearly; never silently swallow failures.
- **Never commit secrets.** Keep API keys in `~/.ignis/config.toml` (git-ignored).

See [CLAUDE.md](CLAUDE.md) for the full style rules.

## Releases

Releases are cut by pushing a `vX.Y.Z` tag, which triggers the release workflow
to build and attach cross-platform binaries. See
[.github/workflows/release.yml](.github/workflows/release.yml).
