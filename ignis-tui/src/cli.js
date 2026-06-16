#!/usr/bin/env node
// Entry point: spawn the headless ignis engine and render the Ink UI against it.
// The engine binary is `ignis` on PATH by default; override with
// IGNIS_ENGINE_BIN (e.g. ../target/debug/ignis during development).
import React from 'react';
import { render } from 'ink';
import { spawnEngine } from './engine.js';
import App from './app.js';

const engine = spawnEngine();
// exitOnCtrlC:false — Ink's default would exit the whole app on Ctrl+C before
// our handler runs, killing an in-flight turn. App owns Ctrl+C: cancel the turn
// when busy, exit cleanly when idle (see app.js).
// `onExit` carries the `ignis --resume <id>` hint, printed AFTER Ink tears down
// the alt buffer so it lands in the real scrollback like the native TUI.
const ctx = {};
const { waitUntilExit } = render(
  React.createElement(App, { engine, onExit: (hint) => (ctx.hint = hint) }),
  { exitOnCtrlC: false },
);
await waitUntilExit();
if (ctx.hint) process.stdout.write(ctx.hint);
