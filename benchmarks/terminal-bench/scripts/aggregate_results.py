"""
Aggregate a harbor TB2 job directory into a per-trial CSV.

Usage:
    python aggregate_results.py <job_dir> [-o results.csv]

The job dir is whatever `harbor run -o ...` produced — it contains one subdir
per trial (`<task>__<hash>/`) plus the top-level result.json. The script reads
each trial's result.json and synced ignis log, classifies the trial into one of
three terminal buckets, and writes a CSV with one row per trial.

The bucket axis is intentionally narrow: `passed` / `failed` / `errored`. The
sub-cause of an `errored` row (harness timeout, rate-limit, stream drop,
nothing-at-all) lives in the `exception` column — see the column note below.

Columns:
    task                 — task name (no trial hash)
    trial                — full trial dir name (`<task>__<hash>`)
    bucket               — passed | failed | errored
    reward               — 1.0 / 0.0 / "" (no verifier result)
    exception            — subtype tag. For `failed`: `TimedOut` (agent burned
                           its budget) or "" (verifier ran and rejected). For
                           `errored`: `VerifierTimeoutError` /
                           `NonZeroAgentExitCodeError` (and other harness
                           exception types) / `ConnectionDropped` /
                           `RateLimited` / `Unknown`. For `passed`: "".
    rate_limited         — true if the agent log shows kimi's rate_limit_reached_error
    duration_seconds     — wall time from harbor's timestamps
    n_input_tokens       — sum from synced ignis-projects/**/<session>.usage.json
    n_output_tokens
    n_cache_read_tokens
    cache_hit_rate       — n_cache_read_tokens / n_input_tokens (rounded to 3 dp)
    started_at           — ISO 8601
    finished_at          — ISO 8601

Bucket precedence:
    reward == 1.0                                   → passed   (recovery counts)
    AgentTimeoutError                               → failed/TimedOut
    reward == 0.0 with no other markers             → failed   (verifier rejected)
    everything else (other exc / stream drop / rate-limit / missing-reward)
                                                     → errored  (exception subtype carries the why)
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
    """Sum input/output/cache_read across every ignis *.usage.json in the tree.

    cache_write_tokens is intentionally not aggregated — only cache_read drives
    the cache_hit_rate metric the report cares about.
    """
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


def classify(
    reward: float | None,
    exc: dict | None,
    rate_limited: bool,
    stream_dropped: bool,
) -> str:
    """Bucket a trial into passed / failed / errored.

    `failed` is a model miss — either the verifier ran and rejected the work,
    OR the agent burned through its full timeout budget (task author's
    `timeout_sec` × our `--agent-timeout-multiplier`, ×2.0 by default) without
    finishing. A stronger model finishes the same task faster, so timeouts are
    a capability signal, not infra failure.

    `errored` is reserved for cases where the model never got a fair shot:
    network drops, provider rate-limits, harness crashes, sandbox exits.
    """
    if reward == 1.0:
        return "passed"
    if (exc or {}).get("exception_type") == "AgentTimeoutError":
        return "failed"
    # Bare verifier-rejection only. Any other signal that the agent didn't get
    # a clean shot (other exc / stream drop / rate-limit) pushes into errored.
    if reward == 0.0 and not (exc or stream_dropped or rate_limited):
        return "failed"
    return "errored"


# Markers in the agent's stdout that indicate a provider-side stream drop —
# the model API closed the connection mid-response, so the agent never got a
# complete turn. The bare "[Error in stream:" catches our own retry-loop's
# final-attempt error too (when MAX_STREAM_RETRIES is hit). Kept narrow on
# purpose: false positives reclassify real model misses as infra errors and
# inflate the resolved% metric.
_STREAM_DROP_MARKERS = (
    "connection closed before message completed",
    "error sending request",
    "[Error in stream:",
)


def _agent_log_indicates_stream_drop(path: Path) -> bool:
    """Stream-scan the agent log for any stream-drop marker; tolerant of multi-GB logs."""
    if not path.exists():
        return False
    try:
        size = path.stat().st_size
    except OSError:
        return False
    chunk = 1 << 20
    needles = [m.encode() for m in _STREAM_DROP_MARKERS]
    carry_len = max(len(n) for n in needles)
    try:
        with path.open("rb") as fh:
            carry = b""
            while True:
                buf = fh.read(chunk)
                if not buf:
                    return False
                window = carry + buf
                if any(n in window for n in needles):
                    return True
                carry = window[-carry_len:]
    except OSError:
        return False


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
        stream_dropped = _agent_log_indicates_stream_drop(agent_log)

        n_in, n_out, n_cache = _sum_usage(trial_dir / "agent" / "ignis-projects")
        cache_rate = round(n_cache / n_in, 3) if n_in else ""

        # Bucket and exception subtype. The bucket is passed/failed/errored —
        # the Exception column distinguishes the sub-cause. For `failed`, the
        # only sub-type is `TimedOut` (agent burned its budget) vs blank (the
        # verifier ran and rejected). For `errored`, the order mirrors
        # `classify`'s precedence: harness exception > stream drop > rate-limit
        # > unknown (no reward and nothing else to say).
        exc_type = (exc or {}).get("exception_type", "")
        bucket = classify(reward, exc, rate_limited, stream_dropped and reward != 1.0)
        if bucket == "passed":
            exception_label = ""
        elif bucket == "failed":
            exception_label = "TimedOut" if exc_type == "AgentTimeoutError" else ""
        elif exc:
            exception_label = exc_type
        elif stream_dropped and reward != 1.0:
            exception_label = "ConnectionDropped"
        elif rate_limited:
            exception_label = "RateLimited"
        else:
            exception_label = "Unknown"

        rows.append({
            "task": task,
            "trial": trial_dir.name,
            "bucket": bucket,
            "reward": "" if reward is None else reward,
            "exception": exception_label,
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
