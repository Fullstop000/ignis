"""
Render a side-by-side HTML comparison between two harbor bench CSVs.

Usage:
    python compare_runs.py <csv-a> <csv-b> [-o out.html]
        [--label-a "deepseek-v4-flash@max"] [--label-b "MiniMax-M3"]
        [--href-a /reports/foo] [--href-b /reports/bar]
        [--title "TB 2.1: A vs B  ·  2026-06-03"]

The two CSVs are joined by `task`. Every task that appears in either input gets a
row in the output table; tasks present in only one input show `—` for the
missing side. The page surfaces three things the per-run reports can't:

1. Side-by-side headline numbers (score, resolved%, bucket counts, tokens,
   cost-per-pass).
2. A 2×2 agreement matrix (how often do the two models agree on a task).
3. The full per-task table with each model's bucket + sub-type, sortable.

YAGNI: takes two CSVs at most. Multi-way comparison can stack two-way runs
side-by-side later if it ever matters; today it doesn't.
"""

from __future__ import annotations

import argparse
import csv
import datetime as dt
import html
import sys
from collections import Counter
from dataclasses import dataclass
from pathlib import Path


@dataclass
class Row:
    task: str
    bucket: str  # passed | failed | errored | ""
    exception: str  # TimedOut / ConnectionDropped / ... / ""
    n_input_tokens: int = 0
    n_output_tokens: int = 0
    n_cache_read_tokens: int = 0


def _read_csv(p: Path) -> dict[str, Row]:
    rows: dict[str, Row] = {}
    for r in csv.DictReader(p.open()):
        rows[r["task"]] = Row(
            task=r["task"],
            bucket=r["bucket"],
            exception=r["exception"],
            n_input_tokens=int(r["n_input_tokens"]) if r["n_input_tokens"] else 0,
            n_output_tokens=int(r["n_output_tokens"]) if r["n_output_tokens"] else 0,
            n_cache_read_tokens=int(r["n_cache_read_tokens"]) if r["n_cache_read_tokens"] else 0,
        )
    return rows


def _fmt_num(n: int) -> str:
    return f"{n:,}" if n else "—"


def _fmt_tokens(n: int) -> str:
    if not n:
        return "—"
    if n >= 1_000_000:
        return f"{n / 1_000_000:.1f}M"
    if n >= 1_000:
        return f"{n / 1_000:.1f}K"
    return str(n)


def _bucket_tag(bucket: str, exception: str) -> str:
    if not bucket:
        return '<span class="muted">—</span>'
    label = bucket if not exception else f"{bucket} · {exception}"
    return f'<span class="bucket-tag bucket-{bucket}">{html.escape(label)}</span>'


