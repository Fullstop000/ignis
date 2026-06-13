#!/usr/bin/env node
// Entry point: spawn the headless ignis engine and render the Ink UI against it.
// The engine binary is `ignis` on PATH by default; override with
// IGNIS_ENGINE_BIN (e.g. ../target/debug/ignis during development).
import React from 'react';
import { render } from 'ink';
import { spawnEngine } from './engine.js';
import App from './app.js';

const engine = spawnEngine();
render(React.createElement(App, { engine }));
