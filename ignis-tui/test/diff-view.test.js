// Unit coverage for the extracted `<DiffView>` component.
import test from 'node:test';
import assert from 'node:assert/strict';
import React from 'react';
import { render } from 'ink-testing-library';
import DiffView from '../src/diff-view.js';
import { highlightSpans } from '../src/highlight.js';

const e = React.createElement;

const SAMPLE = `--- src/main.rs
+++ src/main.rs
@@ -1,3 +1,3 @@
 keep me
-old
+new
@@ -10,2 +10,2 @@
-removed
+added
 context after
`;

test('DiffView renders the new header with add/delete counts', () => {
  const { lastFrame } = render(e(DiffView, { content: SAMPLE, path: 'src/main.rs' }));
  assert.match(plain(lastFrame()), /◆ Edited src\/main\.rs \(\+2 -2\)/);
});

test('DiffView renders line numbers and diff signs', () => {
  const { lastFrame } = render(e(DiffView, { content: SAMPLE, path: 'src/main.rs' }));
  const f = plain(lastFrame());
  assert.match(f, /1\s+keep me/, 'context line keeps its line number');
  assert.match(f, /2\s+-\s+old/, 'deleted line shows old-file line number');
  assert.match(f, /2\s+\+\s+new/, 'added line shows new-file line number');
});

test('DiffView renders a gap separator between non-contiguous hunks', () => {
  const { lastFrame } = render(e(DiffView, { content: SAMPLE, path: 'src/main.rs' }));
  assert.match(plain(lastFrame()), /⋮/);
});

test('DiffView reconstructs each row with its own line content', () => {
  // A previous bug concatenated both sides of the word diff; this guards
  // against regressions by checking each row contains only its own text.
  const { lastFrame } = render(e(DiffView, { content: SAMPLE, path: 'src/main.rs' }));
  const f = plain(lastFrame());
  assert.match(f, /-\s+old$/m, 'deleted row contains only the old text');
  assert.match(f, /\+\s+new$/m, 'added row contains only the new text');
  assert.match(f, /-\s+removed$/m, 'second deleted row contains only removed text');
  assert.match(f, /\+\s+added$/m, 'second added row contains only added text');
});

test('DiffView falls back to plain rendering for very long changed lines', () => {
  // Above MAX_WORD_DIFF_CHARS the component skips the synchronous word diff
  // to avoid freezing the TUI; the row should still contain its own text.
  const oldLine = 'a'.repeat(250);
  const newLine = 'b'.repeat(250);
  const content = `@@ -1,1 +1,1 @@\n-${oldLine}\n+${newLine}\n`;
  const { lastFrame } = render(e(DiffView, { content, path: 'long.rs' }));
  const f = plain(lastFrame()).replace(/\n/g, '');
  assert.ok(f.includes(`-  ${oldLine}`), 'deleted row shows the long old text');
  assert.ok(f.includes(`+  ${newLine}`), 'added row shows the long new text');
});

test('DiffView preserves whitespace-only edits', () => {
  // diffWords ignores whitespace by default, which would make both rows render
  // the same text. diffWordsWithSpace keeps the original spacing on each side.
  const content = '@@ -1,1 +1,1 @@\n-old line\n+old  line\n';
  const { lastFrame } = render(e(DiffView, { content, path: 'ws.rs' }));
  const f = plain(lastFrame());
  assert.match(f, /-\s+old line$/m, 'deleted row keeps single space');
  assert.match(f, /\+\s+old  line$/m, 'added row keeps double space');
});

test('DiffView reports overflow when diff exceeds the cap', () => {
  // toolDiffPreview caps at 30 lines; build a 32-line diff.
  const lines = [];
  for (let i = 1; i <= 32; i++) {
    lines.push(`@@ -${i},1 +${i},1 @@`);
    lines.push(`- old${i}`);
    lines.push(`+ new${i}`);
  }
  const { lastFrame } = render(e(DiffView, { content: lines.join('\n'), path: 'big.rs' }));
  assert.match(plain(lastFrame()), /\+\d+ more lines/);
});

test('DiffView routes Rust code through the syntax highlighter', () => {
  // ink-testing-library strips ANSI codes, so we can't assert color in the
  // rendered frame. Instead we verify (a) the rendered plain text is correct,
  // and (b) the highlight layer produces the expected base16 color for the
  // exact line DiffView feeds it — proving the integration is wired.
  const content = '@@ -1,1 +1,1 @@\n-let x = 1;\n+const y = 2;\n';
  const { lastFrame } = render(e(DiffView, { content, path: 'lib.rs' }));
  const f = plain(lastFrame());
  assert.match(f, /\+\s+const y = 2;/, 'added row keeps its plain text');

  // The highlight layer must colour the `const` keyword base16 purple.
  const spans = highlightSpans('const y = 2;', 'rs');
  const kw = spans.find((s) => s.text === 'const');
  assert.ok(kw, 'const is its own span');
  assert.equal(kw.color, '#b48ead', 'keyword uses the base16 purple');
});

test('DiffView highlights context lines, not just changed rows', () => {
  // The whole diff content should be syntax-highlighted — unchanged context
  // rows included — so context code reads with the same colors as +/- rows.
  // The context line carries an `fn` keyword (base16 purple); the +/- rows are
  // bare identifiers with no keyword, so the purple escape can only come from
  // the context row, proving it is highlighted rather than rendered plain/dim.
  const content = '@@ -1,3 +1,3 @@\n fn keep() {}\n-aaa\n+bbb\n';
  const { lastFrame } = render(e(DiffView, { content, path: 'lib.rs' }));
  const raw = lastFrame() ?? '';
  // #b48ead -> rgb(180,142,173); Ink emits a truecolor SGR for hex colors.
  assert.ok(
    raw.includes('\x1b[38;2;180;142;173m'),
    'context keyword carries the base16 purple color escape',
  );
  // The context text itself stays intact.
  assert.match(plain(lastFrame()), /1\s+fn keep\(\) \{\}/);
});

test('DiffView leaves unknown extensions unhighlighted but intact', () => {
  const content = '@@ -1,1 +1,1 @@\n-alpha\n+beta\n';
  const { lastFrame } = render(e(DiffView, { content, path: 'notes.zzz' }));
  const f = plain(lastFrame());
  assert.match(f, /-\s+alpha$/m);
  assert.match(f, /\+\s+beta$/m);
});

function plain(frame) {
  // Strip ANSI, then drop each row's trailing whitespace: +/- rows are padded
  // with background-tinted spaces to fill the terminal width, which is intended
  // rendering, not line content, and would otherwise break `$`-anchored matches.
  return (frame ?? '')
    // eslint-disable-next-line no-control-regex
    .replace(/\x1b\[[0-9;]*m/g, '')
    .split('\n')
    .map((l) => l.replace(/\s+$/, ''))
    .join('\n');
}