_CSS = """
* { box-sizing: border-box; }
body { font: 14px/1.5 -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; margin: 0 auto; padding: 2rem; color: #1a1a1a; background: #fafafa; max-width: 1400px; }
h1 { margin: 0 0 0.5rem; font-size: 1.6rem; }
h2 { margin: 2.5rem 0 1rem; font-size: 1.2rem; border-bottom: 1px solid #ddd; padding-bottom: 0.4rem; }
.meta { color: #666; margin-bottom: 1.5rem; font-size: 0.9rem; }
.meta a { color: #4a90d9; text-decoration: none; }
.meta a:hover { text-decoration: underline; }

.headline { background: white; padding: 1.25rem 1.5rem; border-radius: 8px; border: 1px solid #e6e6e6; margin-bottom: 1.5rem; }
.headline table { width: 100%; border-collapse: collapse; }
.headline th, .headline td { padding: 0.5rem 0.75rem; text-align: right; border-bottom: 1px solid #efefef; font-variant-numeric: tabular-nums; }
.headline th { background: transparent; color: #888; font-weight: 500; font-size: 0.8rem; text-transform: uppercase; letter-spacing: 0.05em; }
.headline th:first-child, .headline td:first-child { text-align: left; color: #555; font-weight: 500; }
.headline tr:last-child td { border-bottom: none; }
.headline .winner { font-weight: 700; color: #2e8b57; }

.agreement { display: grid; grid-template-columns: auto repeat(2, 1fr); gap: 0; max-width: 720px; margin: 0 auto; }
.agreement .cell { background: white; border: 1px solid #e6e6e6; padding: 1rem 1.25rem; }
.agreement .corner, .agreement .h, .agreement .v { background: transparent; border: none; color: #888; font-size: 0.78rem; text-transform: uppercase; letter-spacing: 0.05em; text-align: center; padding: 0.4rem; }
.agreement .v { writing-mode: vertical-rl; transform: rotate(180deg); text-align: center; padding: 0.4rem 0.6rem; }
.agreement .cell .count { font-size: 1.6rem; font-weight: 700; color: #1a1a1a; }
.agreement .cell .pct { font-size: 0.85rem; color: #888; margin-left: 0.4rem; }
.agreement .cell .desc { font-size: 0.78rem; color: #666; margin-top: 0.25rem; }
.agreement .both-passed { background: #f0f9f3; }
.agreement .only-a { background: #fef6ec; }
.agreement .only-b { background: #ecf3fb; }
.agreement .both-not-passed { background: #fdf3f3; }

table.cmp { width: 100%; border-collapse: collapse; background: white; border-radius: 4px; overflow: hidden; border: 1px solid #e6e6e6; font-size: 13px; }
table.cmp th, table.cmp td { padding: 0.4rem 0.6rem; text-align: left; border-bottom: 1px solid #efefef; vertical-align: middle; }
table.cmp th { background: #f4f4f4; cursor: pointer; user-select: none; white-space: nowrap; font-size: 0.78rem; color: #555; text-transform: uppercase; letter-spacing: 0.04em; }
table.cmp th:hover { background: #ebebeb; }
table.cmp tr.both-passed { background: #f7fbf8; }
table.cmp tr.only-a, table.cmp tr.only-b { background: #fffaf0; }
table.cmp tr.both-not-passed { background: #fcf6f6; }
.bucket-tag { display: inline-block; padding: 1px 8px; border-radius: 10px; font-size: 0.72rem; font-weight: 600; color: white; white-space: nowrap; }
.bucket-passed  { background: #2e8b57; }
.bucket-failed  { background: #c66; }
.bucket-errored { background: #d99650; }
.muted { color: #aaa; }
.agree-tag { display: inline-block; padding: 1px 8px; border-radius: 10px; font-size: 0.7rem; font-weight: 600; }
.agree-both-passed { background: #d6efd6; color: #245524; }
.agree-only-a, .agree-only-b { background: #fde8d2; color: #6b4516; }
.agree-both-not-passed { background: #f4d6d6; color: #6b2727; }
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
    if (numeric) { av = parseFloat(av) || 0; bv = parseFloat(bv) || 0; return asc ? av - bv : bv - av; }
    return asc ? av.localeCompare(bv) : bv.localeCompare(av);
  });
  rows.forEach(r => tbl.tBodies[0].appendChild(r));
}
"""


# Quadrant ordering — divergence first, then the easy/hard agreements.
_QUAD_SORT = {"only-a": 0, "only-b": 1, "both-not-passed": 2, "both-passed": 3}
_QUAD_LABEL = {
    "only-a": "only A passed",
    "only-b": "only B passed",
    "both-passed": "both passed",
    "both-not-passed": "both not-passed",
}


def _agreement(a: Row | None, b: Row | None) -> str:
    a_pass = bool(a and a.bucket == "passed")
    b_pass = bool(b and b.bucket == "passed")
    if a_pass and b_pass:
        return "both-passed"
    if a_pass and not b_pass:
        return "only-a"
    if not a_pass and b_pass:
        return "only-b"
    return "both-not-passed"


def _aggregate(rows: dict[str, Row]) -> dict:
    """Per-run aggregate (score, bucket counts, totals)."""
    buckets = Counter(r.bucket for r in rows.values())
    passed = buckets["passed"]
    failed = buckets["failed"]
    errored = buckets["errored"]
    total = passed + failed + errored
    excs = Counter(r.exception for r in rows.values() if r.exception)
    n_in = sum(r.n_input_tokens for r in rows.values())
    n_out = sum(r.n_output_tokens for r in rows.values())
    n_cache = sum(r.n_cache_read_tokens for r in rows.values())
    return {
        "passed": passed, "failed": failed, "errored": errored, "total": total,
        "score_pct": (passed / total * 100) if total else 0.0,
        "resolved_pct": (passed / (passed + failed) * 100) if (passed + failed) else 0.0,
        "exceptions": dict(excs),
        "n_in": n_in, "n_out": n_out, "n_cache": n_cache,
        "cache_hit_pct": (n_cache / n_in * 100) if n_in else 0.0,
        "tokens_per_pass": (n_in / passed) if passed else 0,
    }


