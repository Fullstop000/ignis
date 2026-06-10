# ignis — benchmark results history

Persistent record of agent-benchmark runs with ignis. The full HTML reports and
`runs/` job dirs are gitignored (the HTML can be hundreds of MB); the small
per-trial **CSV** for each run is committed under
[`terminal-bench/history/`](terminal-bench/history/) and linked in the Report
column — **this table plus those CSVs are the canonical history**. One row per
completed run; newest first.

Bucket taxonomy (`passed` / `failed` / `errored` — no other top-level buckets):
- **passed** — `reward == 1.0`. A trial that hit a transient error mid-turn but recovered stays here.
- **failed** — a real model miss. Either the verifier ran and rejected the agent's completed work (`exception` blank), or the agent burned through its full timeout budget (`AgentTimeoutError`, marked `TimedOut` in the Exception column). Running out of time IS a capability signal — a stronger model finishes the same task faster — so timeouts belong here, not in `errored`.
- **errored** — the model never got a fair shot. Sub-type in the Exception column: `VerifierTimeoutError` / `NonZeroAgentExitCodeError` (harness/sandbox faults), `ConnectionDropped` (provider stream drop), `RateLimited` (provider rate-limit), `Unknown` (missing reward, no other marker).

Columns:
- **Benchmark** — suite + size (task count).
- **Score** — `passed / total · pass%` (errored counts as not-solved).
- **Resolved%** — `passed / (passed + failed)`; excludes errored so the rate reflects how the model does *when it gets a clean attempt*.
- **Errored** — count of errored trials. Notes column breaks down by sub-type when interesting.
- **Cache hit** — cache-read tokens ÷ input tokens.
- **Est. cost ‡** — `(input - cache) × miss-price + cache × hit-price + output × out-price`, summed across all 89 trials and converted to USD. See footnote below the table.
- **Report** — committed per-trial CSV in `terminal-bench/history/` + the rendered HTML on the Vercel host.

Per-trial raw traces (agent stdout log, session JSONL, verifier output, all secret-redacted) live in a private repo: `Fullstop000/ignis-bench-traces`, one GitHub Release per run. Built via `scripts/bundle_traces.py`.

