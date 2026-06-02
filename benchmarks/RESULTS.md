# ignis — benchmark results history

Persistent record of agent-benchmark runs with ignis. The full HTML reports and
`runs/` job dirs are gitignored (the HTML can be hundreds of MB); the small
per-trial **CSV** for each run is committed under
[`terminal-bench/history/`](terminal-bench/history/) and linked in the Report
column — **this table plus those CSVs are the canonical history**. One row per
completed run; newest first.

Columns:
- **Benchmark** — suite + size (task count).
- **Score** — `passed / total · pass%` (counts errored trials as not-solved).
- **Resolved%** — pass rate excluding errored trials (no verifier verdict).
- **Errored** — trials that raised an exception (agent/verifier timeout, crash, infra).
- **Cache hit** — cache-read tokens ÷ input tokens.
- **Report** — committed per-trial CSV for the run (the full HTML report stays local).

| Date | Benchmark | Model @ effort | ignis | Score | Resolved% | Errored | Input tok | Output tok | Cache hit | Report | Notes |
|------|-----------|----------------|-------|-------|----------:|--------:|----------:|-----------:|----------:|--------|-------|
| 2026-06-02 | Terminal-Bench 2.1 (89) | `minimax-token-plan/MiniMax-M3` | v0.32.0 † | 42/89 · 47.2% | 56.8% | 15 | 47.9M | 1.05M | 86.1% | [csv](terminal-bench/history/tb21-minimax-m3-20260602.csv) · [html](https://ignis-bench-reports.vercel.app/reports/tb21-minimax-m3-20260602) | First TB 2.1 run; first MiniMax-M3 baseline. **OpenAI protocol forced** over MiniMax's Anthropic-compat endpoint — ignis's Anthropic-protocol streaming parser duplicates tool-name deltas on that endpoint (`bash`→`bashbash`, every tool call fails). **Daytona disk cap dropped to 10 GB** since the prior run; preset reduced from 16 GB to fit. Mid-run, ~22% of trials saw provider-side stream drops (`connection closed before message completed`); motivated PR #97 (auto-retry on stream drop, not active for this run). 15 errored = all `AgentTimeoutError` on compute-bound tasks under the 2× budget. |
| 2026-05-29 | Terminal-Bench 2 (89) | `deepseek/deepseek-v4-flash@max` | v0.22.0 † | 54/89 · 60.7% | 69.2% | 11 | 127.0M | 2.28M | 98.1% | [csv](terminal-bench/history/tb2-deepseek-v4-flash-max-20260529.csv) · [html](https://ignis-bench-reports.vercel.app/reports/tb2-deepseek-v4-flash-max-20260529) | First full run. A Daytona control-plane blip crashed the orchestrator at 58/89; resumed the 47 unverified tasks with `--max-retries 2`. The 11 errored = 9 agent-timeout + 1 verifier-timeout + 1 non-zero-exit; two produced runaway multi-GB agent logs that burned the whole timeout. |

† In-sandbox binary is whatever `install.sh` fetched (latest release at run time) — the exact version isn't teed into trial logs, so this is best-known, not verified.

**Pending:** `deepseek/deepseek-v4-flash` with no effort suffix (default reasoning) — a same-model contrast to `@max`. After PR #97 lands, re-run MiniMax-M3 on TB 2.1 to measure the connection-drop recovery uplift.

---

## Terminal-Bench 2

`terminal-bench/terminal-bench-2` (89 tasks), via the harbor adapter in
[`terminal-bench/`](terminal-bench/).

```bash
cd benchmarks/terminal-bench

# 1. Run (Daytona shown; -e docker for local).
harbor run -d terminal-bench/terminal-bench-2 \
  -m deepseek/deepseek-v4-flash@max \
  --agent-import-path ignis_agent.agent:IgnisAgent \
  -e daytona --override-storage-mb 5120 -n 8 --max-retries 2 \
  -o runs/<name>

# 2. Aggregate the job dir → per-trial CSV (commit this one) + a bucket summary.
python3 scripts/aggregate_results.py runs/<name>/<timestamp> \
  -o history/<suite>-<model>-<effort>-<yyyymmdd>.csv

# 3. (optional) Single-file HTML drill-down report (stays local — gitignored).
python3 scripts/generate_report.py runs/<name>/<timestamp> -o report.html
```

Commit the step-2 CSV under `history/`, then add a row to the table above with the
headline numbers and a Report link to that CSV.

> If runs become routine, `aggregate_results.py` could grow an
> `--append-history ../RESULTS.md` flag to write the row automatically. Not built
> yet (YAGNI) — append by hand until the pattern recurs.