def _render_headline(stats_a: dict, stats_b: dict, label_a: str, label_b: str) -> str:
    def cell(va, vb, va_str, vb_str, higher_is_better=True):
        if va == vb:
            return f"<td>{va_str}</td><td>{vb_str}</td>"
        a_wins = (va > vb) if higher_is_better else (va < vb)
        cls_a = "winner" if a_wins else ""
        cls_b = "winner" if not a_wins else ""
        return f'<td class="{cls_a}">{va_str}</td><td class="{cls_b}">{vb_str}</td>'
    exc_a = ", ".join(f"{n} {k}" for k, n in stats_a["exceptions"].items()) or "—"
    exc_b = ", ".join(f"{n} {k}" for k, n in stats_b["exceptions"].items()) or "—"
    return f"""<div class="headline">
<table>
<thead><tr><th></th><th>{html.escape(label_a)}</th><th>{html.escape(label_b)}</th></tr></thead>
<tbody>
<tr><td>Score</td>{cell(stats_a["passed"], stats_b["passed"],
    f'{stats_a["passed"]}/{stats_a["total"]} · {stats_a["score_pct"]:.1f}%',
    f'{stats_b["passed"]}/{stats_b["total"]} · {stats_b["score_pct"]:.1f}%')}</tr>
<tr><td>Resolved%</td>{cell(stats_a["resolved_pct"], stats_b["resolved_pct"],
    f'{stats_a["resolved_pct"]:.1f}%', f'{stats_b["resolved_pct"]:.1f}%')}</tr>
<tr><td>Failed</td>{cell(stats_a["failed"], stats_b["failed"],
    str(stats_a["failed"]), str(stats_b["failed"]), higher_is_better=False)}</tr>
<tr><td>Errored</td>{cell(stats_a["errored"], stats_b["errored"],
    str(stats_a["errored"]), str(stats_b["errored"]), higher_is_better=False)}</tr>
<tr><td>Exception breakdown</td><td>{html.escape(exc_a)}</td><td>{html.escape(exc_b)}</td></tr>
<tr><td>Input tokens</td><td>{_fmt_tokens(stats_a["n_in"])}</td><td>{_fmt_tokens(stats_b["n_in"])}</td></tr>
<tr><td>Output tokens</td><td>{_fmt_tokens(stats_a["n_out"])}</td><td>{_fmt_tokens(stats_b["n_out"])}</td></tr>
<tr><td>Cache hit</td><td>{stats_a["cache_hit_pct"]:.1f}%</td><td>{stats_b["cache_hit_pct"]:.1f}%</td></tr>
<tr><td>Input tokens / passed task</td>{cell(stats_a["tokens_per_pass"], stats_b["tokens_per_pass"],
    _fmt_tokens(int(stats_a["tokens_per_pass"])), _fmt_tokens(int(stats_b["tokens_per_pass"])),
    higher_is_better=False)}</tr>
</tbody>
</table>
</div>"""


def _render_agreement_grid(counts: Counter[str], total: int, label_a: str, label_b: str) -> str:
    def cell(quad: str, klass: str) -> str:
        n = counts.get(quad, 0)
        pct = (n / total * 100) if total else 0
        return (
            f'<div class="cell {klass}">'
            f'<div><span class="count">{n}</span><span class="pct">{pct:.0f}%</span></div>'
            f'<div class="desc">{_QUAD_LABEL[quad]}</div>'
            f"</div>"
        )
    return f"""<div class="agreement">
  <div class="corner"></div>
  <div class="h">{html.escape(label_b)} passed</div>
  <div class="h">{html.escape(label_b)} not-passed</div>
  <div class="v">{html.escape(label_a)} passed</div>
  {cell("both-passed", "both-passed")}
  {cell("only-a", "only-a")}
  <div class="v">{html.escape(label_a)} not-passed</div>
  {cell("only-b", "only-b")}
  {cell("both-not-passed", "both-not-passed")}
</div>"""


