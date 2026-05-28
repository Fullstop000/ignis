"""
Generate a self-contained report.html from a harbor TB2 job directory.

Usage:
    python generate_report.py <job_dir> [-o report.html]

The HTML is single-file, no external dependencies, no network calls. Open it
directly in a browser.

Sections:
  1. Headline — real-attempt pass rate, token totals, bucket breakdown.
  2. Per-trial table — sortable, with a drill-down per row showing the full
     agent log, tool-call histogram, and verifier output.
"""

from __future__ import annotations

import argparse
import datetime as dt
import html
import json
import re
import sys
from collections import Counter
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

# Regex matching the tool-call markers ignis tees into ignis.txt:
#   >>> [Tool: <name> (<call_id>)] args: {...}
_TOOL_RE = re.compile(r"^>>> \[Tool: (?P<name>[A-Za-z0-9_]+) \(")


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
    in_t = out_t = cache_t = 0
    if not usage_dir.exists():
        return in_t, out_t, cache_t
    for f in usage_dir.rglob("*.usage.json"):
        data = _safe_load(f)
        if isinstance(data, dict):
            in_t += int(data.get("input_tokens") or 0)
            out_t += int(data.get("output_tokens") or 0)
            cache_t += int(data.get("cache_read_tokens") or 0)
    return in_t, out_t, cache_t


def _classify(reward: float | None, exc: dict | None, rate_limited: bool) -> str:
    if reward == 1.0:
        return "passed"
    if exc:
        return "errored"
    if rate_limited:
        return "quota"
    if reward == 0.0:
        return "failed"
    return "unknown"


def _parse_tool_calls(log: str) -> Counter[str]:
    counter: Counter[str] = Counter()
    for line in log.splitlines():
        m = _TOOL_RE.match(line)
        if m:
            counter[m.group("name")] += 1
    return counter


def _tail_lines(text: str, n: int) -> str:
    lines = text.splitlines()
    return "\n".join(lines[-n:])


@dataclass
class Trial:
    task: str
    trial: str
    bucket: str
    reward: float | None
    exception: str
    rate_limited: bool
    duration_seconds: float | None
    n_input_tokens: int
    n_output_tokens: int
    n_cache_tokens: int
    cache_hit_rate: float | None
    started_at: str
    finished_at: str
    log_full: str
    tool_calls: Counter[str] = field(default_factory=Counter)
    verifier_reward_raw: str = ""
    verifier_test_tail: str = ""

    @property
    def tool_call_total(self) -> int:
        return sum(self.tool_calls.values())


def walk_trials(job_dir: Path) -> list[Trial]:
    trials: list[Trial] = []
    for trial_dir in sorted(p for p in job_dir.iterdir() if p.is_dir()):
        result = _safe_load(trial_dir / "result.json")
        if not isinstance(result, dict):
            continue

        task = result.get("task_name") or trial_dir.name.rsplit("__", 1)[0]
        if task.startswith("terminal-bench/"):
            task = task[len("terminal-bench/") :]

        verifier = (result.get("verifier_result") or {}).get("rewards") or {}
        reward = verifier.get("reward")
        exc = result.get("exception_info")
        agent_log_path = trial_dir / "agent" / "ignis.txt"
        log_text = agent_log_path.read_text(errors="ignore") if agent_log_path.exists() else ""
        rate_limited = "rate_limit_reached_error" in log_text

        n_in, n_out, n_cache = _sum_usage(trial_dir / "agent" / "ignis-projects")
        cache_rate = (n_cache / n_in) if n_in else None

        verifier_reward_p = trial_dir / "verifier" / "reward.txt"
        verifier_test_p = trial_dir / "verifier" / "test-stdout.txt"
        verifier_reward_raw = verifier_reward_p.read_text(errors="ignore") if verifier_reward_p.exists() else ""
        verifier_test_tail = (
            _tail_lines(verifier_test_p.read_text(errors="ignore"), 30) if verifier_test_p.exists() else ""
        )

        trials.append(
            Trial(
                task=task,
                trial=trial_dir.name,
                bucket=_classify(reward, exc, rate_limited),
                reward=reward,
                exception=(exc or {}).get("exception_type", "") if exc else "",
                rate_limited=rate_limited,
                duration_seconds=_wall_seconds(result.get("started_at"), result.get("finished_at")),
                n_input_tokens=n_in,
                n_output_tokens=n_out,
                n_cache_tokens=n_cache,
                cache_hit_rate=cache_rate,
                started_at=result.get("started_at", ""),
                finished_at=result.get("finished_at", ""),
                log_full=log_text,
                tool_calls=_parse_tool_calls(log_text),
                verifier_reward_raw=verifier_reward_raw,
                verifier_test_tail=verifier_test_tail,
            )
        )
    return trials


