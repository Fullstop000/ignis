// e2e harness for the Ink app: render <App> against an in-process mock engine,
// drive keystrokes through ink-testing-library's stdin, and assert on both the
// rendered frame and the ClientCommands the app emits. Deterministic — no
// subprocess, no network, no LLM. (The real subprocess round-trip is covered by
// the Rust `tests/engine_e2e.rs`.)
import React from 'react';
import { render } from 'ink-testing-library';
import App from '../src/app.js';

/** A fake of engine.js: stores App's frame/close/error callbacks, records sends,
 *  and lets the test push Outbound frames in. Shape matches src/engine.js. */
export function mockEngine() {
  let frameCb = null;
  let closeCb = () => {};
  let errorCb = () => {};
  const pending = []; // frames emitted before App registers onFrame (effect timing)
  const sent = [];
  return {
    onFrame: (cb) => {
      frameCb = cb;
      pending.splice(0).forEach(cb); // replay anything that arrived first
    },
    onClose: (cb) => {
      closeCb = cb;
    },
    onError: (cb) => {
      errorCb = cb;
    },
    send: (cmd) => sent.push(cmd),
    close: () => {
      sent.push({ kind: 'shutdown' });
      sent.push({ kind: '_closed' });
    },
    // ── test-facing ──
    emit: (frame) => (frameCb ? frameCb(frame) : pending.push(frame)),
    fireClose: (code) => closeCb(code),
    fireError: (err) => errorCb(err),
    sent,
    last: () => sent[sent.length - 1],
  };
}

export function renderApp(props = {}) {
  const engine = mockEngine();
  const r = render(React.createElement(App, { engine, ...props }));
  return { engine, ...r };
}

// Key byte sequences for stdin.write (what a terminal sends).
export const KEY = {
  enter: '\r',
  up: '\x1b[A',
  down: '\x1b[B',
  left: '\x1b[D',
  right: '\x1b[C',
  esc: '\x1b',
  backspace: '\x7f',
  ctrlC: '\x03',
  ctrlS: '\x13',
  ctrlU: '\x15',
  ctrlA: '\x01',
  ctrlE: '\x05',
  ctrlW: '\x17',
  ctrlO: '\x0f',
  ctrlJ: '\n', // Ink delivers Ctrl+J / LF as a lone '\n' (Enter is '\r')
  ctrlD: '\x04',
  space: ' ',
};

/** Strip ANSI so frame text can be matched regardless of color depth. */
export function plain(frame) {
  // eslint-disable-next-line no-control-regex
  return (frame ?? '').replace(/\x1b\[[0-9;]*m/g, '');
}

/** Let queued React state updates / effects flush. Flushes the event loop
 *  deterministically via `setImmediate` (fires after I/O callbacks, so the
 *  `data` event from `stdin.write` is processed) before a short delay for any
 *  further async work. More reliable under CI CPU contention than a bare
 *  `setTimeout`, which can miss its window when `node --test` runs files in
 *  parallel on a loaded runner. */
export const tick = async (ms = 30) => {
  await new Promise((r) => setImmediate(r));
  if (ms > 0) await new Promise((r) => setTimeout(r, ms));
};

// ── Outbound frame builders (mirror the engine's wire shapes) ──
export const ev = (type, payload) => ({ kind: 'event', data: payload === undefined ? { type } : { type, payload } });
export const request = (id, questions) => ({ kind: 'request', data: { id, questions } });
export const snapshot = (data) => ({ kind: 'snapshot', data });
export const sessions = (list) => ({ kind: 'sessions', data: list });
export const transcript = (sessionId, blocks) => ({ kind: 'transcript', data: { session_id: sessionId, blocks } });
