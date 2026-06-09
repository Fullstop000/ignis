"""
Render a side-by-side HTML comparison across two or more harbor bench CSVs.

Usage:
    python compare_runs.py --csv a.csv --csv b.csv [--csv c.csv ...] [-o out.html]
        --label "kimi-2.6" --label "deepseek-v4-flash@max" [--label "MiniMax-M3"]
        --href /reports/kimi   --href /reports/ds                [--href /reports/mm3]
        --price-cny "6.50,1.10,27.00" --price-cny "1.00,0.02,2.00" [--price-cny "2.10,0.42,8.40"]
        --fx-cny-per-usd 7.10
        --title "TB 2.1 · kimi-2.6 vs deepseek-v4-flash@max vs MiniMax-M3"

CSVs are joined by task slug, with dots collapsed to dashes so TB renames like
`install-windows-3.11` / `install-windows-3-11` align. Every task that appears
in at least one input gets a row; the cell for any missing model shows `—`.

The page surfaces:

1. Headline table — score, resolved%, bucket counts, tokens, est. cost, and
   cost / passed task — one column per run, with the winning value bold-green.
   Cost rows render only when `--price-cny` is supplied for at least one run;
   runs without a price show `—` in those cells.
2. Pass-count histogram — "all N passed", "(N-1)/N passed", ... "0/N passed".
3. Per-task table — one column per run, sortable, ordered by divergence
   (most-disagreed tasks first).
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
    task: str  # original slug (display)
    bucket: str  # passed | failed | errored | ""
    exception: str  # TimedOut / ConnectionDropped / ... / ""
    n_input_tokens: int = 0
    n_output_tokens: int = 0
    n_cache_read_tokens: int = 0


def _norm(task: str) -> str:
    """Join-key normalization. TB 2.0 used `install-windows-3-11`; TB 2.1 renamed
    it to `install-windows-3.11`. Dots otherwise don't appear in TB task slugs,
    so collapsing `.` → `-` is safe."""
    return task.replace(".", "-")


def _read_csv(p: Path) -> dict[str, Row]:
    rows: dict[str, Row] = {}
    for r in csv.DictReader(p.open()):
        key = _norm(r["task"])
        rows[key] = Row(
            task=r["task"],
            bucket=r["bucket"],
            exception=r["exception"],
            n_input_tokens=int(r["n_input_tokens"]) if r["n_input_tokens"] else 0,
            n_output_tokens=int(r["n_output_tokens"]) if r["n_output_tokens"] else 0,
            n_cache_read_tokens=int(r["n_cache_read_tokens"]) if r["n_cache_read_tokens"] else 0,
        )
    return rows


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

.histo { display: grid; grid-template-columns: repeat(auto-fit, minmax(150px, 1fr)); gap: 0.6rem; }
.histo .cell { background: white; border: 1px solid #e6e6e6; padding: 0.9rem 1.1rem; border-radius: 6px; }
.histo .cell .count { font-size: 1.5rem; font-weight: 700; color: #1a1a1a; }
.histo .cell .pct { font-size: 0.85rem; color: #888; margin-left: 0.4rem; }
.histo .cell .desc { font-size: 0.78rem; color: #666; margin-top: 0.25rem; }
.histo .all-passed     { background: #f0f9f3; }
.histo .all-not-passed { background: #fdf3f3; }
.histo .mixed          { background: #fef6ec; }

table.cmp { width: 100%; border-collapse: collapse; background: white; border-radius: 4px; overflow: hidden; border: 1px solid #e6e6e6; font-size: 13px; }
table.cmp th, table.cmp td { padding: 0.4rem 0.6rem; text-align: left; border-bottom: 1px solid #efefef; vertical-align: middle; }
table.cmp th { background: #f4f4f4; cursor: pointer; user-select: none; white-space: nowrap; font-size: 0.78rem; color: #555; text-transform: uppercase; letter-spacing: 0.04em; }
table.cmp th:hover { background: #ebebeb; }
table.cmp tr.q-all-passed     { background: #f7fbf8; }
table.cmp tr.q-mixed          { background: #fffaf0; }
table.cmp tr.q-all-not-passed { background: #fcf6f6; }
.bucket-tag { display: inline-block; padding: 1px 8px; border-radius: 10px; font-size: 0.72rem; font-weight: 600; color: white; white-space: nowrap; }
.bucket-passed  { background: #2e8b57; }
.bucket-failed  { background: #c66; }
.bucket-errored { background: #d99650; }
.muted { color: #aaa; }
.agree-tag { display: inline-block; padding: 1px 8px; border-radius: 10px; font-size: 0.7rem; font-weight: 600; }
.agree-all-passed     { background: #d6efd6; color: #245524; }
.agree-mixed          { background: #fde8d2; color: #6b4516; }
.agree-all-not-passed { background: #f4d6d6; color: #6b2727; }
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


def _aggregate(rows: dict[str, Row], price_cny: tuple[float, float, float] | None,
               fx_cny_per_usd: float) -> dict:
    buckets = Counter(r.bucket for r in rows.values())
    passed = buckets["passed"]
    failed = buckets["failed"]
    errored = buckets["errored"]
    total = passed + failed + errored
    excs = Counter(r.exception for r in rows.values() if r.exception)
    n_in = sum(r.n_input_tokens for r in rows.values())
    n_out = sum(r.n_output_tokens for r in rows.values())
    n_cache = sum(r.n_cache_read_tokens for r in rows.values())

    cost_usd: float | None = None
    cost_per_pass_usd: float | None = None
    if price_cny is not None:
        miss_p, hit_p, out_p = price_cny  # CNY per 1M tokens
        miss_in = max(0, n_in - n_cache)
        cost_cny = (miss_in * miss_p + n_cache * hit_p + n_out * out_p) / 1_000_000
        cost_usd = cost_cny / fx_cny_per_usd
        cost_per_pass_usd = (cost_usd / passed) if passed else None

    return {
        "passed": passed, "failed": failed, "errored": errored, "total": total,
        "score_pct": (passed / total * 100) if total else 0.0,
        "resolved_pct": (passed / (passed + failed) * 100) if (passed + failed) else 0.0,
        "exceptions": dict(excs),
        "n_in": n_in, "n_out": n_out, "n_cache": n_cache,
        "cache_hit_pct": (n_cache / n_in * 100) if n_in else 0.0,
        "tokens_per_pass": (n_in / passed) if passed else 0,
        "cost_usd": cost_usd,
        "cost_per_pass_usd": cost_per_pass_usd,
    }


def _cost_cells(values: list[float | None], fmt) -> str:
    """Headline row helper for cost — lower is better, runs without a price
    show `—` and don't compete for the winner highlight."""
    priced = [v for v in values if v is not None]
    best = min(priced) if priced else None
    out: list[str] = []
    for v in values:
        if v is None:
            out.append("<td>—</td>")
            continue
        cls = "winner" if (best is not None and v == best and len(priced) > 1) else ""
        out.append(f'<td class="{cls}">{fmt(v)}</td>')
    return "".join(out)


