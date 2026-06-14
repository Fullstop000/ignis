// The Ink UI. Written with `React.createElement` (aliased `e`) rather than JSX
// so it runs under plain `node` with no build/transform step — only `ink` +
// `react` need installing.
//
// PR #174: the frontend renders the streaming transcript (user / assistant /
// tool / inject / notice blocks) with markdown, an `ask_user` picker
// (single- and multi-select, free-text "Other", multi-question), and a
// one-line composer; it sends submit / cancel (Ctrl+C) / inject (Ctrl+S) /
// reply. Not yet at parity: native scrollback paging and /connect-style
// text-input (masked) prompts. Pure logic — the Outbound→state reducer, the
// command/answer builders, and the markdown parser — lives in protocol.js /
// markdown.js and is unit-tested via `node --test`.
import React, { useState, useEffect } from 'react';
import { Box, Text, useInput, useApp } from 'ink';
import {
  initialState,
  reduceOutbound,
  submit,
  cancel,
  inject,
  reply,
  newSession,
  setModel,
  parseSlash,
  pickSingle,
  pickMulti,
  answered,
  answerCancelled,
  toolArgsSummary,
  toolOutputPreview,
} from './protocol.js';
import { parseMarkdown } from './markdown.js';

const e = React.createElement;

export default function App({ engine }) {
  const { exit } = useApp();
  const [state, setState] = useState(initialState());
  // Composer text + caret in one object so every edit is an atomic functional
  // update (no stale-closure splits under fast input).
  const [comp, setComp] = useState({ text: '', cursor: 0 });
  const [history, setHistory] = useState([]);
  const [histIdx, setHistIdx] = useState(-1); // -1 = editing live (not recalling)
  const [modelPickerOpen, setModelPickerOpen] = useState(false);

  useEffect(() => {
    engine.onFrame((frame) => setState((s) => reduceOutbound(s, frame)));
    engine.onClose(() => exit());
  }, [engine, exit]);

  const req = state.request;
  const clearRequest = () => setState((s) => ({ ...s, request: null }));
  const resetComposer = () => {
    setComp({ text: '', cursor: 0 });
    setHistIdx(-1);
  };

  // Locally-handled slash commands. Returns true if handled here (so the line
  // is NOT submitted to the engine); false falls through to a normal submit.
  const handleSlash = (slash) => {
    switch (slash.name) {
      case 'clear':
        engine.send(newSession());
        // Clear the local transcript; the engine re-snapshots with the new id.
        setState((s) => ({ ...s, blocks: [], stream: null, turns: 0, usage: null }));
        return true;
      case 'model':
        setModelPickerOpen(true);
        return true;
      default:
        return false; // /compact + unknown → submit (engine / LLM handles)
    }
  };

  useInput((ch, key) => {
    // While a picker is open it owns all keys (PickerFlow / ModelPicker each
    // have their own useInput).
    if (req || modelPickerOpen) return;

    if (key.ctrl && ch === 'c') {
      if (state.status !== 'idle') engine.send(cancel());
      else {
        engine.close();
        exit();
      }
      return;
    }
    if (key.ctrl && ch === 's') {
      if (comp.text.trim()) {
        engine.send(inject(comp.text));
        resetComposer();
      }
      return;
    }
    if (key.return) {
      const t = comp.text;
      if (!t.trim()) return;
      // Slash dispatch: a few commands are handled locally; everything else
      // (plain prompts, /compact, unknown slashes) is submitted to the engine.
      const slash = parseSlash(t);
      if (slash && handleSlash(slash)) {
        resetComposer();
        return;
      }
      engine.send(submit(t));
      setHistory((h) => [...h, t]);
      resetComposer();
      return;
    }
    // History recall (↑/↓), matching ratatui.
    if (key.upArrow) {
      if (!history.length) return;
      const ni = histIdx < 0 ? history.length - 1 : Math.max(0, histIdx - 1);
      setHistIdx(ni);
      setComp({ text: history[ni], cursor: history[ni].length });
      return;
    }
    if (key.downArrow) {
      if (histIdx < 0) return;
      const ni = histIdx + 1;
      if (ni >= history.length) resetComposer();
      else {
        setHistIdx(ni);
        setComp({ text: history[ni], cursor: history[ni].length });
      }
      return;
    }
    // Cursor movement + line editing (Ctrl+A/E/U/W), matching ratatui's apply_edit_key.
    if (key.leftArrow) {
      setComp((c) => ({ ...c, cursor: Math.max(0, c.cursor - 1) }));
      return;
    }
    if (key.rightArrow) {
      setComp((c) => ({ ...c, cursor: Math.min(c.text.length, c.cursor + 1) }));
      return;
    }
    if (key.ctrl && ch === 'a') {
      setComp((c) => ({ ...c, cursor: 0 }));
      return;
    }
    if (key.ctrl && ch === 'e') {
      setComp((c) => ({ ...c, cursor: c.text.length }));
      return;
    }
    if (key.ctrl && ch === 'u') {
      resetComposer();
      return;
    }
    if (key.ctrl && ch === 'w') {
      setComp(deleteWordBefore);
      return;
    }
    // Delete before the caret. Ink maps the common Backspace byte (\x7f) to
    // `key.delete` (and \x08 to `key.backspace`), and can't distinguish it from
    // the Delete key — so both flags do backspace, the overwhelmingly common case.
    if (key.backspace || key.delete) {
      setComp((c) =>
        c.cursor > 0 ? { text: c.text.slice(0, c.cursor - 1) + c.text.slice(c.cursor), cursor: c.cursor - 1 } : c,
      );
      return;
    }
    if (ch && !key.ctrl && !key.meta) {
      setComp((c) => ({ text: c.text.slice(0, c.cursor) + ch + c.text.slice(c.cursor), cursor: c.cursor + ch.length }));
    }
  });

  const children = [];
  if (state.blocks.length === 0 && state.stream == null && !req) {
    children.push(e(Welcome, { key: 'welcome' }));
  }
  state.blocks.forEach((b, i) => children.push(e(Block, { key: `b${i}`, block: b })));
  if (state.stream != null) children.push(e(Markdown, { key: 'stream', text: state.stream }));
  if (req) {
    // Key by request id so a fresh request resets the flow's internal state.
    children.push(e(PickerFlow, { key: `picker-${req.id}`, req, engine, onDone: clearRequest }));
  } else if (modelPickerOpen) {
    children.push(
      e(ModelPicker, {
        key: 'model-picker',
        models: state.models,
        current: { provider: state.provider, model: state.model },
        onPick: (m) => {
          engine.send(setModel(m.provider, m.model));
          setModelPickerOpen(false);
        },
        onCancel: () => setModelPickerOpen(false),
      }),
    );
  } else {
    children.push(e(Composer, { key: 'composer', text: comp.text, cursor: comp.cursor, status: state.status }));
  }
  children.push(e(Footer, { key: 'footer', state }));
  return e(Box, { flexDirection: 'column' }, children);
}

