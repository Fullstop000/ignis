// e2e for /copy: the frontend holds the transcript, so it extracts the last
// assistant reply and hands the text to the engine to copy (the engine reuses
// its platform clipboard helper). Optimistic "Copied to clipboard." notice.
import test from 'node:test';
import assert from 'node:assert/strict';
import { renderApp, plain, tick, KEY, ev } from './harness.js';

async function slash(stdin, name) {
  stdin.write(`/${name}`);
  await tick();
  stdin.write(KEY.enter);
  await tick();
}

test('/copy sends the last assistant message and shows a notice', async () => {
  const { engine, stdin, lastFrame } = renderApp();
  await tick();
  engine.emit(ev('message_end', { message: { content: 'the answer is 42' } }));
  await tick();
  await slash(stdin, 'copy');
  assert.deepEqual(engine.last(), { kind: 'copy', data: { text: 'the answer is 42' } });
  assert.match(plain(lastFrame()), /Copied to clipboard\./);
});

test('/copy with no assistant reply says nothing to copy and sends no command', async () => {
  const { engine, stdin, lastFrame } = renderApp();
  await tick();
  await slash(stdin, 'copy');
  assert.match(plain(lastFrame()), /Nothing to copy\./);
  assert.ok(!engine.sent.some((c) => c.kind === 'copy'), 'no copy command sent');
});