# ─── HTML rendering ─────────────────────────────────────────────────────────


_CSS = """
* { box-sizing: border-box; }
body { font: 14px/1.5 -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; margin: 0; padding: 2rem; color: #1a1a1a; background: #fafafa; max-width: 1280px; margin: 0 auto; }
h1 { margin: 0 0 0.5rem; font-size: 1.6rem; }
h2 { margin: 2.5rem 0 1rem; font-size: 1.2rem; border-bottom: 1px solid #ddd; padding-bottom: 0.4rem; }
h3 { margin: 1.5rem 0 0.5rem; font-size: 0.95rem; color: #444; }
.meta { color: #666; margin-bottom: 1.5rem; }
.stats { display: grid; grid-template-columns: repeat(auto-fit, minmax(160px, 1fr)); gap: 0.75rem; margin-bottom: 1.5rem; }
.stat { background: white; padding: 0.75rem 1rem; border-radius: 6px; border: 1px solid #e6e6e6; }
.stat .label { font-size: 0.75rem; color: #888; text-transform: uppercase; letter-spacing: 0.05em; }
.stat .value { font-size: 1.5rem; font-weight: 600; margin-top: 0.2rem; }
.bar { display: flex; height: 24px; border-radius: 4px; overflow: hidden; margin: 0.5rem 0 1rem; }
.bar > span { display: flex; align-items: center; justify-content: center; color: white; font-size: 0.75rem; font-weight: 600; padding: 0 0.5rem; min-width: 1.5rem; }
.bar .passed  { background: #2e8b57; }
.bar .failed  { background: #c66; }
.bar .errored { background: #d99650; }
.bar .quota   { background: #888; }
table { width: 100%; border-collapse: collapse; background: white; border-radius: 4px; overflow: hidden; border: 1px solid #e6e6e6; font-size: 13px; }
th, td { padding: 0.4rem 0.6rem; text-align: left; border-bottom: 1px solid #efefef; }
th { background: #f4f4f4; cursor: pointer; user-select: none; white-space: nowrap; }
th:hover { background: #ebebeb; }
th.num, td.num { text-align: right; font-variant-numeric: tabular-nums; }
tr.passed { background: #f0f9f3; }
tr.failed { background: #fdf3f3; }
tr.errored { background: #fef6ec; }
tr.quota { background: #f4f4f4; color: #777; }
.bucket-tag { display: inline-block; padding: 1px 8px; border-radius: 10px; font-size: 0.7rem; font-weight: 600; color: white; }
.bucket-passed  { background: #2e8b57; }
.bucket-failed  { background: #c66; }
.bucket-errored { background: #d99650; }
.bucket-quota   { background: #888; }
details { background: #fafbfc; border-top: 1px solid #efefef; padding: 0.5rem 1rem; }
details summary { cursor: pointer; color: #4a90d9; font-size: 0.85rem; }
details[open] summary { font-weight: 600; }
.drill-grid { display: grid; grid-template-columns: 1fr 1fr; gap: 1rem; margin-top: 0.6rem; }
.drill-grid > div { background: white; padding: 0.5rem 0.75rem; border-radius: 4px; border: 1px solid #efefef; }
.drill-grid h4 { margin: 0 0 0.35rem; font-size: 0.78rem; color: #666; text-transform: uppercase; letter-spacing: 0.04em; }
pre { font-family: "SF Mono", Consolas, monospace; font-size: 11px; background: #1e1e1e; color: #ddd; padding: 0.6rem; border-radius: 4px; overflow: auto; max-height: 70vh; line-height: 1.4; margin: 0; white-space: pre-wrap; overflow-wrap: anywhere; }
.tool-bar { display: flex; align-items: center; gap: 0.5rem; font-size: 12px; }
.tool-bar .bar-track { flex: 1; height: 12px; background: #eee; border-radius: 6px; overflow: hidden; }
.tool-bar .bar-fill  { height: 100%; background: #4a90d9; }
"""

