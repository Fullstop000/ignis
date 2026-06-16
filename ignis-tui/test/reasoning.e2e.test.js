// e2e for reasoning (chain-of-thought) rendering: the engine streams reasoning
// as MessageStart(reasoning)/MessageUpdate/MessageEnd, which the frontend shows
// as a ✻ Thinking block — a live rolling preview, then a collapsed breadcrumb
// that Ctrl+O expands. (Reasoning shares the message events with reply text; the
// start event's shape disambiguates them — see protocol.js.)
import test from 'node:test';
import assert from 'node:assert/strict';
import { renderApp, plain, tick, KEY, ev } from './harness.js';

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
  const { engine, stdin, lastFrame } = renderApp();
  await tick();
  engine.emit(ev('message_start', { message: { reasoning_content: '' } }));
  engine.emit(ev('message_end', { message: { reasoning_content: LONG } }));
  await tick();
  let f = plain(lastFrame());
  assert.match(f, /✻ Thinking/);
  assert.match(f, /line1/);
  assert.match(f, /\(\+4 lines · ctrl\+o to expand\)/);
  assert.doesNotMatch(f, /line5/, 'the tail is hidden when collapsed');

  stdin.write(KEY.ctrlO);
  await tick();
  f = plain(lastFrame());
  assert.match(f, /line5/, 'expanded shows the full reasoning');
  assert.doesNotMatch(f, /ctrl\+o to expand/);
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
