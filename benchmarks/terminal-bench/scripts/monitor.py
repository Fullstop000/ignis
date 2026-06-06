#!/usr/bin/env python3
"""Live status for a TB harbor run.

Usage:
    monitor.py                       # snapshot, newest run dir under ./runs/
    monitor.py --watch               # repaint every 5s
    monitor.py runs/<dir>            # specific run
    monitor.py runs/<dir> --watch
    monitor.py --interval 2          # change watch period (default 5s)
"""
from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

ANSI_RE = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
SPINNER_LINE_RE = re.compile(
    r"(?P<elapsed>\d+:\d+:\d+)\s+(?P<task>[A-Za-z0-9._\-]+__\w+):\s+running agent"
)


def strip_ansi(s: str) -> str:
    return ANSI_RE.sub("", s)


def newest_run_dir(root: Path) -> Path | None:
    if not root.is_dir():
        return None
    subdirs = [p for p in root.iterdir() if p.is_dir()]
    subdirs.sort(key=lambda p: p.stat().st_mtime, reverse=True)
    return subdirs[0] if subdirs else None


def resolve_dirs(arg: Path | None) -> tuple[Path, Path]:
    """Return (run_dir, job_dir). run_dir is what was passed (or newest); job_dir is
    the one inside it that has lock.json. If the passed path already has lock.json,
    treat it as the job_dir and the parent as the run_dir."""
    runs_root = Path(__file__).resolve().parent.parent / "runs"
    base = arg if arg else newest_run_dir(runs_root)
    if base is None or not base.exists():
        sys.exit(f"monitor: no run dir found under {runs_root}")
    if (base / "lock.json").exists():
        return base.parent, base
    inner = [p for p in base.iterdir() if p.is_dir() and (p / "lock.json").exists()]
    if not inner:
        sys.exit(f"monitor: no job dir with lock.json under {base}")
    return base, inner[0]


def find_harbor_pid(run_output: str) -> int | None:
    try:
        out = subprocess.run(
            ["pgrep", "-af", "harbor run"], capture_output=True, text=True
        ).stdout
    except FileNotFoundError:
        return None
    for line in out.splitlines():
        # match the harbor process whose -o argument is this run dir
        if run_output in line and "harbor run" in line:
            return int(line.split()[0])
    return None


def fmt_dur(seconds: float) -> str:
    s = int(seconds)
    h, rem = divmod(s, 3600)
    m, s = divmod(rem, 60)
    if h:
        return f"{h}h {m:02d}m {s:02d}s"
    if m:
        return f"{m}m {s:02d}s"
    return f"{s}s"


def parse_spinner_inflight(top_log: Path, limit_lines: int = 200) -> list[tuple[str, str]]:
    """Scan the tail of the top-level run log for the most recent
    `task__xxx: running agent` lines and return (task, elapsed) pairs."""
    if not top_log.exists():
        return []
    # tail-bytes — cheap and bounded
    size = top_log.stat().st_size
    with open(top_log, "rb") as f:
        f.seek(max(0, size - 200_000))
        chunk = f.read().decode("utf-8", errors="replace")
    out: dict[str, str] = {}
    for raw in chunk.splitlines()[-limit_lines:]:
        line = strip_ansi(raw)
        m = SPINNER_LINE_RE.search(line)
        if m:
            out[m.group("task")] = m.group("elapsed")
    return list(out.items())


def load_lock(job_dir: Path) -> dict:
    with open(job_dir / "lock.json") as f:
        return json.load(f)


def classify_trial(trial_dir: Path) -> dict:
    """Return verdict + duration for a finished trial, or {'state': 'pending'}."""
    rj = trial_dir / "result.json"
    if not rj.exists():
        return {"state": "pending"}
    try:
        r = json.load(open(rj))
    except json.JSONDecodeError:
        return {"state": "pending"}  # mid-write
    finished = r.get("finished_at")
    if not finished:
        return {"state": "pending"}
    started = r.get("started_at")
    dur = None
    if started and finished:
        try:
            t0 = dt.datetime.fromisoformat(started.rstrip("Z"))
            t1 = dt.datetime.fromisoformat(finished.rstrip("Z"))
            dur = (t1 - t0).total_seconds()
        except ValueError:
            pass
    # Reward always wins: the verifier may run AFTER an agent timeout and still
    # find the work on disk correct. Check reward first; only if there's no
    # reward do we fall back to the exception bucket.
    rw = ((r.get("verifier_result") or {}).get("rewards") or {}).get("reward")
    if rw is None:
        rw = (r.get("rewards") or {}).get("reward")
    if rw is not None and rw >= 1.0:
        return {
            "state": "resolved",
            "reward": rw,
            "dur": dur,
            "task": r.get("task_name", trial_dir.name),
        }
    exc = r.get("exception_info")
    if exc:
        return {
            "state": "errored",
            "reason": exc.get("exception_type") or exc.get("class") or "exception",
            "dur": dur,
            "task": r.get("task_name", trial_dir.name),
        }
    return {
        "state": "failed",
        "reward": rw,
        "dur": dur,
        "task": r.get("task_name", trial_dir.name),
    }


