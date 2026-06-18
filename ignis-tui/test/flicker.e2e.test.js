// Regression for the Ink flicker bug: once the transcript grows taller than the
// terminal, Ink's renderer falls back to a full-screen clear (ansiEscapes
// .clearTerminal, contains ESC[3J) on EVERY render — and the running-turn
// spinner re-renders ~11×/s, so the screen wipes-and-repaints constantly.
//
// Unlike the other e2e files this renders the REAL `ink` (not the debug-mode
// ink-testing-library), against a fake TTY with a small `rows`, so the
// `outputHeight >= rows` clear path is exercised. The fix (committing settled
// transcript blocks to <Static>) keeps the live region short, so that path is
// never taken: zero full-screen clears while a tall transcript streams.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import React from 'react';
import { render } from 'ink';
import { EventEmitter } from 'node:events';
import App from '../src/app.js';
import { mockEngine, ev } from './harness.js';

const CLEAR = '\x1b[3J'; // the erase-scrollback escape inside ansiEscapes.clearTerminal

function fakeStdout(rows) {
  const s = new EventEmitter();
  s.writes = [];
  s.columns = 80;
  s.rows = rows;
  s.write = (x) => { s.writes.push(x); return true; };
  return s;
}
function fakeStdin() {
  const s = new EventEmitter();
  s.isTTY = true;
  s.setRawMode = () => s;
  s.setEncoding = () => s;
  s.resume = () => s;
  s.pause = () => s;
  s.read = () => null;
  s.ref = () => s;
  s.unref = () => s;
  return s;
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

test('a tall transcript does not full-screen-clear on every render while busy', async () => {
  const engine = mockEngine();
  const stdout = fakeStdout(10); // a short, 10-row terminal
  const stdin = fakeStdin();
  const inst = render(React.createElement(App, { engine }), { stdout, stdin, patchConsole: false });

  // Build a transcript far taller than 10 rows: 12 user/assistant exchanges.
  for (let i = 0; i < 12; i++) {
    engine.emit(ev('user_prompt_committed', { text: `question number ${i}` }));
    engine.emit(ev('message_end', { message: { content: `assistant reply number ${i}` } }));
  }
  await sleep(50);

  // Go busy so the 90ms spinner starts ticking (the constant re-render driver).
  engine.emit(ev('turn_start'));
  await sleep(50);

  // Count full-screen clears over a ~400ms window (≈4 spinner ticks).
  const before = stdout.writes.filter((w) => w.includes(CLEAR)).length;
  await sleep(400);
  const during = stdout.writes.filter((w) => w.includes(CLEAR)).length - before;

  inst.unmount();
  assert.equal(during, 0, `expected 0 full-screen clears while busy, got ${during}`);
});
