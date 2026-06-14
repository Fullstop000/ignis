// `node --test` — pure markdown parser, no dependencies.
import test from 'node:test';
import assert from 'node:assert/strict';
import { parseInline, parseMarkdown } from '../src/markdown.js';

test('parseInline splits bold / italic / code, leaves plain text', () => {
  assert.deepEqual(parseInline('plain'), [{ text: 'plain' }]);
  assert.deepEqual(parseInline('a **b** c'), [{ text: 'a ' }, { text: 'b', bold: true }, { text: ' c' }]);
  assert.deepEqual(parseInline('x *y* z'), [{ text: 'x ' }, { text: 'y', italic: true }, { text: ' z' }]);
  assert.deepEqual(parseInline('run `ls -l` now'), [
    { text: 'run ' },
    { text: 'ls -l', code: true },
    { text: ' now' },
  ]);
});

test('parseInline leaves unterminated markers literal', () => {
  assert.deepEqual(parseInline('a **b'), [{ text: 'a **b' }]);
  assert.deepEqual(parseInline('a `b'), [{ text: 'a `b' }]);
});

test('parseMarkdown classifies block kinds', () => {
  const blocks = parseMarkdown('# Title\n\npara\n\n- one\n- two\n\n1. first\n\n---');
  const types = blocks.map((b) => b.type);
  assert.deepEqual(types, ['heading', 'blank', 'paragraph', 'blank', 'bullet', 'bullet', 'blank', 'ordered', 'blank', 'rule']);
  assert.equal(blocks[0].level, 1);
  assert.equal(blocks[7].marker, '1');
});

test('parseMarkdown captures fenced code, including unterminated (streaming)', () => {
  const closed = parseMarkdown('```js\nconst x = 1;\n```');
  assert.deepEqual(closed, [{ type: 'code', lang: 'js', lines: ['const x = 1;'] }]);
  const open = parseMarkdown('```\nhalf streamed');
  assert.equal(open[0].type, 'code');
  assert.deepEqual(open[0].lines, ['half streamed']);
});

test('parseMarkdown recognizes a table (header + separator + rows)', () => {
  const blocks = parseMarkdown('| Name | Age |\n|------|-----|\n| Ann | 30 |\n| Bob | 25 |\n\nafter');
  assert.equal(blocks[0].type, 'table');
  assert.deepEqual(blocks[0].header, ['Name', 'Age']);
  assert.deepEqual(blocks[0].rows, [['Ann', '30'], ['Bob', '25']]);
  // The paragraph after the blank line is not swallowed by the table.
  assert.equal(blocks.at(-1).type, 'paragraph');
});

test('a |pipe| line without a separator is not a table', () => {
  const blocks = parseMarkdown('a | b not a table');
  assert.equal(blocks[0].type, 'paragraph');
});

test('heading spans are inline-parsed', () => {
  const [h] = parseMarkdown('## A **bold** head');
  assert.equal(h.type, 'heading');
  assert.equal(h.level, 2);
  assert.deepEqual(h.spans, [{ text: 'A ' }, { text: 'bold', bold: true }, { text: ' head' }]);
});
