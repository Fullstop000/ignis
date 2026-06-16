// e2e for /sessions + /resume: the slash command asks the engine for the
// session list, the picker renders the (async) list, and selecting one sends a
// resume_session command + the engine's transcript replay repaints scrollback.
import test from 'node:test';
import assert from 'node:assert/strict';
import { renderApp, plain, tick, KEY, sessions, transcript } from './harness.js';

const LIST = [
  { id: 'session-a', preview: 'first task', message_count: 4, last_modified: 1000 },
  { id: 'session-b', preview: 'second task', message_count: 2, last_modified: 900 },
];

test('/sessions asks the engine for the list, then renders it', async () => {
  const { engine, stdin, lastFrame } = renderApp();
  await tick();
  stdin.write('/sessions');
  await tick();
  stdin.write(KEY.enter);
  await tick();
  // The slash command is handled locally: it requests the list, not a submit.
  assert.deepEqual(engine.last(), { kind: 'list_sessions' });
  // The picker is open but the list hasn't arrived yet.
  assert.match(plain(lastFrame()), /Resume a session/);
  assert.match(plain(lastFrame()), /Loading sessions…/);

  engine.emit(sessions(LIST));
  await tick();
  const f = plain(lastFrame());
  assert.match(f, /first task/);
  assert.match(f, /session-a · 4 msgs/);
  assert.match(f, /second task/);
  assert.doesNotMatch(f, /Loading sessions…/);
});

test('selecting a session resumes it and replays the transcript', async () => {
  const { engine, stdin, lastFrame } = renderApp();
  await tick();
  stdin.write('/sessions');
  await tick();
  stdin.write(KEY.enter);
  await tick();
  engine.emit(sessions(LIST));
  await tick();

  // Move to the second row and confirm.
  stdin.write(KEY.down);
  await tick();
  stdin.write(KEY.enter);
  await tick();
  assert.deepEqual(engine.last(), { kind: 'resume_session', data: { session_id: 'session-b' } });

  // The engine replays the chosen session as render-ready blocks.
  engine.emit(
    transcript('session-b', [
      { kind: 'user', text: 'second task' },
      { kind: 'assistant', text: 'all done' },
      { kind: 'tool', name: 'bash', args: 'ls', result: { content: 'out.txt', is_error: false } },
    ]),
  );
  await tick();
  const f = plain(lastFrame());
  assert.match(f, /second task/);
  assert.match(f, /all done/);
  assert.match(f, /bash\(ls\)/, 'replayed tool block renders');
  assert.match(f, /out\.txt/, 'replayed tool result preview renders');
  // The picker closed; the composer is back.
  assert.doesNotMatch(f, /Resume a session/);
});

test('a long session list is windowed so it never overflows', async () => {
  const many = Array.from({ length: 20 }, (_, i) => ({
    id: `session-${i}`,
    preview: `task ${i}`,
    message_count: 1,
    last_modified: 1000 - i,
  }));
  const { engine, stdin, lastFrame } = renderApp();
  await tick();
  stdin.write('/sessions');
  await tick();
  stdin.write(KEY.enter);
  await tick();
  engine.emit(sessions(many));
  await tick();
  const f = plain(lastFrame());
  // Only a window is shown, with a "more below" affordance; the top rows render
  // (cursor at 0) but the far tail does not.
  assert.match(f, /task 0/);
  assert.match(f, /↓ \d+ more/);
  assert.doesNotMatch(f, /task 19/, 'far tail is windowed out');
});

test('Esc cancels the session picker without resuming', async () => {
  const { engine, stdin, lastFrame } = renderApp();
  await tick();
  stdin.write('/sessions');
  await tick();
  stdin.write(KEY.enter);
  await tick();
  engine.emit(sessions(LIST));
  await tick();
  stdin.write(KEY.esc);
  await tick();
  const f = plain(lastFrame());
  assert.doesNotMatch(f, /Resume a session/, 'picker closed');
  // Only the list request was sent — no resume.
  assert.ok(!engine.sent.some((c) => c.kind === 'resume_session'));
});
