"""
Aggregate a harbor TB2 job directory into a per-trial CSV.

Usage:
    python aggregate_results.py <job_dir> [-o results.csv]

The job dir is whatever `harbor run -o ...` produced — it contains one subdir
per trial (`<task>__<hash>/`) plus the top-level result.json. The script reads
each trial's result.json and synced ignis log, classifies the trial into a
non-overlapping bucket (passed / failed / errored / quota), and writes a CSV
with one row per trial.

Columns:
    task                 — task name (no trial hash)
    trial                — full trial dir name (`<task>__<hash>`)
    bucket               — passed | failed | errored | quota | unknown
    reward               — 1.0 / 0.0 / "" (no verifier result)
    exception            — exception_type if errored, else ""
    rate_limited         — true if the agent log shows kimi's rate_limit_reached_error
    duration_seconds     — wall time from harbor's timestamps
    n_input_tokens       — sum from synced ignis-projects/**/<session>.usage.json
    n_output_tokens
    n_cache_read_tokens
    cache_hit_rate       — n_cache_read_tokens / n_input_tokens (rounded to 3 dp)
    started_at           — ISO 8601
    finished_at          — ISO 8601

Bucket precedence (avoids the double-count harbor's top-level stats produce
when a passing trial also has an exception from the post-success phase):
    reward == 1.0   → passed   (even if exception was raised after the work)
    exception_info  → errored
    rate-limit log  → quota
    reward == 0.0   → failed
    otherwise       → unknown
"""

from __future__ import annotations

import argparse
import csv
import datetime as dt
import json
import sys
from pathlib import Path
from typing import Any


def _safe_load(p: Path) -> Any:
    try:
        return json.loads(p.read_text())
    except (OSError, json.JSONDecodeError):
        return None


def _wall_seconds(started: str | None, finished: str | None) -> float | None:
    if not (started and finished):
        return None
    try:
        return (dt.datetime.fromisoformat(finished) - dt.datetime.fromisoformat(started)).total_seconds()
    except ValueError:
        return None


def _sum_usage(usage_dir: Path) -> tuple[int, int, int]:
    """Sum input/output/cache_read across every ignis *.usage.json in the tree."""
    in_t = out_t = cache_t = 0
    if not usage_dir.exists():
        return in_t, out_t, cache_t
    for f in usage_dir.rglob("*.usage.json"):
        data = _safe_load(f)
        if not isinstance(data, dict):
            continue
        in_t += int(data.get("input_tokens") or 0)
        out_t += int(data.get("output_tokens") or 0)
        cache_t += int(data.get("cache_read_tokens") or 0)
    return in_t, out_t, cache_t


def classify(reward: float | None, exc: dict | None, rate_limited: bool) -> str:
    if reward == 1.0:
        return "passed"
    if exc:
        return "errored"
    if rate_limited:
        return "quota"
    if reward == 0.0:
        return "failed"
    return "unknown"


def walk_trials(job_dir: Path) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    for trial_dir in sorted(p for p in job_dir.iterdir() if p.is_dir()):
        result = _safe_load(trial_dir / "result.json")
        if not isinstance(result, dict):
            continue

        # Trim the per-trial hash suffix to get the bare task name.
        task = result.get("task_name") or trial_dir.name.rsplit("__", 1)[0]
        if task.startswith("terminal-bench/"):
            task = task[len("terminal-bench/"):]

        verifier = (result.get("verifier_result") or {}).get("rewards") or {}
        reward = verifier.get("reward")
        exc = result.get("exception_info")
        agent_log = trial_dir / "agent" / "ignis.txt"
        rate_limited = (
            agent_log.exists()
            and "rate_limit_reached_error" in agent_log.read_text(errors="ignore")
        )

        n_in, n_out, n_cache = _sum_usage(trial_dir / "agent" / "ignis-projects")
        cache_rate = round(n_cache / n_in, 3) if n_in else ""

        rows.append({
            "task": task,
            "trial": trial_dir.name,
            "bucket": classify(reward, exc, rate_limited),
            "reward": "" if reward is None else reward,
            "exception": (exc or {}).get("exception_type", "") if exc else "",
            "rate_limited": "true" if rate_limited else "false",
            "duration_seconds": _wall_seconds(result.get("started_at"), result.get("finished_at")) or "",
            "n_input_tokens": n_in or "",
            "n_output_tokens": n_out or "",
            "n_cache_read_tokens": n_cache or "",
            "cache_hit_rate": cache_rate,
            "started_at": result.get("started_at", ""),
            "finished_at": result.get("finished_at", ""),
        })
    return rows


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description="Aggregate a harbor TB2 job dir into a per-trial CSV.")
    parser.add_argument("job_dir", type=Path, help="Path to the harbor job directory (parent of trial subdirs).")
    parser.add_argument("-o", "--output", type=Path, default=Path("results.csv"))
    args = parser.parse_args(argv)

    if not args.job_dir.is_dir():
        print(f"error: {args.job_dir} is not a directory", file=sys.stderr)
        return 2

    rows = walk_trials(args.job_dir)
    if not rows:
        print(f"warning: no trial dirs with result.json found under {args.job_dir}", file=sys.stderr)

    fields = list(rows[0].keys()) if rows else []
    with args.output.open("w", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=fields)
        writer.writeheader()
        writer.writerows(rows)

    # One-line summary so the script is self-documenting.
    from collections import Counter
    buckets = Counter(r["bucket"] for r in rows)
    print(f"{len(rows)} trials → {args.output} ({dict(buckets)})")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
