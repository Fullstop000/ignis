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

test('todo panel caps visible rows at 8 with a `+N more tasks` overflow line', async () => {
  // Long task lists shouldn't push the composer off the screen. Cap at 8
  // visible rows; the header counter still reflects the full total.
  // No in_progress here, so the window falls back to head-slice.
  const { engine, lastFrame } = renderApp();
  await tick();
  const items = [];
  for (let i = 1; i <= 12; i++) items.push(todo(`task ${i}`, 'pending'));
  engine.emit(ev('todos', { items }));
  await tick();
  const f = plain(lastFrame());
  assert.match(f, /Tasks 0\/12/, 'header reflects the full total');
  // First 8 visible.
  for (let i = 1; i <= 8; i++) {
    assert.match(f, new RegExp(`◻ task ${i}\\b`), `task ${i} should be visible`);
  }
  // Tasks 9..12 elided.
  for (let i = 9; i <= 12; i++) {
    assert.doesNotMatch(f, new RegExp(`◻ task ${i}\\b`), `task ${i} should be hidden`);
  }
  assert.match(f, /\+4 more tasks/, 'overflow line shows count of hidden tasks');
});

test('todo panel shows no overflow line at exactly 8 tasks', async () => {
  const { engine, lastFrame } = renderApp();
  await tick();
  const items = [];
  for (let i = 1; i <= 8; i++) items.push(todo(`task ${i}`, 'pending'));
  engine.emit(ev('todos', { items }));
  await tick();
  const f = plain(lastFrame());
  assert.match(f, /Tasks 0\/8/);
  assert.match(f, /◻ task 8\b/);
  assert.doesNotMatch(f, /\+\d+ more tasks/, 'no overflow line at exactly the cap');
  assert.doesNotMatch(f, /\d+ earlier/, 'no leading-overflow line at exactly the cap');
});

test('todo panel keeps the in_progress row visible when it sits past the head', async () => {
  // The system prompt instructs the model to write the full list up front
  // and advance the in_progress cursor through it (agent/mod.rs:130,
  // tools/todo_write.rs:71-72). On a long plan, the active row eventually
  // lands past index 8 — the window must anchor on it so the user can still
  // see what the agent is working on.
  const { engine, lastFrame } = renderApp();
  await tick();
  const items = [];
  for (let i = 1; i <= 9; i++) items.push(todo(`task ${i}`, 'completed'));
  items.push(todo('task 10', 'in_progress', 'Working on task 10'));
  for (let i = 11; i <= 12; i++) items.push(todo(`task ${i}`, 'pending'));
  engine.emit(ev('todos', { items }));
  await tick();
  const f = plain(lastFrame());
  assert.match(f, /Tasks 9\/12/, 'header reflects the full total');
  // Active row visible with its activeForm.
  assert.match(f, /◐ Working on task 10/, 'in_progress row must be in the visible window');
  // Some earlier rows hidden, summarised.
  assert.match(f, /\d+ earlier/, 'leading overflow line shows the count of hidden earlier rows');
  // Any tasks remaining after the window are summarised; here task 11/12
  // are visible but a `… N earlier` line must replace the head.
  assert.doesNotMatch(f, /◻ task 1\b/, 'far-earlier rows should be elided');
  assert.doesNotMatch(f, /◻ task 2\b/);
});