/** Ctrl+W: delete the word (and any spaces) before the caret. */
function deleteWordBefore(c) {
  let i = c.cursor;
  while (i > 0 && c.text[i - 1] === ' ') i--;
  while (i > 0 && c.text[i - 1] !== ' ') i--;
  return { text: c.text.slice(0, i) + c.text.slice(c.cursor), cursor: i };
}

function Block({ block }) {
  switch (block.kind) {
    case 'user':
      return e(Text, { color: 'magenta' }, `▌ ${block.text}`);
    case 'assistant':
      return e(Markdown, { text: block.text });
    case 'tool':
      return e(ToolBlock, { block });
    case 'inject':
      return e(Text, { color: 'cyan' }, `↳ ${block.text}`);
    default:
      return e(Text, { dimColor: true }, block.text);
  }
}

// Tool call: a `● name(args)` header (yellow pending / green done / red error),
// with the result preview indented under a `╰` gutter once it completes.
function ToolBlock({ block }) {
  const isError = block.result?.is_error;
  const headerColor = !block.done ? 'yellow' : isError ? 'red' : 'green';
  const header = e(
    Text,
    { key: 'h', color: headerColor },
    `● ${block.name}(${toolArgsSummary(block.args)})${block.done ? '' : ' …'}`,
  );
  if (!block.done || !block.result) return header;
  const { lines, more } = toolOutputPreview(block.result.content, isError);
  if (!lines.length) return header;
  const body = lines.map((ln, i) =>
    e(Text, { key: `l${i}`, color: isError ? 'red' : 'gray' }, `  ${i === 0 ? '╰ ' : '  '}${ln}`),
  );
  if (more) body.push(e(Text, { key: 'more', dimColor: true }, `    … +${more} more lines`));
  return e(Box, { flexDirection: 'column' }, [header, ...body]);
}

