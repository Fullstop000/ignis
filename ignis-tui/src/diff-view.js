// Standalone diff view for the Ink frontend.
//
// Parses the unified-diff body returned by `edit_file` and renders it as a
// line-numbered, word-level diff. The entire +/- row is tinted green/red;
// the exact changed words are also bold so the eye can see *what* changed
// without reading both lines side by side.
import React from 'react';
import { Box, Text } from 'ink';
import { diffWords } from 'diff';
import { toolDiffPreview } from './protocol.js';

const e = React.createElement;

/**
 * Render an `edit_file` result as a Claude-Code-style diff view.
 *
 * Props:
 *   - `content`: unified-diff string from the engine
 *   - `path`:    file path shown in the header
 */
export default function DiffView({ content, path }) {
  const { adds, dels, lines, more } = toolDiffPreview(content);
  const header = e(
    Text,
    { key: 'h', color: 'green' },
    `◆ Edited ${path || ''} (+${adds} -${dels})`,
  );
  if (!lines.length) return header;

  const maxLn = lines.reduce(
    (m, ln) => (ln.lineNo != null && ln.lineNo > m ? ln.lineNo : m),
    0,
  );
  const gutterW = Math.max(2, String(maxLn).length);
  const pairs = pairRows(lines);

  const body = lines.map((ln, i) => {
    if (ln.kind === 'gap') {
      // A `⋮` aligned with the gutter — visually the same column as the line
      // numbers, so the diff reads as a single column of marks down the side.
      return e(
        Text,
        { key: `d${i}`, dimColor: true },
        `  ${'⋮'.padStart(gutterW)}     `,
      );
    }

    const num = ln.lineNo == null ? ''.padStart(gutterW) : String(ln.lineNo).padStart(gutterW);
    const sign = ln.kind === 'add' ? '+' : ln.kind === 'del' ? '-' : ' ';
    const prefix = `  ${num}  ${sign}  `;

    if (ln.kind === 'add' || ln.kind === 'del') {
      const pair = pairs.get(i);
      const baseColor = ln.kind === 'add' ? 'green' : 'red';
      const children = pair
        ? renderWordDiff(pair.oldText, pair.newText, ln.kind === 'add', baseColor)
        : [ln.text];
      return e(Text, { key: `d${i}`, color: baseColor }, prefix, ...children);
    }

    return e(
      Text,
      { key: `d${i}`, dimColor: true },
      `${prefix}${ln.text}`,
    );
  });

  if (more) {
    body.push(
      e(
        Text,
        { key: 'more', dimColor: true },
        `  ${''.padStart(gutterW)}     … +${more} more lines`,
      ),
    );
  }

  return e(Box, { flexDirection: 'column' }, [header, ...body]);
}

/**
 * Pair consecutive deletion rows with the consecutive addition rows that
 * follow them so we can run a word-level diff for each matching old/new line.
 * Returns a map of row index -> { oldText, newText } for both sides.
 */
function pairRows(lines) {
  const pairs = new Map();
  let i = 0;
  while (i < lines.length) {
    if (lines[i].kind !== 'del') {
      i++;
      continue;
    }
    const delStart = i;
    while (i < lines.length && lines[i].kind === 'del') i++;
    const addStart = i;
    while (i < lines.length && lines[i].kind === 'add') i++;
    const delCount = addStart - delStart;
    const addCount = i - addStart;
    const n = Math.min(delCount, addCount);
    for (let k = 0; k < n; k++) {
      const oldText = lines[delStart + k].text;
      const newText = lines[addStart + k].text;
      pairs.set(delStart + k, { oldText, newText });
      pairs.set(addStart + k, { oldText, newText });
    }
  }
  return pairs;
}

/**
 * Convert a word diff between an old and new line into Ink `<Text>` children.
 * Equal parts inherit the base row color; added/removed parts are bold so the
 * changed words pop.
 */
function renderWordDiff(oldText, newText, isAdd, baseColor) {
  const changes = diffWords(oldText, newText);
  return changes
    .map((part, idx) => {
      // For an addition row we render unchanged + inserted text; for a deletion
      // row we render unchanged + removed text. The opposite side's exclusive
      // text is omitted so each row reconstructs its own line content.
      if (isAdd ? part.removed : part.added) return null;
      if (!part.value) return null;
      const changed = isAdd ? part.added : part.removed;
      if (!changed) {
        // Unchanged segment: just a string so it inherits the parent color.
        return part.value;
      }
      return e(Text, { key: `w${idx}`, bold: true, color: baseColor }, part.value);
    })
    .filter(Boolean);
}