def _render_headline(stats: list[dict], labels: list[str]) -> str:
    """N-column headline. Winner = best value across all N (ties: no winner)."""
    n = len(stats)

    def cells(vals: list[float], display: list[str], higher_is_better: bool = True) -> str:
        best = max(vals) if higher_is_better else min(vals)
        winners = [v == best for v in vals]
        if sum(winners) == n:  # all tied → no winner
            winners = [False] * n
        return "".join(
            f'<td class="{"winner" if w else ""}">{d}</td>'
            for d, w in zip(display, winners)
        )

    head = "".join(f"<th>{html.escape(lbl)}</th>" for lbl in labels)
    rows: list[str] = []
    rows.append(
        "<tr><td>Score</td>"
        + cells(
            [s["passed"] for s in stats],
            [f'{s["passed"]}/{s["total"]} · {s["score_pct"]:.1f}%' for s in stats],
        )
        + "</tr>"
    )
    rows.append(
        "<tr><td>Resolved%</td>"
        + cells(
            [s["resolved_pct"] for s in stats],
            [f'{s["resolved_pct"]:.1f}%' for s in stats],
        )
        + "</tr>"
    )
    rows.append(
        "<tr><td>Failed</td>"
        + cells([s["failed"] for s in stats], [str(s["failed"]) for s in stats], higher_is_better=False)
        + "</tr>"
    )
    rows.append(
        "<tr><td>Errored</td>"
        + cells([s["errored"] for s in stats], [str(s["errored"]) for s in stats], higher_is_better=False)
        + "</tr>"
    )
    excs = [", ".join(f"{n} {k}" for k, n in s["exceptions"].items()) or "—" for s in stats]
    rows.append(
        "<tr><td>Exception breakdown</td>"
        + "".join(f"<td>{html.escape(e)}</td>" for e in excs)
        + "</tr>"
    )
    rows.append(
        "<tr><td>Input tokens</td>"
        + "".join(f'<td>{_fmt_tokens(s["n_in"])}</td>' for s in stats)
        + "</tr>"
    )
    rows.append(
        "<tr><td>Output tokens</td>"
        + "".join(f'<td>{_fmt_tokens(s["n_out"])}</td>' for s in stats)
        + "</tr>"
    )
    rows.append(
        "<tr><td>Cache hit</td>"
        + "".join(f'<td>{s["cache_hit_pct"]:.1f}%</td>' for s in stats)
        + "</tr>"
    )
    rows.append(
        "<tr><td>Input tokens / passed task</td>"
        + cells(
            [s["tokens_per_pass"] for s in stats],
            [_fmt_tokens(int(s["tokens_per_pass"])) for s in stats],
            higher_is_better=False,
        )
        + "</tr>"
    )

    # Cost rows render only when at least one run has prices. Each row covers
    # all N runs; runs without prices show "—" and are excluded from the winner
    # comparison.
    if any(s["cost_usd"] is not None for s in stats):
        rows.append("<tr><td>Est. cost ‡</td>" + _cost_cells(
            [s["cost_usd"] for s in stats],
            lambda v: f"≈${v:.2f}",
        ) + "</tr>")
        rows.append("<tr><td>Cost / passed task ‡</td>" + _cost_cells(
            [s["cost_per_pass_usd"] for s in stats],
            lambda v: f"≈${v:.2f}",
        ) + "</tr>")

    return f"""<div class="headline">
<table>
<thead><tr><th></th>{head}</tr></thead>
<tbody>
{"".join(rows)}
</tbody>
</table>
</div>"""


