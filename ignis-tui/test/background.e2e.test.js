// e2e for the background-shell footer indicator: a `background_shells` event
// shows `⚙ N bg` in the footer while N > 0, and hides it at 0.
import test from 'node:test';
import assert from 'node:assert/strict';
import { renderApp, plain, tick, ev, snapshot } from './harness.js';

test('footer shows ⚙ N bg while background shells are live, hides at 0', async () => {
  const { engine, lastFrame } = renderApp();
  await tick();
  // A snapshot so the footer has provider/model and renders.
  engine.emit(snapshot({ session_id: 's', provider: 'deepseek', model: 'v4', cwd: '/tmp/proj' }));
  await tick();
  assert.doesNotMatch(plain(lastFrame()), /bg/, 'no indicator before any shell');

  engine.emit(ev('background_shells', { running: 2 }));
  await tick();
  assert.match(plain(lastFrame()), /⚙ 2 bg/);

  engine.emit(ev('background_shells', { running: 1 }));
  await tick();
  assert.match(plain(lastFrame()), /⚙ 1 bg/);

  engine.emit(ev('background_shells', { running: 0 }));
  await tick();
  assert.doesNotMatch(plain(lastFrame()), /bg/, 'indicator hidden at 0');
});