def collect(job_dir: Path) -> dict:
    trial_dirs = sorted(p for p in job_dir.iterdir() if p.is_dir())
    classified = [(p.name, classify_trial(p)) for p in trial_dirs]
    return {
        "trials": classified,
        "total_dirs": len(classified),
    }


def render(run_dir: Path, job_dir: Path) -> str:
    lock = load_lock(job_dir)
    invocation = lock.get("invocation", [])
    # extract model + env from invocation
    def arg_after(flag):
        try:
            i = invocation.index(flag)
            return invocation[i + 1]
        except (ValueError, IndexError):
            return None

    model = arg_after("-m") or "?"
    env = arg_after("-e") or "?"
    n = arg_after("-n") or "?"
    dataset = arg_after("-d") or "?"
    n_total = len(lock.get("trials", []))

    created = lock.get("created_at")
    elapsed_s = None
    started_str = "—"
    if created:
        try:
            t0 = dt.datetime.fromisoformat(created.rstrip("Z")).replace(tzinfo=dt.timezone.utc)
            elapsed_s = (dt.datetime.now(dt.timezone.utc) - t0).total_seconds()
            started_str = t0.astimezone().strftime("%Y-%m-%d %H:%M:%S")
        except ValueError:
            started_str = created

    run_output_arg = arg_after("-o") or ""
    pid = find_harbor_pid(run_output_arg) if run_output_arg else None

    data = collect(job_dir)
    states = [c["state"] for _, c in data["trials"]]
    resolved = states.count("resolved")
    failed = states.count("failed")
    errored = states.count("errored")
    pending_on_disk = states.count("pending")
    done = resolved + failed + errored
    not_done = n_total - done

    # Live in-flight info from spinner log (sibling of the run dir, not inside)
    top_log = run_dir.parent / f"{run_dir.name}.log"
    inflight = parse_spinner_inflight(top_log)

    lines = []
    lines.append(
        f"TB · {dataset} · {model} · {env} · n={n}"
    )
    pid_str = f"PID {pid} (up)" if pid else "PID — (not running)"
    el_str = f"elapsed {fmt_dur(elapsed_s)}" if elapsed_s else "elapsed —"
    lines.append(f"{pid_str} · started {started_str} · {el_str}")
    lines.append("")
    rate = (resolved / done * 100) if done else 0.0
    lines.append(
        f"Progress: {done} / {n_total}   "
        f"(✓ {resolved}  ✗ {failed}  ! {errored})   "
        f"pending {not_done}   "
        f"resolve-rate {rate:.1f}%"
    )
    lines.append("")
    if inflight:
        lines.append(f"In flight ({len(inflight)}):")
        for task, el in inflight:
            short = task.split("__", 1)[0]
            lines.append(f"  ▶ {short:<36} {el}")
        lines.append("")
    finished_with_dur = [
        (name, c) for name, c in data["trials"] if c["state"] in ("resolved", "failed", "errored")
    ]
    if finished_with_dur:
        # Order by trial-dir mtime (proxy for completion order)
        finished_with_dur.sort(
            key=lambda nc: (job_dir / nc[0]).stat().st_mtime, reverse=True
        )
        lines.append(f"Recent verdicts (newest first, up to 20):")
        sym = {"resolved": "✓", "failed": "✗", "errored": "!"}
        for name, c in finished_with_dur[:20]:
            short = c.get("task", name).removeprefix("terminal-bench/")
            d = fmt_dur(c["dur"]) if c.get("dur") else "?"
            verdict = c["state"]
            if c["state"] == "errored":
                verdict = f"errored ({c.get('reason','?')})"
            lines.append(f"  {sym[c['state']]} {short:<36} {verdict:<25} {d}")
    else:
        lines.append("Recent verdicts: none yet")
    return "\n".join(lines)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("run_dir", nargs="?", type=Path)
    ap.add_argument("--watch", action="store_true", help="repaint until Ctrl-C")
    ap.add_argument("--interval", type=float, default=5.0)
    args = ap.parse_args()
    run_dir, job_dir = resolve_dirs(args.run_dir)
    if not args.watch:
        print(render(run_dir, job_dir))
        return
    try:
        while True:
            sys.stdout.write("\x1b[2J\x1b[H")  # clear screen, home cursor
            sys.stdout.write(render(run_dir, job_dir))
            sys.stdout.write("\n\n(refresh: " + f"{args.interval:.0f}s · Ctrl-C to quit)\n")
            sys.stdout.flush()
            time.sleep(args.interval)
    except KeyboardInterrupt:
        print()


if __name__ == "__main__":
    main()
