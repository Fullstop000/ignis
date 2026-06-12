"""
Package a harbor bench job dir into a single .tar.gz for archival.

Usage:
    python bundle_traces.py <job_dir> -o <out>.tar.gz [--name <slug>]
        [--runaway-bytes 52428800]   # 50 MB; trials whose agent/ignis.txt
                                     # exceeds this are skipped entirely
                                     # (runaway-output trials, per the
                                     # user-set TB-2 policy).
        [--companion-csv path.csv]   # optional: include a per-trial CSV
                                     # already produced by aggregate_results

Per trial we keep these files (with all text content secret-redacted):
    result.json
    agent/ignis.txt
    agent/ignis-projects/**/*.usage.json
    agent/ignis-projects/**/*.jsonl
    verifier/reward.txt
    verifier/test-stdout.txt

We drop, because they re-leak the in-sandbox config.toml with the
provider api_key, brave key, etc.:
    agent/setup/
    job.log (lives at job_dir root)

Top-level files copied (also redacted): config.json (harbor's own job
config — has no secrets, but redaction is cheap defense-in-depth).

Output structure inside the tarball:
    <name>/
        README.md           — what this is, redaction policy, skip list
        config.json         — harbor config, redacted
        results.csv         — copied if --companion-csv given
        trials/<task>__<hash>/...

The README also lists which trials were skipped and why, so the archive
is self-describing.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import re
import shutil
import sys
import tarfile
import tempfile
from pathlib import Path


# Same regex set as generate_report._redact_secrets — kept in sync.
_SECRET_PATTERNS = [
    (re.compile(rb"sk-[A-Za-z0-9-]{24,}"), b"sk-REDACTED"),
    (re.compile(rb"ghp_[A-Za-z0-9]{20,}"), b"ghp_REDACTED"),
    (re.compile(rb"gho_[A-Za-z0-9]{20,}"), b"gho_REDACTED"),
    (re.compile(rb"hf_[A-Za-z0-9]{20,}"), b"hf_REDACTED"),
    (re.compile(rb"AKIA[0-9A-Z]{16}"), b"AKIA-REDACTED"),
    (re.compile(rb"BSA[A-Za-z0-9]{20,}"), b"BSA-REDACTED"),
    (re.compile(rb"dtn_[A-Za-z0-9]{20,}"), b"dtn_REDACTED"),
    (re.compile(rb"sk-cp-[A-Za-z0-9_-]{20,}"), b"sk-cp-REDACTED"),
    (re.compile(rb"sk-kimi-[A-Za-z0-9-]{20,}"), b"sk-kimi-REDACTED"),
    # Value-agnostic credential shapes: an agent that `cat`s ~/.ignis/config.toml
    # (or whose error echoes the request) surfaces a key whose *value* has no
    # recognizable prefix (e.g. a bare-UUID provider token). Redact by the
    # surrounding indicator instead — the config `api_key = "…"` line and any
    # `Authorization: Bearer …` header.
    (re.compile(rb'api_key\s*=\s*"[^"]*"'), b'api_key = "REDACTED"'),
    (re.compile(rb"(?i)bearer\s+[A-Za-z0-9._-]{20,}"), b"Bearer REDACTED"),
]


def _redact_bytes(data: bytes) -> bytes:
    """Apply every secret pattern in order; idempotent."""
    for rx, repl in _SECRET_PATTERNS:
        data = rx.sub(repl, data)
    return data


def _copy_redacted(src: Path, dst: Path) -> int:
    """Copy a file, redacting any secret-shaped substrings.

    Streams in 4 MB chunks with carry-over to handle patterns that straddle
    a chunk boundary — same idea as generate_report._file_contains.
    """
    dst.parent.mkdir(parents=True, exist_ok=True)
    chunk = 4 << 20
    max_pat_len = max(len(p.pattern) for p, _ in _SECRET_PATTERNS) + 32
    n = 0
    with src.open("rb") as fin, dst.open("wb") as fout:
        carry = b""
        while True:
            buf = fin.read(chunk)
            if not buf:
                if carry:
                    fout.write(_redact_bytes(carry))
                    n += len(carry)
                break
            window = carry + buf
            # Hold back the tail to keep cross-chunk patterns intact.
            emit = window[:-max_pat_len] if len(window) > max_pat_len else b""
            carry = window[len(emit):]
            if emit:
                redacted = _redact_bytes(emit)
                fout.write(redacted)
                n += len(redacted)
    return n


def _filter_trial(
    trial_dir: Path,
    out_dir: Path,
    runaway_bytes: int,
) -> tuple[bool, str]:
    """Copy keep-list files for one trial into `out_dir`.

    Returns (kept, reason). kept=False means we skipped this trial; reason
    is a short tag for the README skip list.
    """
    agent_log = trial_dir / "agent" / "ignis.txt"
    if agent_log.exists():
        try:
            size = agent_log.stat().st_size
        except OSError:
            size = 0
        if size > runaway_bytes:
            return False, f"runaway-log ({size / 1024 / 1024:.0f}MB)"

    out_dir.mkdir(parents=True, exist_ok=True)

    # result.json (required to identify the trial).
    result = trial_dir / "result.json"
    if not result.exists():
        return False, "no-result.json"
    _copy_redacted(result, out_dir / "result.json")

    if agent_log.exists():
        _copy_redacted(agent_log, out_dir / "agent" / "ignis.txt")

    # ignis-projects/<session>.{usage,jsonl}.json — full session transcripts.
    proj = trial_dir / "agent" / "ignis-projects"
    if proj.exists():
        for f in proj.rglob("*"):
            if f.is_file() and (f.name.endswith(".usage.json") or f.name.endswith(".jsonl")):
                rel = f.relative_to(proj)
                _copy_redacted(f, out_dir / "agent" / "ignis-projects" / rel)

    # verifier/
    v = trial_dir / "verifier"
    if v.exists():
        for f in ("reward.txt", "test-stdout.txt"):
            if (v / f).exists():
                _copy_redacted(v / f, out_dir / "verifier" / f)

    return True, ""


def _readme(
    name: str,
    job_dir: Path,
    kept: list[str],
    skipped: list[tuple[str, str]],
    has_csv: bool,
) -> str:
    skip_lines = "\n".join(f"- `{t}` — {r}" for t, r in skipped) if skipped else "_(none)_"
    return f"""# {name}