_JS = """
function sortTable(th) {
  const tbl = th.closest('table');
  const colIdx = Array.from(th.parentElement.children).indexOf(th);
  const numeric = th.classList.contains('num');
  const asc = th.dataset.sort !== 'asc';
  th.parentElement.querySelectorAll('th').forEach(h => h.dataset.sort = '');
  th.dataset.sort = asc ? 'asc' : 'desc';
  const rows = Array.from(tbl.tBodies[0].rows);
  rows.sort((a, b) => {
    let av = a.cells[colIdx].dataset.sort ?? a.cells[colIdx].textContent.trim();
    let bv = b.cells[colIdx].dataset.sort ?? b.cells[colIdx].textContent.trim();
    if (numeric) {
      av = parseFloat(av) || 0;
      bv = parseFloat(bv) || 0;
      return asc ? av - bv : bv - av;
    }
    return asc ? av.localeCompare(bv) : bv.localeCompare(av);
  });
  rows.forEach(r => tbl.tBodies[0].appendChild(r));
}
"""


def _fmt_num(n: int | float | None) -> str:
    if n is None or n == "":
        return "—"
    if isinstance(n, float):
        return f"{n:,.0f}" if n >= 100 else f"{n:.1f}"
    return f"{n:,}"


def _fmt_pct(p: float | None) -> str:
    return f"{p * 100:.1f}%" if p is not None else "—"


def _fmt_dur(s: float | None) -> str:
    if s is None:
        return "—"
    if s < 60:
        return f"{s:.0f}s"
    return f"{s/60:.1f}m"


def _bucket_tag(b: str) -> str:
    return f'<span class="bucket-tag bucket-{b}">{b}</span>'


def _render_trial_drilldown(t: Trial) -> str:
    tool_total = t.tool_call_total
    if tool_total:
        top_tools = t.tool_calls.most_common(10)
        max_count = top_tools[0][1]
        tool_rows = "".join(
            f'<div class="tool-bar"><span style="min-width:110px">{html.escape(name)}</span>'
            f'<div class="bar-track"><div class="bar-fill" style="width:{count/max_count*100:.0f}%"></div></div>'
            f'<span style="min-width:30px;text-align:right">{count}</span></div>'
            for name, count in top_tools
        )
    else:
        tool_rows = '<div style="color:#999;font-size:12px">no tool calls parsed</div>'

    verifier_block = (
        f'<pre>{html.escape(t.verifier_test_tail) or "(no verifier output captured)"}</pre>'
    )
    reward_raw = html.escape(t.verifier_reward_raw.strip()) or "—"
    log_block = (
        f'<pre>{html.escape(t.log_full)}</pre>'
        if t.log_full else '<div style="color:#999;font-size:12px">(no agent log captured)</div>'
    )
    log_size = len(t.log_full)

    return f"""<details>
  <summary>drill-down · {tool_total} tool calls · verifier reward {reward_raw} · agent log {log_size:,} bytes</summary>
  <div class="drill-grid" style="margin-top:0.6rem">
    <div><h4>tool call counts</h4>{tool_rows}</div>
    <div><h4>verifier output (tail)</h4>{verifier_block}</div>
  </div>
  <h4 style="margin-top:0.8rem">agent log — full</h4>
  {log_block}
</details>"""


