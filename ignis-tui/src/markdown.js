// Minimal markdown → token-tree parser for the Ink frontend.
//
// Pure and dependency-free (no `ink`/`react`) so it runs under `node --test`.
// app.js turns the token tree into Ink <Text>/<Box> elements. We mirror only
// the subset the agent actually emits — headings, bold/italic/inline-code,
// fenced code, bullet/ordered lists, horizontal rules — not a full CommonMark
// engine (YAGNI; the ratatui renderer is the reference for "enough").

/** Inline scan → styled spans: [{ text, bold?, italic?, code? }]. */
export function parseInline(text) {
  const spans = [];
  let plain = '';
  let i = 0;
  const flush = () => {
    if (plain) spans.push({ text: plain });
    plain = '';
  };
  while (i < text.length) {
    // `code` — wins over emphasis and is not re-parsed inside.
    if (text[i] === '`') {
      const end = text.indexOf('`', i + 1);
      if (end > i) {
        flush();
        spans.push({ text: text.slice(i + 1, end), code: true });
        i = end + 1;
        continue;
      }
    }
    // **bold**
    if (text.startsWith('**', i)) {
      const end = text.indexOf('**', i + 2);
      if (end > i + 1) {
        flush();
        spans.push({ text: text.slice(i + 2, end), bold: true });
        i = end + 2;
        continue;
      }
    }
    // *italic* (single star, not the opening of **)
    if (text[i] === '*' && text[i + 1] !== '*') {
      const end = text.indexOf('*', i + 1);
      if (end > i) {
        flush();
        spans.push({ text: text.slice(i + 1, end), italic: true });
        i = end + 1;
        continue;
      }
    }
    plain += text[i];
    i++;
  }
  flush();
  return spans.length ? spans : [{ text: '' }];
}

/** Block scan → [{type, ...}]. Unterminated fences (mid-stream) close at EOF. */
export function parseMarkdown(text) {
  const lines = String(text ?? '').split('\n');
  const blocks = [];
  let inFence = false;
  let fenceLines = [];
  let fenceLang = '';
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const fence = line.match(/^```(.*)$/);
    if (inFence) {
      if (fence) {
        blocks.push({ type: 'code', lang: fenceLang, lines: fenceLines });
        inFence = false;
        fenceLines = [];
        fenceLang = '';
      } else {
        fenceLines.push(line);
      }
      continue;
    }
    if (fence) {
      inFence = true;
      fenceLang = fence[1].trim();
      continue;
    }
    if (/^\s*$/.test(line)) {
      blocks.push({ type: 'blank' });
      continue;
    }
    if (/^\s*([-*_])(\s*\1){2,}\s*$/.test(line)) {
      blocks.push({ type: 'rule' });
      continue;
    }
    const h = line.match(/^(#{1,6})\s+(.*)$/);
    if (h) {
      blocks.push({ type: 'heading', level: h[1].length, spans: parseInline(h[2]) });
      continue;
    }
    const b = line.match(/^(\s*)[-*+]\s+(.*)$/);
    if (b) {
      blocks.push({ type: 'bullet', indent: b[1].length, spans: parseInline(b[2]) });
      continue;
    }
    const o = line.match(/^(\s*)(\d+)[.)]\s+(.*)$/);
    if (o) {
      blocks.push({ type: 'ordered', indent: o[1].length, marker: o[2], spans: parseInline(o[3]) });
      continue;
    }
    blocks.push({ type: 'paragraph', spans: parseInline(line) });
  }
  // A fence still open at end-of-input (streaming, or malformed) renders as code.
  if (inFence) blocks.push({ type: 'code', lang: fenceLang, lines: fenceLines });
  return blocks;
}
