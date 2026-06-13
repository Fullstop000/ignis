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

**Real but minimal — not yet at parity with the in-process ratatui TUI.** It
renders the streaming transcript (user / assistant / tool / inject / notice
blocks), a single-select `ask_user` picker, and a one-line composer, and sends
submit / cancel (Ctrl+C) / inject (Ctrl+S) / picker replies. Not yet: multi-
select & free-text picker answers, markdown, scrollback paging, `/connect`-style
text-input prompts.

The pure protocol logic (Outbound→view-state reducer, key→ClientCommand mapper)
lives in `src/protocol.js` and is unit-tested with `node --test` (no install
required).

## Run it (manual dogfood)

```sh
cd ignis-tui
npm install                       # fetches ink + react
# point at a locally-built engine; needs a configured provider to do real work
IGNIS_ENGINE_BIN=../target/debug/ignis npm start
```

(Default engine binary is `ignis` on `PATH`.) Phase 4 will add a launcher so the
`ignis` binary itself selects this frontend when Node is present, falling back
to the in-process ratatui TUI otherwise.

## Test

```sh
npm test            # node --test — pure protocol logic, no deps needed
```
