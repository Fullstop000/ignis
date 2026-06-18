// Message-waiting queue: typing while the agent is BUSY must hold the message
// in a local queue (visible strip, no transcript block, no submit), then drain
// exactly ONE per turn-end — mirroring the native ratatui TUI (app.enqueue +
// runner pump_queued).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { renderApp, KEY, tick, ev, plain } from './harness.js';

const submits = (engine) => engine.sent.filter((c) => c.kind === 'submit');

test('Enter while busy queues the message (no submit, no transcript block)', async () => {
  const { engine, stdin, lastFrame } = renderApp();
  await tick();
  engine.emit(ev('turn_start')); // busy
  await tick();

  stdin.write('queued one');
  await tick();
  stdin.write(KEY.enter);
  await tick();

  assert.equal(submits(engine).length, 0, 'busy Enter must NOT submit');
  const frame = plain(lastFrame());
  assert.match(frame, /queued one/, 'queued message shows in the queued strip');
  // The composer cleared (ready for the next line).
  assert.doesNotMatch(frame, /❯ .*queued one/, 'queued text is not still in the composer');
});

test('turn_end drains exactly one queued message as a submit', async () => {
  const { engine, stdin, lastFrame } = renderApp();
  await tick();
  engine.emit(ev('turn_start'));
  await tick();

  stdin.write('first');
  await tick();
  stdin.write(KEY.enter);
  await tick();
  stdin.write('second');
  await tick();
  stdin.write(KEY.enter);
  await tick();

  assert.equal(submits(engine).length, 0, 'nothing submitted while busy');

  // Turn ends → drain ONE (FIFO: "first").
  engine.emit(ev('turn_end'));
  await tick();
  assert.equal(submits(engine).length, 1, 'exactly one drains per turn-end');
  assert.equal(submits(engine)[0].data.text, 'first', 'FIFO order');
  // "second" still queued, "first" gone.
  const frame = plain(lastFrame());
  assert.match(frame, /second/, 'remaining message still queued');

  // Next turn cycle drains the second.
  engine.emit(ev('turn_start'));
  await tick();
  engine.emit(ev('turn_end'));
  await tick();
  assert.equal(submits(engine).length, 2, 'second drains on the next turn-end');
  assert.equal(submits(engine)[1].data.text, 'second');
});

test('an idle Enter still submits immediately (unchanged)', async () => {
  const { engine, stdin } = renderApp();
  await tick();
  // idle from the start
  stdin.write('hello');
  await tick();
  stdin.write(KEY.enter);
  await tick();
  assert.equal(submits(engine).length, 1, 'idle Enter submits right away');
  assert.equal(submits(engine)[0].data.text, 'hello');
});
