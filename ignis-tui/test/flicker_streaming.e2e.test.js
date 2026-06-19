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

test('parallel tool calls (one slow + many quick) never trip full-screen-clears', async () => {
  // Original bug: `firstLive` walked from index 0 and stopped at the first
  // pending tool, pinning every later block into the dynamic region. With
  // `bash run_in_background` (or any slow tool) sitting at the top of the
  // turn while several quick tools finished after it, the dynamic region
  // grew unbounded and Ink's `outputHeight >= rows` fallback kicked in,
  // full-screen-clearing on every later event.
  //
  // Under the append-only pipeline, tool starts no longer enter `blocks`;
  // they live in `state.activeTools` (a Map keyed by tool_call_id, so
  // parallel starts don't overwrite each other). Each tool's _end commits
  // its full block — with the right name/args — to <Static>. The dynamic
  // region stays bounded at RunningBar + Composer + Footer.
  //
  // We assert (a) zero full-screen clears across the whole sequence, AND
  // (b) the six quick tools each committed with the correct name+args —
  // the latter would have failed under the old single-slot `activeTool`
  // (each later `_start` overwrote the previous one, so `_end` lookups
  // missed and committed `● ()`).
  const engine = mockEngine();
  const stdout = fakeStdout(10); // 10-row terminal
  const stdin = fakeStdin();
  const inst = render(React.createElement(App, { engine }), { stdout, stdin, patchConsole: false });

  engine.emit(ev('user_prompt_committed', { text: 'go' }));
  engine.emit(ev('turn_start'));

  // A slow tool that won't finish for the duration of the test.
  engine.emit(ev('tool_execution_start', {
    tool_call_id: 'slow',
    tool_name: 'bash',
    arguments: '{"command":"sleep 999"}',
  }));
  await sleep(20);

  const before = stdout.writes.filter((w) => w.includes(CLEAR)).length;
  // Six quick tools, all started in parallel (true buffer_unordered shape:
  // every Start arrives before any End). Under the old single-slot model,
  // each later Start overwrote the prior one, so the first five `_end`
  // events looked up a stale slot and committed `● ()`.
  for (let i = 0; i < 6; i++) {
    engine.emit(ev('tool_execution_start', {
      tool_call_id: `q${i}`,
      tool_name: 'read_file',
      arguments: `{"path":"file_${i}.txt"}`,
    }));
  }
  // Now drain the ends out of order (mirrors real concurrency: the longest
  // file is rarely the first to finish).
  for (const i of [3, 0, 5, 1, 4, 2]) {
    engine.emit(ev('tool_execution_end', {
      tool_call_id: `q${i}`,
      result: { content: `content of file ${i}\nline 2\nline 3`, is_error: false },
    }));
    await sleep(20);
  }
  await sleep(50);
  const during = stdout.writes.filter((w) => w.includes(CLEAR)).length - before;

  // The flushed Static output should contain the six quick tools' headers
  // with their real names and args (`read_file(file_N.txt)`), not the
  // empty `● ()` the single-slot bug produced. We can read the cumulative
  // stdout for this assertion since each Static commit is one write.
  const flushed = stdout.writes.join('');

  inst.unmount();
  assert.equal(during, 0, `expected 0 full-screen clears across parallel tool churn, got ${during}`);
  // The single-slot bug committed `● ()` (empty name + empty args) for every
  // tool whose `_end` arrived after another `_start` had clobbered its slot.
  // Under the Map, every commit carries the right header.
  assert.ok(
    !/●\s*\(\)/.test(flushed),
    'committed tool blocks must carry name+args; saw `● ()` (single-slot regression)',
  );
});
