// Flicker reproductions for paths NOT covered by flicker.e2e.test.js.
//
// flicker.e2e.test.js only proves that a tall *settled* transcript no longer
// triggers Ink's full-screen-clear path while busy — because settled blocks
// move into <Static> and the live region stays short.
//
// But two paths still keep the live region tall, and on every state change
// during them Ink trips its `outputHeight >= rows` branch in
// node_modules/ink/build/ink.js (`onRender`):
//
//     if (outputHeight >= this.options.stdout.rows) {
//         this.options.stdout.write(
//             ansiEscapes.clearTerminal + this.fullStaticOutput + output
//         );
//         return;   // ← bypasses the 32ms throttledLog
//     }
//
// `ansiEscapes.clearTerminal` contains ESC[3J (erase scrollback), so we count
// writes containing that as full-screen clears.
//
// 1. **Streaming assistant reply taller than the terminal.** `state.stream` is
//    rendered live as a single <Markdown> block (app.js:407–413); each
//    message_update delta grows it. As soon as it crosses the rows threshold
//    every subsequent delta full-screen-clears. No <Static> can catch it
//    because it's not settled yet.
//
// 2. **An unfinished tool block pinning everything after it dynamic.** The
//    `firstLive` walk in app.js:389–396 stops at the first `tool && !done`,
//    so a single slow tool keeps every later (already-done) block dynamic.
//    With parallel tool execution + bash run_in_background this is now common.
//
// Both tests render REAL `ink` against a fake TTY with small `rows`, like
// flicker.e2e.test.js. They will fail until the live region is bounded
// (e.g. a rolling-window stream view; or letting completed tools settle even
// when an earlier tool is still pending).

import { test } from 'node:test';
import assert from 'node:assert/strict';
import React from 'react';
import { render } from 'ink';
import { EventEmitter } from 'node:events';
import App from '../src/app.js';
import { mockEngine, ev } from './harness.js';

const CLEAR = '\x1b[3J'; // erase-scrollback escape inside ansiEscapes.clearTerminal

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

/** Stream `text` to App via message_start + N message_update deltas + message_end. */
async function streamReply(engine, text, deltas = 20) {
  engine.emit(ev('message_start', { message: { content: '' } }));
  const step = Math.ceil(text.length / deltas);
  for (let i = 0; i < text.length; i += step) {
    engine.emit(ev('message_update', { delta: text.slice(i, i + step) }));
    await sleep(10);
  }
  engine.emit(ev('message_end', { message: { content: text } }));
}

test('streaming assistant reply taller than the terminal full-screen-clears on every delta', async () => {
  const engine = mockEngine();
  const stdout = fakeStdout(10); // 10-row terminal
  const stdin = fakeStdin();
  const inst = render(React.createElement(App, { engine }), { stdout, stdin, patchConsole: false });

  // A turn opens; status goes busy.
  engine.emit(ev('user_prompt_committed', { text: 'hi' }));
  engine.emit(ev('turn_start'));
  await sleep(20);

  // Now stream a 30-line reply (well above the 10-row terminal). Each delta
  // grows state.stream → re-renders the (live) <Markdown text=stream> child.
  const lines = Array.from({ length: 30 }, (_, i) => `line number ${i} of the streaming reply`);
  const before = stdout.writes.filter((w) => w.includes(CLEAR)).length;
  await streamReply(engine, lines.join('\n'), 20);
  await sleep(50);
  const during = stdout.writes.filter((w) => w.includes(CLEAR)).length - before;

  inst.unmount();
  // Today: one full-screen clear per delta (sometimes two, after the
  // throttled batch boundary). The fix should bound the live stream view (a
  // rolling tail like ReasoningView, or commit the prefix to <Static>) so
  // this number stays at 0.
  assert.equal(during, 0, `expected 0 full-screen clears while streaming, got ${during}`);
});

test('a single unfinished tool pins later blocks dynamic and full-screen-clears on later events', async () => {
  const engine = mockEngine();
  const stdout = fakeStdout(10); // 10-row terminal
  const stdin = fakeStdin();
  const inst = render(React.createElement(App, { engine }), { stdout, stdin, patchConsole: false });

  engine.emit(ev('user_prompt_committed', { text: 'go' }));
  engine.emit(ev('turn_start'));

  // Open a slow tool that won't finish for the duration of the test. Per
  // app.js:389–396 this pins firstLive at this index — every later block stays
  // in the dynamic region instead of moving to <Static>.
  engine.emit(ev('tool_execution_start', {
    tool_call_id: 'slow',
    tool_name: 'bash',
    arguments: '{"command":"sleep 999"}',
  }));
  await sleep(20);

  // Now run several quick tools to completion. Each pair (start + end) is
  // small on its own, but they accumulate in the dynamic region (pinned by
  // 'slow') and push the live output past the 10-row terminal. The slow tool
  // is intentionally never marked done.
  const before = stdout.writes.filter((w) => w.includes(CLEAR)).length;
  for (let i = 0; i < 6; i++) {
    const id = `q${i}`;
    engine.emit(ev('tool_execution_start', {
      tool_call_id: id,
      tool_name: 'read_file',
      arguments: `{"path":"file_${i}.txt"}`,
    }));
    engine.emit(ev('tool_execution_end', {
      tool_call_id: id,
      result: { content: `content of file ${i}\nline 2\nline 3`, is_error: false },
    }));
    await sleep(20);
  }
  await sleep(50);
  const during = stdout.writes.filter((w) => w.includes(CLEAR)).length - before;

  inst.unmount();
  // Today: each completed tool grows the dynamic region (because 'slow' pins
  // firstLive at 0), so each tool_execution_end above the rows threshold
  // triggers a full-screen clear. The fix should let already-`done:true`
  // tools settle into <Static> even when an earlier tool is still pending.
  assert.equal(during, 0, `expected 0 full-screen clears while later tools complete behind a pending one, got ${during}`);
});
