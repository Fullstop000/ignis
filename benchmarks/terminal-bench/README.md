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
├── run.sh                  # reproducible runner with sane defaults + env presets
├── scripts/                # stdlib-only result aggregator + HTML report generator
├── history/                # committed per-run CSVs (linked from ../RESULTS.md)
└── README.md
```

Run history and headline scores live in [`../RESULTS.md`](../RESULTS.md).

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

**Web search (optional).** If `BRAVE_API_KEY` (preferred) or `TAVILY_API_KEY` is
in the adapter's env, it writes a `[web_search]` block into the sandbox config so
the in-sandbox `web_search` tool works — needed for tasks that require looking
something up. Absent → the tool stays disabled.

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

Use `run.sh` — it reads keys from `~/.ignis/config.toml`, picks sane defaults,
and aggregates nothing on its own (run the `scripts/` afterward). Keys never
print.

```bash
./run.sh                                   # deepseek/deepseek-v4-flash@max on Daytona
MODEL=anthropic/claude-haiku-4-5 ./run.sh  # any wired provider
ENV=docker  ./run.sh                       # local, no cloud quota
ENV=novita  ./run.sh                       # cheapest cloud, roomy disk
```

It sets `--max-retries 2` (a transient cloud blip retries the trial instead of
crashing the whole job) and `--agent-timeout-multiplier 2.0` (compute-heavy
tasks — builds, ML training — need more than the stock budget). Override any
default via env: `MODEL`, `ENV`, `DATASET`, `NCONC`, `STORAGE_MB`,
`TIMEOUT_MULT`, `MAX_RETRIES`.

### Disk vs concurrency (the one real tradeoff)

Tasks that install heavy wheels (torch, cudnn) need real disk, or the
**verifier** dies with *"No space left on device"* and never tests the agent's
work. But on Daytona, `disk × concurrency` must fit the ~50 GB account quota, so
bigger disk forces lower `-n`. `run.sh` presets this per env:

| `ENV`    | disk / sandbox | `-n` | notes |
| -------- | -------------- | ---- | ----- |
| `daytona`| 16 GB          | 3    | fits the quota; slower |
| `novita` | 20 GB          | 8    | no tight quota — fast + roomy (cheapest, see RESULTS) |
| `docker` | host disk      | 4    | local; no `--override-storage-mb` |

So disk-heavy suites are better on local Docker or Novita than quota-limited
Daytona. Note: any `--override-storage-mb` already disqualifies leaderboard
submission — this is for measuring, not submitting.

After a run, aggregate + report with `scripts/` (see [`../RESULTS.md`](../RESULTS.md)).

## Open knobs

- emit an ATIF trajectory in `populate_context_post_run` (we currently only tee
  raw stdout) so cost surfaces in Harbor's report alongside the token counts
- the ~17 capability-ceiling failures from the first run move with a stronger
  model, not the harness