def _quad(passes: int, n: int) -> str:
    """Row colour class for the per-task table."""
    if passes == n:
        return "all-passed"
    if passes == 0:
        return "all-not-passed"
    return "mixed"


def _quad_label(passes: int, n: int) -> str:
    if passes == n:
        return f"all {n} passed" if n != 1 else "passed"
    if passes == 0:
        return f"none of {n} passed"
    return f"{passes} of {n} passed"


def _render_histogram(counts: Counter[int], total: int, n: int) -> str:
    """One cell per "k of N passed" bucket, k = N..0 (most-agreed best first)."""
    cells: list[str] = []
    for k in range(n, -1, -1):
        c = counts.get(k, 0)
        pct = (c / total * 100) if total else 0
        cls = _quad(k, n)
        cells.append(
            f'<div class="cell {cls}">'
            f'<div><span class="count">{c}</span><span class="pct">{pct:.0f}%</span></div>'
            f'<div class="desc">{_quad_label(k, n)}</div>'
            f"</div>"
        )
    return f'<div class="histo">{"".join(cells)}</div>'


def _render_table(joined: list[tuple[str, list[Row | None], int]], labels: list[str]) -> str:
    n = len(labels)
    # Sort: divergence first (most disagreement = passes closest to N/2),
    # then "all not-passed" before "all passed", then alphabetical.
    def sort_key(row):
        task, _, passes = row
        # divergence score: lower = more disagreement
        div = min(passes, n - passes)
        # within same divergence: prefer "all not-passed" over "all passed",
        # and "only a few passed" over "almost all passed"
        return (div, -((passes if passes <= n // 2 else n - passes)), passes, task)

    joined_sorted = sorted(joined, key=sort_key)
    rows_html: list[str] = []
    for task, runs, passes in joined_sorted:
        quad = _quad(passes, n)
        agree_cell = f'<span class="agree-tag agree-{quad}">{_quad_label(passes, n)}</span>'
        run_cells = "".join(
            f'<td>{_bucket_tag(r.bucket if r else "", r.exception if r else "")}</td>'
            for r in runs
        )
        rows_html.append(
            f'<tr class="q-{quad}" data-quad="{quad}">'
            f"<td>{html.escape(task)}</td>"
            f"{run_cells}"
            f'<td data-sort="{passes}">{agree_cell}</td>'
            f"</tr>"
        )
    head_cells = "".join(f'<th onclick="sortTable(this)">{html.escape(lbl)}</th>' for lbl in labels)
    return f"""<table class="cmp">
<thead><tr>
  <th onclick="sortTable(this)">Task</th>
  {head_cells}
  <th onclick="sortTable(this)" class="num">Agreement</th>
</tr></thead>
<tbody>{"".join(rows_html)}</tbody>
</table>"""


def render(csvs: list[Path], labels: list[str], hrefs: list[str | None],
           prices: list[tuple[float, float, float] | None], fx_cny_per_usd: float,
           title: str) -> str:
    if len(csvs) != len(labels):
        raise ValueError(f"got {len(csvs)} csvs but {len(labels)} labels — one --label per --csv")
    if len(hrefs) < len(csvs):
        hrefs = hrefs + [None] * (len(csvs) - len(hrefs))
    if len(prices) < len(csvs):
        prices = prices + [None] * (len(csvs) - len(prices))
    per_run = [_read_csv(p) for p in csvs]
    keys = sorted({k for run in per_run for k in run})

    joined: list[tuple[str, list[Row | None], int]] = []
    for k in keys:
        runs = [run.get(k) for run in per_run]
        # display name: take the first non-empty original slug from the inputs
        display = next((r.task for r in runs if r is not None), k)
        passes = sum(1 for r in runs if r and r.bucket == "passed")
        joined.append((display, runs, passes))

    counts = Counter(p for _, _, p in joined)
    stats = [_aggregate(run, price, fx_cny_per_usd) for run, price in zip(per_run, prices)]

    link_parts = [
        f'<a href="{html.escape(h)}">{html.escape(lbl)}</a>' if h else html.escape(lbl)
        for lbl, h in zip(labels, hrefs)
    ]
    meta_links = "  ·  ".join(link_parts)

    cost_footnote = ""
    if any(s["cost_usd"] is not None for s in stats):
        cost_footnote = (
            f"<p style=\"color:#888;font-size:0.78rem;margin:0.5rem 0 0\">"
            f"‡ Cost = (input − cache) × miss-price + cache × hit-price + output × out-price, "
            f"summed across all trials and converted at "
            f"≈1 USD = {fx_cny_per_usd:.2f} CNY. Per-run prices configured via "
            f"<code>--price-cny</code> in CNY/1M tokens.</p>"
        )

    return f"""<!DOCTYPE html>
<html><head><meta charset="utf-8"><title>{html.escape(title)}</title>
<style>{_CSS}</style></head><body>
<h1>{html.escape(title)}</h1>
<div class="meta">{meta_links}  ·  generated {dt.datetime.now().isoformat(timespec="seconds")}</div>

<h2>Headline</h2>
{_render_headline(stats, labels)}
{cost_footnote}

<h2>Per-task agreement</h2>
{_render_histogram(counts, len(joined), len(per_run))}

<h2>All {len(joined)} tasks</h2>
<p style="color:#666;font-size:0.85rem">Sorted by divergence (most-disagreed first). Click a column header to re-sort.</p>
{_render_table(joined, labels)}

<script>{_JS}</script>
</body></html>"""


def _parse_price(s: str) -> tuple[float, float, float]:
    parts = [p.strip() for p in s.split(",")]
    if len(parts) != 3:
        raise argparse.ArgumentTypeError(
            f"--price-cny needs 3 comma-separated numbers (miss,hit,out); got {s!r}"
        )
    try:
        return (float(parts[0]), float(parts[1]), float(parts[2]))
    except ValueError as e:
        raise argparse.ArgumentTypeError(f"--price-cny: {e}") from e


def main(argv: list[str]) -> int:
    p = argparse.ArgumentParser(description="Render a side-by-side HTML comparison across 2+ harbor bench CSVs.")
    p.add_argument("--csv", dest="csvs", action="append", type=Path, required=True,
                   help="path to a harbor bench CSV (repeat for each run).")
    p.add_argument("--label", dest="labels", action="append", default=[],
                   help="display label for the corresponding --csv (repeat).")
    p.add_argument("--href", dest="hrefs", action="append", default=[],
                   help="URL the corresponding label links to (repeat; optional).")
    p.add_argument("--price-cny", dest="prices", action="append", default=[],
                   type=_parse_price,
                   help='per-run CNY/1M-token price as "miss,hit,out" '
                        '(repeat; pass an empty string "" or omit to skip a run).')
    p.add_argument("--fx-cny-per-usd", type=float, default=7.10,
                   help="CNY per USD for cost conversion (default 7.10).")
    p.add_argument("-o", "--output", type=Path, default=Path("compare.html"))
    p.add_argument("--title", default=None)
    args = p.parse_args(argv)
    if len(args.csvs) < 2:
        p.error("need at least 2 --csv inputs")
    if not args.labels:
        args.labels = [c.stem for c in args.csvs]
    if len(args.labels) != len(args.csvs):
        p.error(f"got {len(args.csvs)} --csv but {len(args.labels)} --label; one label per csv")
    if args.prices and len(args.prices) > len(args.csvs):
        p.error("more --price-cny than --csv")
    title = args.title or "  vs  ".join(args.labels)
    html_str = render(args.csvs, args.labels, args.hrefs, args.prices, args.fx_cny_per_usd, title)
    args.output.write_text(html_str)
    print(f"wrote {args.output} ({len(html_str):,} bytes)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
