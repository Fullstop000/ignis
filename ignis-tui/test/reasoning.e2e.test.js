// e2e for reasoning (chain-of-thought) rendering: the engine streams reasoning
// as MessageStart(reasoning)/MessageUpdate/MessageEnd, which the frontend shows
// as a ✻ Thinking block — a live rolling preview, then a collapsed breadcrumb
// that Ctrl+O expands. (Reasoning shares the message events with reply text; the
// start event's shape disambiguates them — see protocol.js.)
import test from 'node:test';
import assert from 'node:assert/strict';
import { renderApp, plain, tick, KEY, ev } from './harness.js';

const lastWith = (frames, re) => plain([...frames].reverse().find((f) => re.test(plain(f))) ?? '');

const LONG = 'line1\nline2\nline3\nline4\nline5';

test('a reasoning stream renders a live ✻ Thinking block (not assistant text)', async () => {
  const { engine, lastFrame } = renderApp();
  await tick();
  engine.emit(ev('turn_start'));
  engine.emit(ev('message_start', { message: { reasoning_content: '' } }));
  engine.emit(ev('message_update', { delta: 'pondering the problem' }));
  await tick();
  const f = plain(lastFrame());
  assert.match(f, /✻ Thinking…/);
  assert.match(f, /pondering the problem/);
});

test('a finished long reasoning collapses, and Ctrl+O expands it', async () => {
  const { engine, stdin, lastFrame, frames } = renderApp();
  await tick();
  engine.emit(ev('message_start', { message: { reasoning_content: '' } }));
  engine.emit(ev('message_end', { message: { reasoning_content: LONG } }));
  await tick();
  const f = plain(lastFrame());
  assert.match(f, /✻ Thinking/);
  assert.match(f, /line1/);
  assert.match(f, /\(\+4 lines · ctrl\+o to expand\)/);
  assert.doesNotMatch(f, /line5/, 'the tail is hidden when collapsed');

  // The reasoning block has committed to <Static>; Ctrl+O repaints (screen wipe
  // + remount) with the expanded state, the same way the native TUI re-anchors.
  // Debug-mode lastFrame keeps the older collapsed flush, so assert the latest
  // re-rendered frame shows the full reasoning and the screen was wiped.
  stdin.write(KEY.ctrlO);
  await tick();
  assert.match(lastWith(frames, /line5/), /line5/, 'expanded shows the full reasoning');
  assert.ok(frames.some((fr) => fr.includes('\x1b[2J')), 'Ctrl+O repaints the screen');
});

test('a reasoning block then a reply render as distinct blocks', async () => {
  const { engine, lastFrame } = renderApp();
  await tick();
  engine.emit(ev('message_start', { message: { reasoning_content: '' } }));
  engine.emit(ev('message_end', { message: { reasoning_content: 'brief thought' } }));
  engine.emit(ev('message_start', { message: { content: '' } }));
  engine.emit(ev('message_end', { message: { content: 'the final answer' } }));
  await tick();
  const f = plain(lastFrame());
  assert.match(f, /✻ Thinking/);
  assert.match(f, /brief thought/);
  assert.match(f, /the final answer/);
});
