// Acceptance tests for the append-only render pipeline.
//
// Goal: prove that under the new rendering contract, every committed block of
// the conversation lands in Ink's <Static> output (= the terminal's real
// scrollback), and the dynamic region only ever holds the running status bar,
// composer, footer, and other ephemeral chrome.
//
// We use real `ink` (not ink-testing-library) against a fake TTY so we can
// observe the SAME `staticOutput` Ink writes to scrollback. Each test asserts
// on what eventually accumulates in stdout (Static + last dynamic frame).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import React from 'react';
import { render } from 'ink';
import { EventEmitter } from 'node:events';
import App from '../src/app.js';
import { mockEngine, ev, transcript } from './harness.js';

const ESC_RE = /\x1b\[[0-9;]*[A-Za-z]/g;
const plain = (s) => (s ?? '').replace(ESC_RE, '');

function fakeStdout(rows = 24, columns = 80) {
  const s = new EventEmitter();
  s.writes = [];
  s.columns = columns;
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

/** Concatenated stdout writes, ANSI-stripped. */
const allText = (stdout) => plain(stdout.writes.join(''));

test('a streaming assistant reply lands in scrollback once on message_end (append-only)', async () => {
  const engine = mockEngine();
  const stdout = fakeStdout(40);
  const stdin = fakeStdin();
  const inst = render(React.createElement(App, { engine }), { stdout, stdin, patchConsole: false });
  await sleep(20);

  engine.emit(ev('user_prompt_committed', { text: 'hi' }));
  engine.emit(ev('turn_start'));
  engine.emit(ev('message_start', { message: { content: '' } }));
  // Stream the reply across 10 deltas. Under append-only NONE of these should
  // grow the dynamic region with reply text — the running status may show
  // counters, but the reply itself only appears after message_end.
  const reply = 'this is a multi-line reply.\nsecond line.\nthird line.';
  const step = Math.ceil(reply.length / 10);
  for (let i = 0; i < reply.length; i += step) {
    engine.emit(ev('message_update', { delta: reply.slice(i, i + step) }));
    await sleep(5);
  }
  // Before message_end: the reply text must not be in the rendered output
  // (otherwise we're back to an in-place streaming view that re-renders on
  // every delta — the flicker root cause).
  const midText = allText(stdout);
  assert.ok(!midText.includes('multi-line reply'),
    'reply text must not appear in stdout while still streaming');

  engine.emit(ev('message_end', { message: { content: reply } }));
  engine.emit(ev('turn_end'));
  await sleep(30);

  const finalText = allText(stdout);
  assert.ok(finalText.includes('multi-line reply'), 'final reply lands in scrollback');
  assert.ok(finalText.includes('second line'));
  assert.ok(finalText.includes('third line'));

  inst.unmount();
});

test('a tool call only shows up in scrollback after tool_execution_end (append-only)', async () => {
  const engine = mockEngine();
  const stdout = fakeStdout(40);
  const stdin = fakeStdin();
  const inst = render(React.createElement(App, { engine }), { stdout, stdin, patchConsole: false });
  await sleep(20);

  engine.emit(ev('user_prompt_committed', { text: 'go' }));
  engine.emit(ev('turn_start'));
  engine.emit(ev('tool_execution_start', {
    tool_call_id: 'b1',
    tool_name: 'bash',
    arguments: '{"command":"ls -l"}',
  }));
  await sleep(20);

  // Mid-tool: the tool block (with its result body) must not be in scrollback.
  // The running status bar may show "● bash(...)" inline, but the committed
  // tool block — including the result preview — only writes once on _end.
  const midText = allText(stdout);
  assert.ok(!midText.includes('total 0'), 'tool result must not appear before _end');

  engine.emit(ev('tool_execution_end', {
    tool_call_id: 'b1',
    result: { content: 'total 0\nfile-a\nfile-b', is_error: false },
  }));
  await sleep(20);

  const finalText = allText(stdout);
  assert.ok(finalText.includes('bash'), 'tool name in scrollback');
  assert.ok(finalText.includes('total 0'), 'tool result preview in scrollback');

  inst.unmount();
});

test('a complete turn (user → reasoning → tool → assistant) writes every block to scrollback', async () => {
  const engine = mockEngine();
  const stdout = fakeStdout(50);
  const stdin = fakeStdin();
  const inst = render(React.createElement(App, { engine }), { stdout, stdin, patchConsole: false });
  await sleep(20);

  engine.emit(ev('user_prompt_committed', { text: 'design something' }));
  engine.emit(ev('turn_start'));

  // Reasoning stream → end.
  engine.emit(ev('message_start', { message: { reasoning_content: '', content: null } }));
  engine.emit(ev('message_update', { delta: 'thinking step A\n' }));
  engine.emit(ev('message_update', { delta: 'thinking step B' }));
  engine.emit(ev('message_end', {
    message: { reasoning_content: 'thinking step A\nthinking step B', content: null },
  }));
  await sleep(10);

  // Tool call → end.
  engine.emit(ev('tool_execution_start', {
    tool_call_id: 't1',
    tool_name: 'read_file',
    arguments: '{"path":"src/main.rs"}',
  }));
  engine.emit(ev('tool_execution_end', {
    tool_call_id: 't1',
    result: { content: 'fn main() {}', is_error: false },
  }));
  await sleep(10);

  // Assistant reply → end.
  engine.emit(ev('message_start', { message: { content: '' } }));
  engine.emit(ev('message_update', { delta: 'here is the answer.' }));
  engine.emit(ev('message_end', { message: { content: 'here is the answer.' } }));
  engine.emit(ev('turn_end'));
  await sleep(30);

  const text = allText(stdout);
  assert.ok(text.includes('design something'), 'user prompt in scrollback');
  assert.ok(text.includes('thinking step A'), 'reasoning A in scrollback');
  assert.ok(text.includes('thinking step B'), 'reasoning B in scrollback');
  assert.ok(text.includes('read_file'), 'tool name in scrollback');
  assert.ok(text.includes('fn main()'), 'tool result in scrollback');
  assert.ok(text.includes('here is the answer'), 'assistant reply in scrollback');

  inst.unmount();
});

test('resume replays the full transcript into scrollback', async () => {
  const engine = mockEngine();
  const stdout = fakeStdout(40);
  const stdin = fakeStdin();
  const inst = render(React.createElement(App, { engine }), { stdout, stdin, patchConsole: false });
  await sleep(20);

  engine.emit(transcript('s-resumed', [
    { kind: 'user', text: 'first message' },
    { kind: 'assistant', text: 'first reply body' },
    { kind: 'tool', name: 'bash', args: '{"command":"ls"}', result: { content: 'a\nb\nc', is_error: false } },
    { kind: 'user', text: 'second message' },
    { kind: 'assistant', text: 'second reply body' },
  ]));
  await sleep(30);

  const text = allText(stdout);
  assert.ok(text.includes('first message'), 'first user in scrollback');
  assert.ok(text.includes('first reply body'), 'first assistant in scrollback');
  assert.ok(text.includes('bash'), 'replayed tool name in scrollback');
  assert.ok(text.includes('second message'), 'second user in scrollback');
  assert.ok(text.includes('second reply body'), 'second assistant in scrollback');

  inst.unmount();
});
