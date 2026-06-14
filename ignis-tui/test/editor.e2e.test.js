// e2e for composer editing: cursor movement, mid-line insert/delete, Ctrl+W/U,
// and input history — parity with ratatui's apply_edit_key.
import test from 'node:test';
import assert from 'node:assert/strict';
import { renderApp, plain, tick, KEY } from './harness.js';

async function submitText(stdin, s) {
  stdin.write(s);
  await tick();
  stdin.write(KEY.enter);
  await tick();
}

test('cursor moves left and inserts mid-line', async () => {
  const { engine, stdin } = renderApp();
  await tick();
  stdin.write('ac');
  await tick();
  stdin.write(KEY.left); // caret between a|c
  await tick();
  stdin.write('b'); // a b c
  await tick();
  stdin.write(KEY.enter);
  await tick();
  assert.deepEqual(engine.last(), { kind: 'submit', data: { text: 'abc' } });
});

test('Backspace deletes the char before the caret (mid-line)', async () => {
  // Ink reports the Backspace byte \x7f as key.delete; the app treats it as
  // delete-before-caret regardless.
  const { engine, stdin } = renderApp();
  await tick();
  stdin.write('abcd');
  await tick();
  stdin.write(KEY.left); // caret at ...c|d
  await tick();
  stdin.write(KEY.backspace); // removes c → ab|d
  await tick();
  stdin.write(KEY.enter);
  await tick();
  assert.deepEqual(engine.last(), { kind: 'submit', data: { text: 'abd' } });
});

test('Ctrl+W deletes the previous word, Ctrl+U clears the line', async () => {
  const { engine, stdin, lastFrame } = renderApp();
  await tick();
  stdin.write('hello world');
  await tick();
  stdin.write(KEY.ctrlW); // → "hello "
  await tick();
  stdin.write(KEY.enter);
  await tick();
  assert.deepEqual(engine.last(), { kind: 'submit', data: { text: 'hello ' } });

  stdin.write('junk');
  await tick();
  stdin.write(KEY.ctrlU);
  await tick();
  assert.doesNotMatch(plain(lastFrame()), /junk/);
});

test('Ctrl+A / Ctrl+E jump caret to start / end for insertion', async () => {
  const { engine, stdin } = renderApp();
  await tick();
  stdin.write('middle');
  await tick();
  stdin.write(KEY.ctrlA); // caret to start
  await tick();
  stdin.write('>'); // ">middle"
  await tick();
  stdin.write(KEY.ctrlE); // caret to end
  await tick();
  stdin.write('<'); // ">middle<"
  await tick();
  stdin.write(KEY.enter);
  await tick();
  assert.deepEqual(engine.last(), { kind: 'submit', data: { text: '>middle<' } });
});

test('a multi-line paste collapses to a chip and expands on submit', async () => {
  const { engine, lastFrame, stdin } = renderApp();
  await tick();
  stdin.write('before ');
  await tick();
  stdin.write('def f():\n    return 1\n'); // a multi-line paste (one chunk)
  await tick();
  const f = plain(lastFrame());
  assert.match(f, /before \[paste #1 · 3 lines\]/, 'paste collapsed to a chip');
  assert.doesNotMatch(f, /def f\(\)/, 'raw paste not shown inline');
  stdin.write(KEY.enter);
  await tick();
  assert.deepEqual(engine.last(), { kind: 'submit', data: { text: 'before def f():\n    return 1\n' } });
});

test('↑/↓ recall submitted input from history', async () => {
  const { stdin, lastFrame } = renderApp();
  await tick();
  await submitText(stdin, 'first');
  await submitText(stdin, 'second');
  stdin.write(KEY.up); // → "second"
  await tick();
  assert.match(plain(lastFrame()), /second/);
  stdin.write(KEY.up); // → "first"
  await tick();
  assert.match(plain(lastFrame()), /first/);
  stdin.write(KEY.down); // → "second"
  await tick();
  assert.match(plain(lastFrame()), /second/);
});
