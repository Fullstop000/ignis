// Per-line syntax highlighting for the Ink diff view, via `lowlight`
// (highlight.js core). Returns ordered `{ color, text }` spans so the caller
// can layer syntax colors over the diff's add/remove background tints — the
// same model the native ratatui TUI uses (`src/console/highlight.rs`, syntect).
// The palette below approximates native's so the two frontends look close (the
// tokenizers differ, so it isn't pixel-identical). Unknown extensions or any
// failure fall back to one uncolored span, exactly like the native side.
import { createLowlight, common } from 'lowlight';

const lowlight = createLowlight(common);

// File extension -> highlight.js language. Mirrors the spirit of the native
// `find_syntax_by_extension`; anything not listed renders plain.
const EXT_TO_LANG = {
  rs: 'rust',
  ts: 'typescript', tsx: 'typescript', mts: 'typescript', cts: 'typescript',
  js: 'javascript', jsx: 'javascript', mjs: 'javascript', cjs: 'javascript',
  py: 'python', go: 'go', json: 'json', toml: 'ini',
  yml: 'yaml', yaml: 'yaml', sh: 'bash', bash: 'bash',
  md: 'markdown', markdown: 'markdown',
  c: 'c', h: 'c', cpp: 'cpp', cc: 'cpp', hpp: 'cpp', cxx: 'cpp',
  java: 'java', rb: 'ruby', php: 'php',
  css: 'css', scss: 'scss', less: 'less', sql: 'sql',
  html: 'xml', xml: 'xml', svg: 'xml',
  lua: 'lua', kt: 'kotlin', swift: 'swift',
};

// base16-ocean.dark default foreground (base05). Unscoped tokens inside a
// highlighted line get this — matching what native syntect emits for the things
// it leaves uncolored: plain identifiers, punctuation, types and method names.
const DEFAULT_FG = '#c0c5ce';

// base16-ocean.dark, keyed by the highlight.js scope (className minus `hljs-`).
// Tuned to reproduce what native syntect actually emits (verified against
// `src/console/highlight.rs`): keywords purple, function names blue, strings
// green, numbers/constants orange, variables red, comments grey. Types,
// built-ins and method names are deliberately left at DEFAULT_FG, as native
// does not color them.
const SCOPE_COLORS = {
  keyword: '#b48ead', 'selector-tag': '#b48ead',
  title: '#8fa1b3', function: '#8fa1b3',
  string: '#a3be8c', symbol: '#a3be8c', bullet: '#a3be8c', addition: '#a3be8c',
  number: '#d08770', literal: '#d08770', meta: '#d08770',
  'template-variable': '#d08770', subst: '#d08770',
  variable: '#bf616a', attribute: '#bf616a', attr: '#bf616a',
  tag: '#bf616a', name: '#bf616a', regexp: '#bf616a', link: '#bf616a',
  'selector-id': '#bf616a', 'selector-class': '#bf616a', deletion: '#bf616a',
  section: '#ebcb8b',
  comment: '#65737e', quote: '#65737e',
};

// Skip pathological lines (minified bundles, lockfiles) — the synchronous
// highlight would be wasted work and word-diff already bails at a similar size.
const MAX_HIGHLIGHT_CHARS = 2000;

/**
 * Highlight one line of code for the given file extension.
 * Returns an array of `{ color, text }` spans whose `text` concatenated equals
 * the input. `color` is a hex string, or `undefined` for unscoped text.
 */
export function highlightSpans(text, ext) {
  const lang = EXT_TO_LANG[(ext || '').toLowerCase()];
  if (!text || !lang || text.length > MAX_HIGHLIGHT_CHARS) {
    return [{ color: undefined, text: text ?? '' }];
  }
  try {
    const tree = lowlight.highlight(lang, text);
    const spans = [];
    walk(tree.children, DEFAULT_FG, spans);
    // Coalesce adjacent same-color spans so we emit the fewest `<Text>` nodes.
    return coalesce(spans, text);
  } catch {
    return [{ color: undefined, text }];
  }
}

/** The color for a hast element's scopes, or the inherited color if none map. */
function scopeColor(node, inherited) {
  const classes = node.properties?.className;
  if (Array.isArray(classes)) {
    for (const c of classes) {
      const key = c.startsWith('hljs-') ? c.slice(5) : c;
      if (SCOPE_COLORS[key]) return SCOPE_COLORS[key];
    }
  }
  return inherited;
}

/** Depth-first flatten of hast nodes into `{ color, text }` spans. */
function walk(nodes, color, out) {
  for (const node of nodes) {
    if (node.type === 'text') {
      if (node.value) out.push({ color, text: node.value });
    } else if (node.type === 'element' && node.children) {
      walk(node.children, scopeColor(node, color), out);
    }
  }
}

/** Merge neighbouring spans of the same color; fall back to plain on mismatch. */
function coalesce(spans, original) {
  const out = [];
  for (const s of spans) {
    const last = out[out.length - 1];
    if (last && last.color === s.color) last.text += s.text;
    else out.push({ color: s.color, text: s.text });
  }
  if (!out.length) return [{ color: undefined, text: original }];
  return out;
}
