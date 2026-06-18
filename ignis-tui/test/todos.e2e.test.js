// e2e for the todo_write task-list panel: a `todos` event renders a checklist
// above the composer with status glyphs; a later event replaces it; an empty
// list hides the panel; a resume (transcript) clears it.
import test from 'node:test';
import assert from 'node:assert/strict';
import { renderApp, plain, tick, ev, transcript } from './harness.js';

const todo = (content, status, activeForm) =>
  activeForm ? { content, status, activeForm } : { content, status };

test('todos event renders a checklist with status glyphs', async () => {
  const { engine, lastFrame } = renderApp();
  await tick();
  engine.emit(
    ev('todos', {
      items: [
        todo('Build the parser', 'completed'),
        todo('Wire the tool', 'in_progress', 'Wiring the tool'),
        todo('Write tests', 'pending'),
      ],
    }),
  );
  await tick();
  const f = plain(lastFrame());
  assert.match(f, /Tasks 1\/3/);
  assert.match(f, /✓ Build the parser/);
  // in_progress prefers the present-continuous activeForm.
  assert.match(f, /◐ Wiring the tool/);
  assert.match(f, /◻ Write tests/);
});

test('a later todos event replaces the list', async () => {
  const { engine, lastFrame } = renderApp();
  await tick();
  engine.emit(ev('todos', { items: [todo('first', 'pending')] }));
  await tick();
  assert.match(plain(lastFrame()), /◻ first/);
  engine.emit(ev('todos', { items: [todo('second', 'in_progress')] }));
  await tick();
  const f = plain(lastFrame());
  assert.match(f, /◐ second/);
  assert.doesNotMatch(f, /first/);
});

test('an empty todos list hides the panel', async () => {
  const { engine, lastFrame } = renderApp();
  await tick();
  engine.emit(ev('todos', { items: [todo('only', 'pending')] }));
  await tick();
  assert.match(plain(lastFrame()), /only/);
  engine.emit(ev('todos', { items: [] }));
  await tick();
  assert.doesNotMatch(plain(lastFrame()), /Tasks/);
});

test('resume clears the todo panel', async () => {
  const { engine, lastFrame } = renderApp();
  await tick();
  engine.emit(ev('todos', { items: [todo('stale', 'pending')] }));
  await tick();
  assert.match(plain(lastFrame()), /stale/);
  engine.emit(transcript('sess-2', [{ kind: 'user', text: 'resumed' }]));
  await tick();
  assert.doesNotMatch(plain(lastFrame()), /stale/);
});
