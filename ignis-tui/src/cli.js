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
render(React.createElement(App, { engine }), { exitOnCtrlC: false });
