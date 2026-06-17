# ignis-tui (Ink frontend) — experimental

An out-of-process terminal frontend for **ignis**, written in [Ink](https://github.com/vadimdemedes/ink).
It owns the terminal (render + keyboard) and drives the headless Rust core
(`ignis --engine`) over the line-delimited JSON (NDJSON) protocol — topology
(ii) of PR #174: the frontend hosts the engine, the protocol rides the engine
child's stdin/stdout, and the engine itself needs no TTY.

```
ignis-tui (Ink, owns TTY)
   │  spawn `ignis --engine`, stdio:[pipe,pipe,inherit]
   ▼
ignis --engine (headless Rust core)
   stdout ──▶ Outbound  NDJSON  (events / requests / snapshots)  → render
   stdin  ◀── ClientCommand NDJSON (submit / cancel / inject / reply) ← keys
```

## Status

**Real, usable, approaching parity with the in-process ratatui TUI.** It renders
the streaming transcript (user / assistant / tool / inject / notice blocks) with
**markdown** (headings, bold/italic, inline + fenced code, bullet/ordered lists,
tables), an `ask_user` picker that supports **single-select, multi-select, and
free-text "Other"** answers across **multiple questions**, masked text-input for
`/connect`, slash-command suggestions, and a multi-line composer. It sends submit
/ cancel (Ctrl+C) / inject (Ctrl+S) / picker replies and drives `/model`,
`/connect`, `/sessions`, `/resume`, and the other engine commands over the seam.

Not yet at parity: native scrollback paging — long output reflows in place rather
than scrolling into terminal history (see the `<Static>` follow-up).

The pure logic — the Outbound→view-state reducer and command/answer builders
(`src/protocol.js`), and the markdown parser (`src/markdown.js`) — is unit-tested
with `node --test` (no install required).

## Run it

Install the deps once, then let the `ignis` binary launch this frontend for you:

```sh
( cd ignis-tui && npm install )   # fetches ink + react, once
cargo build                       # build the engine binary
./target/debug/ignis              # auto-locates ignis-tui/ and starts Ink
```

Ink is the **default** frontend wherever its deps are installed: a source checkout
finds `ignis-tui/` next to the build, and releases bundle it (`install.sh` /
`ignis upgrade` lay it down at `~/.ignis/ignis-tui`). `IGNIS_FRONTEND=native`
forces the in-process ratatui TUI, and any failure — Node missing, deps not
installed — falls back to it too. The lookup requires a sibling `node_modules`,
so a checkout without `npm install` stays on ratatui rather than crashing. Point
`IGNIS_TUI_ENTRY=<path to src/cli.js>` at a custom location to override.

System Node (>=18) is required; bundling a Node runtime is deliberately left out
(it would break the single-binary install).

To drive the frontend directly (manual dogfood against a locally-built engine):

```sh
cd ignis-tui
IGNIS_ENGINE_BIN=../target/debug/ignis npm start
```

(Default engine binary is `ignis` on `PATH`.)

## Test

```sh
npm test            # node --test — pure protocol logic, no deps needed
```
