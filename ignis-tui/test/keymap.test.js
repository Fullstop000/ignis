// Unit tests for the shortcut keymap — the single source of truth for chorded
// bindings. Pure: no Ink, no engine, just match/dispatch over a fake ctx.
import test from 'node:test';
import assert from 'node:assert/strict';
import { SHORTCUTS, dispatchShortcut } from '../src/keymap.js';

// Build a ctx whose every action records that it ran, so a dispatch can be
// asserted by which method fired.
function spyCtx() {
  const calls = [];
  const ctx = {};
  for (const name of [
    'cancelOrHint',
    'exitArm',
    'inject',
    'toggleReasoning',
    'lineStart',
    'lineEnd',
    'killLine',
    'killWord',
    'newline',
  ]) {
    ctx[name] = () => calls.push(name);
  }
  return { ctx, calls };
}

// chord byte → (key, ch) the way Ink delivers it, and the action it must run.
const CASES = [
  [{ ctrl: true }, 'c', 'cancelOrHint'],
  [{ ctrl: true }, 'd', 'exitArm'],
  [{ ctrl: true }, 's', 'inject'],
  [{ ctrl: true }, 'o', 'toggleReasoning'],
  [{ ctrl: true }, 'a', 'lineStart'],
  [{ ctrl: true }, 'e', 'lineEnd'],
  [{ ctrl: true }, 'u', 'killLine'],
  [{ ctrl: true }, 'w', 'killWord'],
  [{}, '\n', 'newline'], // Ink delivers Ctrl+J / LF as a lone '\n', ctrl=false
];

for (const [key, ch, action] of CASES) {
  test(`dispatch runs ${action} for its chord`, () => {
    const { ctx, calls } = spyCtx();
    assert.equal(dispatchShortcut(ch, key, ctx), true, 'chord is consumed');
    assert.deepEqual(calls, [action], 'exactly the bound action runs');
  });
}

test('a plain character is not a chord — dispatch is a no-op', () => {
  const { ctx, calls } = spyCtx();
  assert.equal(dispatchShortcut('x', {}, ctx), false, 'unmatched key not consumed');
  assert.deepEqual(calls, [], 'no action runs');
});

test('a ctrl chord that is not bound falls through', () => {
  const { ctx, calls } = spyCtx();
  // Ctrl+G has no binding; it must not be consumed.
  assert.equal(dispatchShortcut('g', { ctrl: true }, ctx), false);
  assert.deepEqual(calls, []);
});

test('a lone newline does not need ctrl to match', () => {
  const { ctx, calls } = spyCtx();
  // key.ctrl is false because Ink reports LF without the modifier.
  assert.equal(dispatchShortcut('\n', { ctrl: false }, ctx), true);
  assert.deepEqual(calls, ['newline']);
});

test('every shortcut has a unique id and a help string', () => {
  const ids = SHORTCUTS.map((s) => s.id);
  assert.equal(new Set(ids).size, ids.length, 'ids are unique');
  for (const s of SHORTCUTS) {
    assert.equal(typeof s.help, 'string', `${s.id} has help text`);
    assert.ok(s.help.length > 0, `${s.id} help is non-empty`);
  }
});
