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
**markdown** (headings, bold/italic, inline + fenced code, bullet/ordered
lists), an `ask_user` picker that supports **single-select, multi-select, and
free-text "Other"** answers across **multiple questions**, and a one-line
composer. It sends submit / cancel (Ctrl+C) / inject (Ctrl+S) / picker replies.

Not yet at parity: native scrollback paging (long output reflows in place rather
than scrolling into terminal history — see `<Static>` follow-up) and
`/connect`-style masked text-input prompts (the wire carries `text_input`/`mask`,
but no slash-command surface drives them yet).

The pure logic — the Outbound→view-state reducer and command/answer builders
(`src/protocol.js`), and the markdown parser (`src/markdown.js`) — is unit-tested
with `node --test` (no install required).

## Run it (manual dogfood)

```sh
cd ignis-tui
npm install                       # fetches ink + react
# point at a locally-built engine; needs a configured provider to do real work
IGNIS_ENGINE_BIN=../target/debug/ignis npm start
```

(Default engine binary is `ignis` on `PATH`.) The `ignis` binary can also select
this frontend itself: set `IGNIS_FRONTEND=ink` and `IGNIS_TUI_ENTRY=<path to
src/cli.js>`; on any failure (Node missing, entry unset) it falls back to the
in-process ratatui TUI. Auto-locating the entry without `IGNIS_TUI_ENTRY` is a
packaging decision (how the JS + a Node runtime ship) that is deliberately left
open — see PR #174.

## Test

```sh
npm test            # node --test — pure protocol logic, no deps needed
```
