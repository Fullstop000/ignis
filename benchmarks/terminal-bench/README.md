# Terminal-Bench 2 adapter for ignis

Runs [ignis](https://github.com/Fullstop000/ignis) as a custom agent inside the
[Harbor](https://www.harborframework.com) harness — the official runner for
Terminal-Bench 2.0.

```
benchmarks/terminal-bench/
├── pyproject.toml          # python package: depends on harbor>=0.8
├── ignis_agent/
│   ├── __init__.py
│   └── agent.py            # IgnisAgent (BaseInstalledAgent subclass)
└── README.md
```

## How it works

`IgnisAgent` is a Harbor `BaseInstalledAgent`:

1. **`install()`** – installs `curl`/`ca-certificates`, then pipes
   `install.sh` to `sh` with `IGNIS_INSTALL_DIR=/usr/local/bin` so the binary
   lands on root + agent-user PATH without profile sourcing. Pin a release by
   passing `--version <tag>` (or setting `_version` on the agent) — that
   surfaces as `IGNIS_VERSION=<tag>` to `install.sh`.
2. **`run()`** – parses `--model provider/name`, looks up the matching
   API-key env var (forwarded from the host into the sandbox by Harbor),
   writes a minimal `~/.ignis/config.toml`, then invokes `ignis "<instruction>"`.
   A non-empty prompt arg switches ignis off the TUI and into one-shot
   streaming mode (see `ignis/src/main.rs::main`). stdout/stderr are teed to
   `/logs/agent/ignis.txt` for trial inspection.

## Supported providers

| `--model` prefix | Env var the agent reads |
| ---------------- | ----------------------- |
| `anthropic/*`    | `ANTHROPIC_API_KEY`     |
| `openai/*`       | `OPENAI_API_KEY`        |
| `gemini/*`       | `GEMINI_API_KEY`        |
| `deepseek/*`     | `DEEPSEEK_API_KEY`      |

Add more in `_PROVIDERS` at the top of `ignis_agent/agent.py` — anything ignis
already knows about (`ollama`, `kimi-code`, `Moonshot Platform CN`) just needs
an entry.

## Setup

```bash
cd benchmarks/terminal-bench

# 1. Install harbor + this adapter into an isolated venv.
uv venv
uv pip install -e .          # pulls harbor and registers ignis_agent

# 2. Sanity-check the harness against the oracle (no model needed).
uv run harbor run -d terminal-bench/terminal-bench-2 -a oracle
```

Docker must be installed and running.

## Run a real benchmark with ignis

```bash
export ANTHROPIC_API_KEY=sk-ant-...

uv run harbor run \
  -d terminal-bench/terminal-bench-2 \
  -m anthropic/claude-haiku-4-5 \
  --agent-import-path ignis_agent.agent:IgnisAgent
```

Add `-n 32 --env daytona` (and `DAYTONA_API_KEY`) to fan out trials on Daytona
sandboxes. The Harbor docs cover the rest:
<https://www.harborframework.com/docs/tutorials/running-terminal-bench>.

## Open knobs

The first pass is intentionally small. Likely follow-ups once we have a score:

- pin `IGNIS_VERSION` so a re-run is reproducible
- emit an ATIF trajectory in `populate_context_post_run` (we currently only
  tee raw stdout) so token counts + cost surface in Harbor's report
- expose ignis-specific `CliFlag`s (reasoning effort, model context override)

For now: install, model selection, one-shot invocation, log tee. That's it.