def _render_main_table(trials: list[Trial]) -> str:
    rows = []
    # Default sort: by bucket (passed first), then duration descending.
    bucket_order = {"passed": 0, "failed": 1, "errored": 2, "quota": 3, "unknown": 4}
    sorted_trials = sorted(
        trials, key=lambda t: (bucket_order.get(t.bucket, 9), -(t.duration_seconds or 0))
    )
    for t in sorted_trials:
        rows.append(f"""<tr class="{t.bucket}">
  <td>{html.escape(t.task)}</td>
  <td data-sort="{bucket_order.get(t.bucket, 9)}">{_bucket_tag(t.bucket)}</td>
  <td class="num" data-sort="{t.reward if t.reward is not None else -1}">{_fmt_num(t.reward) if t.reward is not None else "—"}</td>
  <td class="num" data-sort="{t.duration_seconds or 0}">{_fmt_dur(t.duration_seconds)}</td>
  <td class="num" data-sort="{t.n_input_tokens}">{_fmt_num(t.n_input_tokens) if t.n_input_tokens else "—"}</td>
  <td class="num" data-sort="{t.n_output_tokens}">{_fmt_num(t.n_output_tokens) if t.n_output_tokens else "—"}</td>
  <td class="num" data-sort="{t.cache_hit_rate or 0}">{_fmt_pct(t.cache_hit_rate)}</td>
  <td class="num" data-sort="{t.tool_call_total}">{t.tool_call_total or "—"}</td>
  <td>{html.escape(t.exception) or "—"}</td>
</tr>
<tr class="{t.bucket}"><td colspan="9" style="padding:0">{_render_trial_drilldown(t)}</td></tr>""")
    return f"""<table id="trials">
  <thead><tr>
    <th onclick="sortTable(this)">Task</th>
    <th onclick="sortTable(this)" class="num">Bucket</th>
    <th onclick="sortTable(this)" class="num">Reward</th>
    <th onclick="sortTable(this)" class="num">Duration</th>
    <th onclick="sortTable(this)" class="num">In tokens</th>
    <th onclick="sortTable(this)" class="num">Out tokens</th>
    <th onclick="sortTable(this)" class="num">Cache hit</th>
    <th onclick="sortTable(this)" class="num">Tool calls</th>
    <th onclick="sortTable(this)">Exception</th>
  </tr></thead>
  <tbody>{"".join(rows)}</tbody>
</table>"""


def render(trials: list[Trial], job_dir: Path) -> str:
    bucket_counts = Counter(t.bucket for t in trials)
    real_attempts = [t for t in trials if t.bucket in {"passed", "failed", "errored"}]
    passed = [t for t in trials if t.bucket == "passed"]
    real_pass_pct = (len(passed) / len(real_attempts) * 100) if real_attempts else 0.0
    total_in = sum(t.n_input_tokens for t in trials)
    total_out = sum(t.n_output_tokens for t in trials)
    total_cache = sum(t.n_cache_tokens for t in trials)
    cache_pct = (total_cache / total_in * 100) if total_in else 0.0

    # Stacked bar segments (passed, failed, errored, quota)
    bar_segs = []
    total = sum(bucket_counts.values()) or 1
    for b in ("passed", "failed", "errored", "quota"):
        c = bucket_counts.get(b, 0)
        if c:
            bar_segs.append(f'<span class="{b}" style="flex:{c}">{c} {b}</span>')

    return f"""<!DOCTYPE html>
<html><head><meta charset="utf-8"><title>ignis TB2 report</title>
<style>{_CSS}</style></head><body>
<h1>ignis · Terminal-Bench 2 report</h1>
<div class="meta">job dir: <code>{html.escape(str(job_dir))}</code> · generated {dt.datetime.now().isoformat(timespec="seconds")}</div>

<div class="stats">
  <div class="stat"><div class="label">Real-attempt pass rate</div><div class="value">{real_pass_pct:.1f}%</div><div class="meta">{len(passed)} / {len(real_attempts)} real trials</div></div>
  <div class="stat"><div class="label">Total trials</div><div class="value">{len(trials)}</div></div>
  <div class="stat"><div class="label">Input tokens</div><div class="value">{_fmt_num(total_in)}</div></div>
  <div class="stat"><div class="label">Output tokens</div><div class="value">{_fmt_num(total_out)}</div></div>
  <div class="stat"><div class="label">Cache hit rate</div><div class="value">{cache_pct:.1f}%</div></div>
</div>

<div class="bar">{"".join(bar_segs)}</div>

<h2>All trials</h2>
<p style="color:#666;font-size:0.85rem">Click a column header to sort. Expand drill-down on any row for the full agent log, tool-call counts, and verifier output.</p>
{_render_main_table(trials)}

<script>{_JS}</script>
</body></html>"""


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description="Render a harbor TB2 job dir into a single-file HTML report aimed at ignis improvement.")
    parser.add_argument("job_dir", type=Path)
    parser.add_argument("-o", "--output", type=Path, default=Path("report.html"))
    args = parser.parse_args(argv)

    if not args.job_dir.is_dir():
        print(f"error: {args.job_dir} is not a directory", file=sys.stderr)
        return 2

    trials = walk_trials(args.job_dir)
    if not trials:
        print(f"warning: no trial dirs with result.json under {args.job_dir}", file=sys.stderr)

    args.output.write_text(render(trials, args.job_dir))
    print(f"{len(trials)} trials → {args.output} ({args.output.stat().st_size // 1024} KB)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
