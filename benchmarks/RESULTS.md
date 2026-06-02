# ignis — benchmark results history

Persistent record of agent-benchmark runs with ignis. The full HTML reports and
`runs/` job dirs are gitignored (the HTML can be hundreds of MB); the small
per-trial **CSV** for each run is committed under
[`terminal-bench/history/`](terminal-bench/history/) and linked in the Report
column — **this table plus those CSVs are the canonical history**. One row per
completed run; newest first.

Bucket taxonomy (`passed` / `failed` / `errored` — no other top-level buckets):
- **passed** — `reward == 1.0`. A trial that hit a transient error mid-turn but recovered stays here.
- **failed** — `reward == 0.0` *and no other markers*. The verifier ran and rejected the agent's completed solution. The only "real model miss" bucket.
- **errored** — everything else. The agent never got a fair, complete attempt. Sub-type in the Exception column: `AgentTimeoutError` / `VerifierTimeoutError` / `NonZeroAgentExitCodeError` (harness exception types), `ConnectionDropped` (provider stream drop), `RateLimited` (provider rate-limit), `Unknown` (missing reward, no other marker).

Columns:
- **Benchmark** — suite + size (task count).
- **Score** — `passed / total · pass%` (errored counts as not-solved).
- **Resolved%** — `passed / (passed + failed)`; excludes errored so the rate reflects how the model does *when it gets a clean attempt*.
- **Errored** — count of errored trials. Notes column breaks down by sub-type when interesting.
- **Cache hit** — cache-read tokens ÷ input tokens.
- **Report** — committed per-trial CSV in `terminal-bench/history/` + the rendered HTML on the Vercel host.

| Date | Benchmark | Model @ effort | ignis | Score | Resolved% | Errored | Input tok | Output tok | Cache hit | Report | Notes |
|------|-----------|----------------|-------|-------|----------:|--------:|----------:|-----------:|----------:|--------|-------|
| 2026-06-02 | Terminal-Bench 2.1 (89) | `minimax-token-plan/MiniMax-M3` | v0.32.0 † | 42/89 · 47.2% | 72.4% | 31 | 47.9M | 1.05M | 86.1% | [csv](terminal-bench/history/tb21-minimax-m3-20260602.csv) · [html](https://ignis-bench-reports.vercel.app/reports/tb21-minimax-m3-20260602) | First TB 2.1 run; first MiniMax-M3 baseline. **OpenAI protocol forced** over MiniMax's Anthropic-compat endpoint — ignis's Anthropic-protocol streaming parser duplicates tool-name deltas on that endpoint (`bash`→`bashbash`, every tool call fails — see [#99](https://github.com/Fullstop000/ignis/issues/99)). **Daytona disk cap dropped to 10 GB** since the prior run; preset reduced from 16 GB to fit. 31 errored = 15 `AgentTimeoutError` on compute-bound tasks + 16 `ConnectionDropped` (`connection closed before message completed`); the latter motivated PR #97 (auto-retry on stream drop, not active for this run). |
| 2026-05-29 | Terminal-Bench 2.0 (89) | `deepseek/deepseek-v4-flash@max` | v0.22.0 † | 54/89 · 60.7% | 70.1% | 12 | 127.0M | 2.28M | 98.1% | [csv](terminal-bench/history/tb2-deepseek-v4-flash-max-20260529.csv) · [html](https://ignis-bench-reports.vercel.app/reports/tb2-deepseek-v4-flash-max-20260529) | First full run. A Daytona control-plane blip crashed the orchestrator at 58/89; resumed the 47 unverified tasks with `--max-retries 2`. 12 errored = 9 agent-timeout + 1 verifier-timeout + 1 non-zero-exit + 1 `ConnectionDropped`; two produced runaway multi-GB agent logs that burned the whole timeout. |

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
