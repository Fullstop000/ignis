// Standalone diff view for the Ink frontend.
//
// Parses the unified-diff body returned by `edit_file` and renders it as a
// line-numbered diff with per-token syntax highlighting layered over add/remove
// background tints — the same model the native ratatui TUI uses. +/- rows get
// full-width green/red background bars (Claude-Code dark-theme tints); the exact
// changed words get a stronger background so they stand out, while the code keeps
// its syntax colors.
import React from 'react';
import { Box, Text, useStdout } from 'ink';
import { diffWordsWithSpace } from 'diff';
import { toolDiffPreview } from './protocol.js';
import { highlightSpans } from './highlight.js';

const e = React.createElement;

// Claude-Code dark-theme diff tints: line-level bars + stronger changed-word bg.
const COLORS = {
  addBg: '#225c2b', // rgb(34,92,43)
  delBg: '#7a2936', // rgb(122,41,54)
  addWordBg: '#38a660', // rgb(56,166,96)
  delWordBg: '#b3596b', // rgb(179,89,107)
};

// Word-level diffing is synchronous and can freeze the TUI on very long lines
// (e.g., minified JSON, lockfiles). Fall back to plain row rendering above
// this combined old+new character threshold.
const MAX_WORD_DIFF_CHARS = 400;

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

  const cols = useTerminalWidth();
  const ext = fileExt(path);
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
      const isAdd = ln.kind === 'add';
      const rowBg = isAdd ? COLORS.addBg : COLORS.delBg;
      const wordBg = isAdd ? COLORS.addWordBg : COLORS.delWordBg;
      const segments = buildSegments(ln.text, ext, pairs.get(i), isAdd);
      // Pad to the terminal width so the background fills the whole row.
      const pad = ' '.repeat(Math.max(0, cols - prefix.length - ln.text.length));
      return e(
        Text,
        { key: `d${i}`, backgroundColor: rowBg },
        prefix,
        ...segments.map((sg, k) =>
          e(
            Text,
            {
              key: `s${k}`,
              color: sg.color,
              backgroundColor: sg.changed ? wordBg : rowBg,
              bold: sg.changed,
            },
            sg.text,
          ),
        ),
        pad,
      );
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

/** Read the terminal width from Ink's stdout context; default to 80 in tests. */
function useTerminalWidth() {
  const { stdout } = useStdout();
  return stdout?.columns || 80;
}

/** The file extension (lowercased, no dot) used to pick the highlight language. */
function fileExt(path) {
  if (!path) return '';
  const base = path.split('/').pop();
  const dot = base.lastIndexOf('.');
  return dot > 0 ? base.slice(dot + 1) : '';
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
 * Build the renderable segments for one +/- row: syntax colors (foreground)
 * intersected with the word-level diff (changed words get the stronger bg).
 * Returns `[{ text, color, changed }]`.
 */
function buildSegments(text, ext, pair, isAdd) {
  const syntax = highlightSpans(text, ext);
  let wordParts = [{ changed: false, text }];
  if (pair && pair.oldText.length + pair.newText.length <= MAX_WORD_DIFF_CHARS) {
    wordParts = wordRanges(pair.oldText, pair.newText, isAdd);
  }
  return mergeSegments(text, syntax, wordParts);
}

/**
 * The word-diff parts belonging to one side of a paired old/new line, in order.
 * Each `{ changed, text }`; concatenated, they reconstruct this side's line.
 */
function wordRanges(oldText, newText, isAdd) {
  const changes = diffWordsWithSpace(oldText, newText);
  const parts = [];
  for (const part of changes) {
    // Skip the opposite side's exclusive text so each row rebuilds its own line.
    if (isAdd ? part.removed : part.added) continue;
    if (!part.value) continue;
    parts.push({ changed: isAdd ? !!part.added : !!part.removed, text: part.value });
  }
  return parts.length ? parts : [{ changed: false, text: isAdd ? newText : oldText }];
}

/**
 * Tile two sequences over the same line text — syntax spans (`{ color, text }`)
 * and word-diff parts (`{ changed, text }`) — into segments carrying both, then
 * coalesce neighbours that share color+changed. Both sequences cover the whole
 * line, so we walk them in lockstep, cutting at whichever boundary comes first.
 */
function mergeSegments(text, syntax, wordParts) {
  const segs = [];
  let si = 0;
  let wi = 0;
  let so = 0;
  let wo = 0;
  let pos = 0;
  while (pos < text.length && si < syntax.length && wi < wordParts.length) {
    const s = syntax[si];
    const w = wordParts[wi];
    const sRemain = s.text.length - so;
    const wRemain = w.text.length - wo;
    const take = Math.min(sRemain, wRemain);
    if (take <= 0) {
      if (sRemain <= 0) {
        si++;
        so = 0;
      }
      if (wRemain <= 0) {
        wi++;
        wo = 0;
      }
      continue;
    }
    segs.push({ text: text.slice(pos, pos + take), color: s.color, changed: w.changed });
    pos += take;
    so += take;
    wo += take;
    if (so >= s.text.length) {
      si++;
      so = 0;
    }
    if (wo >= w.text.length) {
      wi++;
      wo = 0;
    }
  }
  // The two sequences should tile `text` exactly; if rounding ever leaves a
  // tail, emit it plain so the row still shows all of its content.
  if (pos < text.length) segs.push({ text: text.slice(pos), color: undefined, changed: false });

  const merged = [];
  for (const seg of segs) {
    const last = merged[merged.length - 1];
    if (last && last.color === seg.color && last.changed === seg.changed) last.text += seg.text;
    else merged.push({ ...seg });
  }
  return merged.length ? merged : [{ text, color: undefined, changed: false }];
}
