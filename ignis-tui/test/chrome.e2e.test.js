// e2e for the UI "chrome": slash-command suggestions, the running status bar,
// and the resume hint on a clean Ctrl+D exit — the dogfood-reported gaps.
import test from 'node:test';
import assert from 'node:assert/strict';
import { renderApp, plain, tick, KEY, ev, snapshot } from './harness.js';

test('typing / shows command suggestions', async () => {
  const { stdin, lastFrame } = renderApp();
  await tick();
  stdin.write('/');
  await tick();
  const f = plain(lastFrame());
  assert.match(f, /\/model/);
  assert.match(f, /\/sessions/);
  assert.match(f, /\/copy/);
});

test('startup banner shows the IGNIS art, engine version, and cwd', async () => {
  const { engine, lastFrame } = renderApp();
  await tick();
  engine.emit(snapshot({ session_id: 's1', version: '9.9.9', cwd: '/home/me/projx' }));
  await tick();
  const f = plain(lastFrame());
  assert.match(f, /██/, 'ascii-art banner rendered');
  assert.match(f, /v9\.9\.9/, 'engine version shown');
  assert.match(f, /\/home\/me\/projx/, 'cwd shown');
});

test('a typed prefix filters suggestions; Enter runs the selected one', async () => {
  const { stdin, lastFrame } = renderApp();
  await tick();
  stdin.write('/mo');
  await tick();
  const f = plain(lastFrame());
  assert.match(f, /\/model/);
  assert.doesNotMatch(f, /\/clear/, 'filtered to the prefix match');
  stdin.write(KEY.enter); // run the selected command
  await tick();
  assert.match(plain(lastFrame()), /Switch model/, 'running /model opened its picker');
});

test('a running turn shows the status bar (spinner work + ↓ tokens + interrupt)', async () => {
  const { engine, lastFrame } = renderApp();
  await tick();
  engine.emit(ev('turn_start'));
  engine.emit(ev('message_update', { delta: 'x'.repeat(40) }));
  await tick();
  const f = plain(lastFrame());
  assert.match(f, /Working…/);
  assert.match(f, /ctrl\+c to interrupt/);
  assert.match(f, /↓ \d+ tok/, 'live output-token estimate from streamed chars');
});

test('Ctrl+D after a turn emits the resume hint and exits', async () => {
  const seen = {};
  const { engine, stdin } = renderApp({ onExit: (h) => (seen.hint = h) });
  await tick();
  engine.emit(snapshot({ session_id: 'session-123-abcd' }));
  engine.emit(ev('user_prompt_committed', { text: 'hi' }));
  await tick();
  stdin.write(KEY.ctrlD);
  await tick();
  assert.ok(seen.hint, 'onExit called with a hint');
  assert.match(seen.hint, /ignis --resume session-123-abcd/);
});

test('Ctrl+D with no turns yet does not emit a resume hint', async () => {
  const seen = {};
  const { stdin } = renderApp({ onExit: (h) => (seen.hint = h) });
  await tick();
  stdin.write(KEY.ctrlD);
  await tick();
  assert.ok(!seen.hint, 'nothing worth resuming');
});