// Statusline footer: provider/model · cwd · turns · context tokens — fed by the
// startup snapshot and the `usage`/`turn_start` events (engine-owned data).
function Footer({ state }) {
  const segs = [];
  if (state.provider || state.model) segs.push(`${state.provider || '?'}/${state.model || '?'}`);
  if (state.cwd) segs.push(baseName(state.cwd));
  segs.push(`${state.turns} turn${state.turns === 1 ? '' : 's'}`);
  if (state.usage && state.usage.input_tokens != null) segs.push(`${state.usage.input_tokens} tok`);
  if (!segs.length) return null;
  return e(Box, { marginTop: 1 }, e(Text, { dimColor: true }, segs.join('  ·  ')));
}

function baseName(p) {
  const parts = String(p).replace(/[/\\]+$/, '').split(/[/\\]/);
  return parts[parts.length - 1] || p;
}

function Welcome() {
  return e(
    Box,
    { flexDirection: 'column', marginBottom: 1 },
    e(Text, { bold: true, color: 'magenta' }, 'ignis'),
    e(Text, { dimColor: true }, 'Type a message and press Enter · Ctrl+C to cancel a turn or exit'),
  );
}

function Composer({ text, cursor, status }) {
  // Faux block caret at the cursor position — Ink hides the real cursor inline.
  const before = text.slice(0, cursor);
  const at = text.slice(cursor, cursor + 1) || ' ';
  const after = text.slice(cursor + 1);
  return e(
    Box,
    { marginTop: 1 },
    e(Text, { color: 'gray' }, status === 'idle' ? '› ' : '… '),
    e(Text, null, before),
    e(Text, { inverse: true }, at),
    e(Text, null, after),
  );
}

// ── Picker (ask_user): single/multi-select, free-text "Other", multi-question ──

function PickerFlow({ req, engine, onDone }) {
  const [qIdx, setQIdx] = useState(0);
  const [cursor, setCursor] = useState(0);
  const [selected, setSelected] = useState([]); // selection-order indices (multi)
  const [acc, setAcc] = useState([]); // completed PickerAnswers for prior questions
  const [other, setOther] = useState(''); // free-text buffer for the "Other" row

  const q = req.questions[qIdx] ?? {};
  const opts = q.options ?? [];
  const otherIdx = q.allow_other ? opts.length : -1;
  const rowCount = opts.length + (q.allow_other ? 1 : 0);
  const labelAt = (i) => (i === otherIdx ? other : opts[i]?.label);

  const finish = (pick) => {
    const picks = [...acc, pick];
    if (qIdx + 1 < req.questions.length) {
      setAcc(picks);
      setQIdx(qIdx + 1);
      setCursor(0);
      setSelected([]);
      setOther('');
    } else {
      engine.send(reply(req.id, answered(picks)));
      onDone();
    }
  };

  useInput((ch, key) => {
    // Precedence matters: special keys are handled before free-text capture,
    // because Enter/Tab/etc. carry a truthy `ch` that would otherwise be typed
    // into the "Other" buffer instead of confirming.
    if (key.escape) {
      engine.send(reply(req.id, answerCancelled()));
      onDone();
      return;
    }
    if (key.upArrow) {
      setCursor((c) => Math.max(0, c - 1));
      return;
    }
    if (key.downArrow) {
      setCursor((c) => Math.min(Math.max(rowCount - 1, 0), c + 1));
      return;
    }
    if (key.return) {
      if (q.multi_select) {
        const idxs = selected.length ? selected : [cursor];
        const labels = idxs.map(labelAt).filter((l) => l != null && l !== '');
        if (labels.length) finish(pickMulti(labels));
      } else {
        const label = labelAt(cursor);
        if (label != null && label !== '') finish(pickSingle(label));
      }
      return;
    }
    // Space toggles in multi-select (reserved, so it can't type into "Other").
    if (q.multi_select && ch === ' ') {
      setSelected((s) => (s.includes(cursor) ? s.filter((i) => i !== cursor) : [...s, cursor]));
      return;
    }
    // The "Other" row captures printable free-text.
    const onOther = cursor === otherIdx;
    if (onOther && (key.backspace || key.delete)) {
      setOther((t) => t.slice(0, -1));
      return;
    }
    if (onOther && ch && ch.charCodeAt(0) >= 0x20 && !key.ctrl && !key.meta) {
      setOther((t) => t + ch);
    }
  });

  const rows = [];
  if (req.questions.length > 1) {
    rows.push(e(Text, { key: 'prog', dimColor: true }, `(${qIdx + 1}/${req.questions.length})`));
  }
  rows.push(e(Text, { key: 'q', bold: true }, q.question ?? ''));
  opts.forEach((o, i) => rows.push(e(PickerRow, { key: `o${i}`, label: o.label, focused: i === cursor, checked: selected.includes(i), multi: q.multi_select })));
  if (q.allow_other) {
    const label = other ? `Other: ${other}` : 'Other (type custom)…';
    rows.push(e(PickerRow, { key: 'other', label, focused: cursor === otherIdx, checked: selected.includes(otherIdx), multi: q.multi_select }));
  }
  rows.push(
    e(
      Text,
      { key: 'hint', dimColor: true },
      q.multi_select ? '↑/↓ move · space toggle · enter confirm · esc cancel' : '↑/↓ select · enter confirm · esc cancel',
    ),
  );
  return e(Box, { flexDirection: 'column', marginTop: 1, borderStyle: 'round', paddingX: 1 }, rows);
}

