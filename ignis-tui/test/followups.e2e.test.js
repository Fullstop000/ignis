// e2e for the follow-up suggestions strip: a `follow_ups` event renders an
// idle-only strip; Tab focuses it, ↑/↓ select, Enter submits the chosen prompt;
// a new turn clears it.
import test from 'node:test';
import assert from 'node:assert/strict';
import { renderApp, plain, tick, KEY, ev } from './harness.js';

const TAB = '\t';
const ESC = '\x1b';

test('follow_ups event renders an idle strip with the suggestions', async () => {
  const { engine, lastFrame } = renderApp();
  await tick();
  engine.emit(ev('follow_ups', { items: ['Run the tests', 'Add error handling'] }));
  await tick();
  const f = plain(lastFrame());
  assert.match(f, /Run the tests/);
  assert.match(f, /Add error handling/);
  assert.match(f, /Tab to pick a suggestion/);
});

test('Tab focuses, ↓ selects, Enter submits the chosen follow-up', async () => {
  const { engine, stdin, lastFrame } = renderApp();
  await tick();
  engine.emit(ev('follow_ups', { items: ['First option', 'Second option'] }));
  await tick();
  stdin.write(TAB);
  await tick();
  // Focused: the hint switches to navigation.
  assert.match(plain(lastFrame()), /Enter send/);
  stdin.write(KEY.down);
  await tick();
  stdin.write(KEY.enter);
  await tick();
  // The second option was submitted to the engine.
  assert.deepEqual(engine.last(), { kind: 'submit', data: { text: 'Second option' } });
});

test('Esc cancels focus without submitting', async () => {
  const { engine, stdin, lastFrame } = renderApp();
  await tick();
  engine.emit(ev('follow_ups', { items: ['Only option'] }));
  await tick();
  stdin.write(TAB);
  await tick();
  stdin.write(ESC);
  await tick();
  assert.match(plain(lastFrame()), /Tab to pick a suggestion/, 'back to unfocused hint');
  assert.ok(!engine.sent.some((c) => c.kind === 'submit'), 'nothing submitted');
});

test('a new turn clears the follow-up strip', async () => {
  const { engine, lastFrame } = renderApp();
  await tick();
  engine.emit(ev('follow_ups', { items: ['Stale suggestion'] }));
  await tick();
  assert.match(plain(lastFrame()), /Stale suggestion/);
  engine.emit(ev('turn_start'));
  await tick();
  assert.doesNotMatch(plain(lastFrame()), /Stale suggestion/);
});