| Date | Benchmark | Model @ effort | ignis | Score | Resolved% | Errored | Input tok | Output tok | Cache hit | Est. cost ‡ | Report | Notes |
|------|-----------|----------------|-------|-------|----------:|--------:|----------:|-----------:|----------:|-----------:|--------|-------|
| 2026-06-09 | Terminal-Bench 2.1 (89) | `ark-coding/glm-5.1` | v0.38.0-rc.1 | 57/89 · 64.0% | 65.5% | 2 | 31.8M | 0.65M | 0.0% | ≈$33.15 (≈$0.58/pass) | [csv](terminal-bench/history/tb21-glm-5.1-ark-20260609.csv) · [html](https://ignis-bench-reports.vercel.app/reports/tb21-glm-5.1-ark-20260609) · [compare](https://ignis-bench-reports.vercel.app/compare/tb21-compare-20260609) · [traces](https://github.com/Fullstop000/ignis-bench-traces/releases/tag/tb21-glm-5.1-ark-20260609) | First run on `ark-coding/glm-5.1` (Volcengine Ark Coding Plan, OpenAI-compat endpoint `/api/coding/v3`). **New SOTA on TB 2.1**: 64.0%, +4.4 pp over kimi-2.6 (59.6%) and +7.8 pp over `deepseek-v4-flash@max` (56.2%). Cleanest run in the table — zero `RateLimited` / `ConnectionDropped` / billing aborts. 30 failed = 20 verifier-reject + 10 `TimedOut`; 2 errored = both `VerifierTimeoutError` (harness, not model). **Lowest token spend of any run** (31.8M input vs deepseek 328M, kimi 119M) yet **highest per-pass cost** — the catch is **cache hit 0.0%**: Ark's OpenAI-compatible endpoint returns no `cached_tokens`, so every input token bills at the full miss rate. The same run at kimi-like 97% cache would be ≈$10.95 (≈$0.19/pass) — caching is the entire cost delta. Ark Coding Plan is a flat-fee subscription, so the figure is the per-token equivalent at published Z.ai GLM-5.1 rates, not out-of-pocket. RC binary pinned via `IGNIS_VERSION=v0.38.0-rc.1` (the `ark-coding` provider shipped under `[Unreleased]`, not yet in a stable release; binary self-reports `0.37.0` since the crate version wasn't bumped). A prior attempt died at 22/89 (mean 0.500) when the benchmark worktree was deleted mid-run; this row is the clean full re-run from the main checkout. |
| 2026-06-06 | Terminal-Bench 2.1 (89) | `kimi-code/kimi-for-coding` | v0.34.0 † | 53/89 · 59.6% | 63.9% | 6 | 119.0M | 1.93M | 97.3% | ≈$28.19 (≈$0.53/pass) | [csv](terminal-bench/history/tb21-kimi-for-coding-20260606.csv) · [html](https://ignis-bench-reports.vercel.app/reports/tb21-kimi-2.6-20260606) · [compare](https://ignis-bench-reports.vercel.app/compare/tb21-compare-20260606) | First run on `kimi-code/kimi-for-coding` (Kimi Coding Plan, k2.6 family). **New SOTA on this dataset**: +3.4 pp over `deepseek-v4-flash@max` (56.2%) and +7.9 pp over MiniMax-M3 (51.7%). Per-pass token cost is the catch — ≈$0.53 vs $0.046 for DS (~12×) and $0.20 for MM3 (~2.6×); in practice the Coding Plan is a flat-fee subscription, so the headline figure is the per-token equivalent at published k2.6 API rates, not out-of-pocket. 30 failed = 16 verifier-reject + 14 `TimedOut`; 6 errored = all `RateLimited` (explicit 429 aborts). Bigger issue is Kimi's **silent per-account throughput throttle** under parallel keys — token output rate collapsed from ~25 tok/s to 1–2 tok/s on the long-stalling trials without raising 429s, eating the wall-time budget. Cache hit 97.3% (strip-think trim from PR #123 keeping the prefix stable across replays). |
| 2026-06-03 | Terminal-Bench 2.1 (89) | `deepseek/deepseek-v4-flash@max` | v0.33.1 † | 50/89 · 56.2% | 61.7% | 8 | 328.2M | 3.13M | 99.0% | ≈$2.28 (≈$0.05/pass) | [csv](terminal-bench/history/tb21-deepseek-v4-flash-max-20260603.csv) · [html](https://ignis-bench-reports.vercel.app/reports/tb21-ds-v4-flash-max-20260603) | First TB 2.1 baseline for `deepseek-v4-flash@max`. Score 56.2% sits between TB 2.0 deepseek@max (60.7%) and TB 2.1 MiniMax-M3 (51.7%) — TB 2.1 is harder, drop is expected. Resolved% 61.7% essentially matches TB 2.0 deepseek@max (62.8%), so model capability is consistent. 31 failed = 26 verifier-reject + 5 `TimedOut`; 8 errored = 7 `ConnectionDropped` + 1 `VerifierTimeoutError`. Auto-retry (PR #97) is active in v0.33.1 binary, but `ConnectionDropped` still climbed to 7 here — deepseek's streaming endpoint appears flakier than MiniMax's OpenAI-protocol path. Input tokens 328M is by far the highest of the three runs (reasoning effort `@max` is generous), cache hit 99.0%. |
| 2026-06-03 | Terminal-Bench 2.1 (89) | `minimax-token-plan/MiniMax-M3` | v0.33.0 † | 46/89 · 51.7% | 53.5% | 3 | 91.9M | 2.20M | 93.8% | ≈$9.39 (≈$0.20/pass) | [csv](terminal-bench/history/tb21-minimax-m3-20260603.csv) · [html](https://ignis-bench-reports.vercel.app/reports/tb21-minimax-m3-20260603) | Re-run on v0.33.0 with PR #97's stream-drop auto-retry active. `ConnectionDropped` dropped from 16 → 2 vs the 06-02 baseline (-87.5%); the third errored row was a single `VerifierTimeoutError`. 40 failed = 28 verifier-reject + 12 `TimedOut`. Score +4.5 pp; Resolved% slipped -4.0 pp because the denominator grew with recovered (mostly non-trivial) trials. Input tokens nearly doubled (47.9M → 91.9M) — auto-retry replays context on each attempt and a possible mid-run v0.33.1 sandbox refresh (released today, no behavioral change for the OpenAI-forced MiniMax path); cache-hit rose to 93.8%. |
| 2026-06-02 | Terminal-Bench 2.1 (89) | `minimax-token-plan/MiniMax-M3` | v0.32.0 † | 42/89 · 47.2% | 57.5% | 16 | 47.9M | 1.05M | 86.1% | ≈$5.65 (≈$0.13/pass) | [csv](terminal-bench/history/tb21-minimax-m3-20260602.csv) · [html](https://ignis-bench-reports.vercel.app/reports/tb21-minimax-m3-20260602) | First TB 2.1 run; first MiniMax-M3 baseline. **OpenAI protocol forced** over MiniMax's Anthropic-compat endpoint — ignis's Anthropic-protocol streaming parser duplicates tool-name deltas on that endpoint (`bash`→`bashbash`, every tool call fails — see [#99](https://github.com/Fullstop000/ignis/issues/99)). **Daytona disk cap dropped to 10 GB** since the prior run; preset reduced from 16 GB to fit. 31 failed = 16 verifier-reject + 15 `TimedOut` on compute-bound tasks; 16 errored = all `ConnectionDropped` (`connection closed before message completed`), motivating PR #97 (auto-retry on stream drop, not active for this run). |
| 2026-05-29 | Terminal-Bench 2.0 (89) | `deepseek/deepseek-v4-flash@max` | v0.22.0 † | 54/89 · 60.7% | 62.8% | 3 | 127.0M | 2.28M | 98.1% | ≈$1.34 (≈$0.02/pass) | [csv](terminal-bench/history/tb2-deepseek-v4-flash-max-20260529.csv) · [html](https://ignis-bench-reports.vercel.app/reports/tb2-deepseek-v4-flash-max-20260529) | First full run. A Daytona control-plane blip crashed the orchestrator at 58/89; resumed the 47 unverified tasks with `--max-retries 2`. 32 failed = 23 verifier-reject + 9 `TimedOut`; 3 errored = 1 `VerifierTimeoutError` + 1 `NonZeroAgentExitCodeError` + 1 `ConnectionDropped`; two produced runaway multi-GB agent logs that burned the whole timeout. |

† In-sandbox binary is whatever `install.sh` fetched (latest release at run time) — the exact version isn't teed into trial logs, so this is best-known, not verified.

‡ **Cost methodology.** Computed from the per-trial token totals in the linked CSV × published platform pricing in CNY, converted at ≈1 USD = 7.10 CNY (mid-2026 rate). Prices (CNY per 1M tokens, in `input-miss / cache-hit / output` form): `deepseek-v4-flash` = `1.00 / 0.02 / 2.00` ([source](https://api-docs.deepseek.com/zh-cn/quick_start/pricing/)); `kimi-for-coding` (i.e. kimi-k2.6) = `6.50 / 1.10 / 27.00` ([source](https://platform.kimi.com/docs/pricing/chat-k26)); `MiniMax-M3` = `2.10 / 0.42 / 8.40` (standard tier, ≤512k context, promotional rate; [source](https://platform.minimaxi.com/docs/guides/pricing-paygo.md)); `glm-5.1` = `6.96 / 1.85 / 21.87` (≈ $0.98 / $0.26 / $3.08 per 1M at 7.10; published Z.ai rate, [source](https://openrouter.ai/z-ai/glm-5.1)). `kimi-for-coding` and `ark-coding/glm-5.1` are in practice flat-fee Coding Plan subscriptions, so their figures are the per-token equivalent at the published API rate, not actual out-of-pocket. **Ark/GLM-5.1 cache hit was 0.0%** — the Ark `/api/coding/v3` OpenAI-compatible endpoint returns no `cached_tokens`, so the entire input bills at the miss rate, which is why its per-pass cost is the table's highest despite the lowest token count.

**Pending:** `deepseek/deepseek-v4-flash` with no effort suffix (default reasoning) — a same-model contrast to `@max`.

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