Archived traces from the harbor job at:
```
{job_dir}
```

Bundled {dt.datetime.now().isoformat(timespec="seconds")}.

## What's included
- `config.json` — harbor job config (timeout multipliers, env, retry policy).
- `results.csv` — per-trial aggregate (bucket, reward, exception, tokens, duration). {"Present." if has_csv else "_Not included for this bundle._"}
- `trials/<task>__<hash>/` — one directory per included trial:
  - `result.json` — harbor verdict, exception_info, started/finished.
  - `agent/ignis.txt` — the agent's stdout log.
  - `agent/ignis-projects/**/*.usage.json` — per-turn token counts.
  - `agent/ignis-projects/**/*.jsonl` — full per-session transcripts (replayable).
  - `verifier/reward.txt`, `verifier/test-stdout.txt` — verifier output.

## Redaction
Every text file is streamed through these regex replacements before bundling:

| Pattern | Replacement |
|---|---|
| `sk-[A-Za-z0-9-]{{24,}}` | `sk-REDACTED` |
| `ghp_[A-Za-z0-9]{{20,}}` | `ghp_REDACTED` |
| `gho_[A-Za-z0-9]{{20,}}` | `gho_REDACTED` |
| `hf_[A-Za-z0-9]{{20,}}` | `hf_REDACTED` |
| `AKIA[0-9A-Z]{{16}}` | `AKIA-REDACTED` |
| `BSA[A-Za-z0-9]{{20,}}` | `BSA-REDACTED` |
| `dtn_[A-Za-z0-9]{{20,}}` | `dtn_REDACTED` |
| `sk-cp-[A-Za-z0-9_-]{{20,}}` | `sk-cp-REDACTED` |
| `sk-kimi-[A-Za-z0-9-]{{20,}}` | `sk-kimi-REDACTED` |
| `api_key\\s*=\\s*"[^"]*"` | `api_key = "REDACTED"` |
| `(?i)bearer\\s+[A-Za-z0-9._-]{{20,}}` | `Bearer REDACTED` |

The last two are value-agnostic: they catch a config-dumped key whose value has no recognizable prefix (e.g. a bare-UUID provider token an agent surfaces by `cat`-ing `~/.ignis/config.toml` mid-task). These are mostly the same patterns the `/ship` pre-push secret scan uses. The list is conservative — it may catch task-planted "secret" strings used by tasks like `vulnerable-secret`. That's intentional defense-in-depth for a private archive.

The original `job.log` (echoes the full in-sandbox `~/.ignis/config.toml`, including the provider api_key) and `agent/setup/` (install logs that echo the same config write) are dropped entirely, not redacted.

## Counts
- Trials included: **{len(kept)}**
- Trials skipped: **{len(skipped)}**

## Skip list
{skip_lines}
"""


def main(argv: list[str]) -> int:
    p = argparse.ArgumentParser(description="Package a harbor bench job into a tarball for archival.")
    p.add_argument("job_dir", type=Path)
    p.add_argument("-o", "--output", type=Path, required=True, help="Output .tar.gz path.")
    p.add_argument("--name", default=None, help="Top-level dir name inside the tarball (defaults to job_dir basename).")
    p.add_argument(
        "--runaway-bytes", type=int, default=50 * 1024 * 1024,
        help="Skip trials whose agent/ignis.txt exceeds this size (default 50MB).",
    )
    p.add_argument("--companion-csv", type=Path, default=None,
                   help="Copy this CSV in as results.csv (optional).")
    args = p.parse_args(argv)

    if not args.job_dir.is_dir():
        print(f"error: {args.job_dir} is not a directory", file=sys.stderr)
        return 2

    name = args.name or args.job_dir.name
    with tempfile.TemporaryDirectory(prefix="bundle_traces_") as tmpd:
        stage = Path(tmpd) / name
        stage.mkdir()
        kept: list[str] = []
        skipped: list[tuple[str, str]] = []

        for trial in sorted(p for p in args.job_dir.iterdir() if p.is_dir()):
            ok, reason = _filter_trial(trial, stage / "trials" / trial.name, args.runaway_bytes)
            if ok:
                kept.append(trial.name)
            else:
                skipped.append((trial.name, reason))

        # Top-level config.json (job-level, no secrets — still redacted as a cheap belt+braces).
        cfg = args.job_dir / "config.json"
        if cfg.exists():
            _copy_redacted(cfg, stage / "config.json")

        # Optional companion CSV.
        if args.companion_csv:
            shutil.copyfile(args.companion_csv, stage / "results.csv")

        (stage / "README.md").write_text(_readme(name, args.job_dir, kept, skipped, args.companion_csv is not None))

        args.output.parent.mkdir(parents=True, exist_ok=True)
        with tarfile.open(args.output, "w:gz", compresslevel=6) as tar:
            tar.add(stage, arcname=name)
        size = args.output.stat().st_size
        print(f"wrote {args.output} ({size / 1024 / 1024:.1f} MB) — kept {len(kept)}, skipped {len(skipped)}")
        for t, r in skipped:
            print(f"  skipped {t}: {r}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