def _render_table(joined: list[tuple[str, Row | None, Row | None, str]], label_a: str, label_b: str) -> str:
    rows_html = []
    # Sort: divergence first (only-A, only-B), then both-not-passed, then both-passed; within each, alphabetical.
    joined_sorted = sorted(joined, key=lambda x: (_QUAD_SORT[x[3]], x[0]))
    for task, a, b, quad in joined_sorted:
        agree_cell = f'<span class="agree-tag agree-{quad}">{_QUAD_LABEL[quad]}</span>'
        rows_html.append(
            f'<tr class="{quad}" data-quad="{quad}">'
            f'<td>{html.escape(task)}</td>'
            f'<td>{_bucket_tag(a.bucket if a else "", a.exception if a else "")}</td>'
            f'<td>{_bucket_tag(b.bucket if b else "", b.exception if b else "")}</td>'
            f'<td data-sort="{_QUAD_SORT[quad]}">{agree_cell}</td>'
            f"</tr>"
        )
    return f"""<table class="cmp">
<thead><tr>
  <th onclick="sortTable(this)">Task</th>
  <th onclick="sortTable(this)">{html.escape(label_a)}</th>
  <th onclick="sortTable(this)">{html.escape(label_b)}</th>
  <th onclick="sortTable(this)" class="num">Agreement</th>
</tr></thead>
<tbody>{"".join(rows_html)}</tbody>
</table>"""


def render(csv_a: Path, csv_b: Path, label_a: str, label_b: str,
           href_a: str | None, href_b: str | None, title: str) -> str:
    a_rows = _read_csv(csv_a)
    b_rows = _read_csv(csv_b)
    tasks = sorted(set(a_rows) | set(b_rows))
    joined = [(t, a_rows.get(t), b_rows.get(t), _agreement(a_rows.get(t), b_rows.get(t))) for t in tasks]
    counts = Counter(quad for _, _, _, quad in joined)
    stats_a = _aggregate(a_rows)
    stats_b = _aggregate(b_rows)

    a_link = f'<a href="{html.escape(href_a)}">{html.escape(label_a)}</a>' if href_a else html.escape(label_a)
    b_link = f'<a href="{html.escape(href_b)}">{html.escape(label_b)}</a>' if href_b else html.escape(label_b)

    return f"""<!DOCTYPE html>
<html><head><meta charset="utf-8"><title>{html.escape(title)}</title>
<style>{_CSS}</style></head><body>
<h1>{html.escape(title)}</h1>
<div class="meta">{a_link} &nbsp;vs&nbsp; {b_link} · generated {dt.datetime.now().isoformat(timespec="seconds")}</div>

<h2>Headline</h2>
{_render_headline(stats_a, stats_b, label_a, label_b)}

<h2>Per-task agreement</h2>
{_render_agreement_grid(counts, len(joined), label_a, label_b)}

<h2>All {len(joined)} tasks</h2>
<p style="color:#666;font-size:0.85rem">Sorted by divergence (only-A / only-B first), then both-not-passed, then both-passed. Click a column header to re-sort.</p>
{_render_table(joined, label_a, label_b)}

<script>{_JS}</script>
</body></html>"""


def main(argv: list[str]) -> int:
    p = argparse.ArgumentParser(description="Render a side-by-side HTML comparison of two harbor bench CSVs.")
    p.add_argument("csv_a", type=Path)
    p.add_argument("csv_b", type=Path)
    p.add_argument("-o", "--output", type=Path, default=Path("compare.html"))
    p.add_argument("--label-a", default="A")
    p.add_argument("--label-b", default="B")
    p.add_argument("--href-a", default=None, help="URL the A label links to (e.g., the A per-run report).")
    p.add_argument("--href-b", default=None, help="URL the B label links to.")
    p.add_argument("--title", default=None, help="Page title (default: 'A vs B').")
    args = p.parse_args(argv)
    title = args.title or f"{args.label_a} vs {args.label_b}"
    html_str = render(args.csv_a, args.csv_b, args.label_a, args.label_b, args.href_a, args.href_b, title)
    args.output.write_text(html_str)
    print(f"wrote {args.output} ({len(html_str):,} bytes)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