// Local `/model` picker: single-select over the engine-supplied model list.
function ModelPicker({ models, current, onPick, onCancel }) {
  const start = Math.max(
    0,
    models.findIndex((m) => m.provider === current.provider && m.model === current.model),
  );
  const [cursor, setCursor] = useState(start);

  useInput((ch, key) => {
    if (key.escape) {
      onCancel();
      return;
    }
    if (key.upArrow) {
      setCursor((c) => Math.max(0, c - 1));
      return;
    }
    if (key.downArrow) {
      setCursor((c) => Math.min(Math.max(models.length - 1, 0), c + 1));
      return;
    }
    if (key.return && models[cursor]) onPick(models[cursor]);
  });

  const rows = [e(Text, { key: 'q', bold: true }, 'Switch model')];
  if (!models.length) {
    rows.push(e(Text, { key: 'empty', dimColor: true }, 'No models configured.'));
  }
  models.forEach((m, i) =>
    rows.push(
      e(PickerRow, { key: `m${i}`, label: `${m.provider}/${m.model}`, focused: i === cursor, checked: false, multi: false }),
    ),
  );
  rows.push(e(Text, { key: 'hint', dimColor: true }, '↑/↓ select · enter switch · esc cancel'));
  return e(Box, { flexDirection: 'column', marginTop: 1, borderStyle: 'round', paddingX: 1 }, rows);
}

function PickerRow({ label, focused, checked, multi }) {
  const box = multi ? (checked ? '[x] ' : '[ ] ') : '';
  return e(Text, { color: focused ? 'cyan' : undefined }, `${focused ? '❯ ' : '  '}${box}${label}`);
}

// ── Markdown rendering (token tree from markdown.js → Ink elements) ──

function Markdown({ text }) {
  const blocks = parseMarkdown(text);
  return e(
    Box,
    { flexDirection: 'column' },
    blocks.map((b, i) => e(MdBlock, { key: `m${i}`, block: b })),
  );
}

/** Inline spans → keyed nested <Text> segments. */
function spanEls(spans) {
  return spans.map((s, i) =>
    e(
      Text,
      { key: `s${i}`, bold: s.bold || undefined, italic: s.italic || undefined, color: s.code ? 'cyan' : undefined },
      s.text,
    ),
  );
}

function MdBlock({ block }) {
  switch (block.type) {
    case 'heading':
      return e(Text, { bold: true, color: block.level <= 2 ? 'magenta' : undefined }, spanEls(block.spans));
    case 'bullet':
      return e(Text, null, [`${' '.repeat(block.indent)}• `, ...spanEls(block.spans)]);
    case 'ordered':
      return e(Text, null, [`${' '.repeat(block.indent)}${block.marker}. `, ...spanEls(block.spans)]);
    case 'code':
      return e(
        Box,
        { flexDirection: 'column', paddingLeft: 2 },
        block.lines.map((ln, i) => e(Text, { key: `c${i}`, color: 'green' }, ln.length ? ln : ' ')),
      );
    case 'rule':
      return e(Text, { dimColor: true }, '─'.repeat(40));
    case 'blank':
      return e(Text, null, ' ');
    default:
      return e(Text, null, spanEls(block.spans));
  }
}
