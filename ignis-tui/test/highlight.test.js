// Unit coverage for `highlightSpans` — mirrors the native `highlight.rs` tests.
import test from 'node:test';
import assert from 'node:assert/strict';
import { highlightSpans } from '../src/highlight.js';

test('highlights a Rust keyword line into multiple colored spans', () => {
  const spans = highlightSpans('fn main() {}', 'rs');
  assert.ok(spans.length > 1, 'expected multiple spans');
  const colors = new Set(spans.map((s) => s.color));
  assert.ok(colors.size > 1, 'expected more than one color');
  assert.equal(spans.map((s) => s.text).join(''), 'fn main() {}', 'round-trips the text');
});

test('unknown extension falls back to one uncolored span', () => {
  const spans = highlightSpans('some text', 'no-such-ext');
  assert.deepEqual(spans, [{ color: undefined, text: 'some text' }]);
});

test('empty input yields a single empty span', () => {
  assert.deepEqual(highlightSpans('', 'rs'), [{ color: undefined, text: '' }]);
});

test('over-cap lines skip highlighting but keep their text', () => {
  const long = 'x'.repeat(3000);
  const spans = highlightSpans(long, 'rs');
  assert.deepEqual(spans, [{ color: undefined, text: long }]);
});

test('matches the base16 string color for a quoted literal', () => {
  const spans = highlightSpans('"hello"', 'js');
  assert.ok(
    spans.some((s) => s.color === '#a3be8c' && s.text.includes('hello')),
    'string literal should be base16 green',
  );
});

test('matches native: function names are base16 blue (base0D)', () => {
  const spans = highlightSpans('fn greet() {}', 'rs');
  const fn = spans.find((s) => s.text === 'greet');
  assert.ok(fn, 'function name is its own span');
  assert.equal(fn.color, '#8fa1b3', 'function name uses base0D blue, like native syntect');
});

test('matches native: types and methods stay at the default fg, not colored', () => {
  // Native syntect leaves type/builtin/method identifiers at base05 (#c0c5ce);
  // the Ink palette must not over-color them (they were orange/yellow before).
  const spans = highlightSpans('let m: HashMap = 1;', 'rs');
  const ty = spans.find((s) => s.text.includes('HashMap'));
  assert.ok(ty, 'type span present');
  assert.equal(ty.color, '#c0c5ce', 'type uses base05 default fg, not a syntax color');
});
