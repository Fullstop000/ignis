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
  setMode,
  toggleSkill,
  toggleMcp,
  listSessions,
  resumeSession,
  copy,
  parseSlash,
  expandPastes,
  pickSingle,
  pickMulti,
  answered,
  answerCancelled,
  toolArgsSummary,
  toolOutputPreview,
} from './protocol.js';
import { parseMarkdown, parseInline } from './markdown.js';

const e = React.createElement;

// Permission modes for the /afk picker (mode strings match the Rust enum).
const AFK_MODES = [
  { mode: 'off', label: 'Off — approve each tool' },
  { mode: 'hands_free', label: 'Hands-free — auto-approve, stay interactive' },
  { mode: 'fully_unattended', label: 'AFK — fully unattended' },
];

export default function App({ engine }) {
  const { exit } = useApp();
  const [state, setState] = useState(initialState());
  // Composer text + caret in one object so every edit is an atomic functional
  // update (no stale-closure splits under fast input).
  const [comp, setComp] = useState({ text: '', cursor: 0 });
  const [history, setHistory] = useState([]);
  const [histIdx, setHistIdx] = useState(-1); // -1 = editing live (not recalling)
  const [localPicker, setLocalPicker] = useState(null); // null | 'model' | 'afk' | 'skills' | 'mcp' | 'sessions'
  const [pastes, setPastes] = useState([]); // multi-line paste contents, shown as [paste #N] chips
  const [reasoningExpanded, setReasoningExpanded] = useState(false); // Ctrl+O expands ✻ Thinking blocks

  useEffect(() => {
    engine.onFrame((frame) => setState((s) => reduceOutbound(s, frame)));
    engine.onClose(() => exit());
  }, [engine, exit]);

  const req = state.request;
  const clearRequest = () => setState((s) => ({ ...s, request: null }));
  const resetComposer = () => {
    setComp({ text: '', cursor: 0 });
    setHistIdx(-1);
    setPastes([]);
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
        setLocalPicker('model');
        return true;
      case 'afk':
        setLocalPicker('afk');
        return true;
      case 'skills':
        setLocalPicker('skills');
        return true;
      case 'mcp':
        setLocalPicker('mcp');
        return true;
      case 'sessions':
      case 'resume':
        // Ask the engine for the project's sessions; the picker renders them
        // once the 'sessions' frame lands (engine owns the listing).
        engine.send(listSessions());
        setLocalPicker('sessions');
        return true;
      case 'copy': {
        // The frontend holds the transcript: extract the last assistant reply
        // and hand the text to the engine to copy (it reuses its platform
        // clipboard helper). Optimistic local notice; the engine warns on fail.
        const last = [...state.blocks].reverse().find((b) => b.kind === 'assistant' && b.text.trim());
        const note = last ? 'Copied to clipboard.' : 'Nothing to copy.';
        if (last) engine.send(copy(last.text));
        setState((s) => ({ ...s, blocks: [...s.blocks, { kind: 'notice', text: note }] }));
        return true;
      }
      default:
        return false; // /compact + unknown → submit (engine / LLM handles)
    }
  };

  useInput((ch, key) => {
    // While a picker is open it owns all keys (PickerFlow / ChoicePicker each
    // have their own useInput).
    if (req || localPicker) return;

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
        engine.send(inject(expandPastes(comp.text, pastes)));
        resetComposer();
      }
      return;
    }
    // Ctrl+O expands/collapses ✻ Thinking (reasoning) blocks, like ratatui.
    if (key.ctrl && ch === 'o') {
      setReasoningExpanded((x) => !x);
      return;
    }
    if (key.return) {
      const raw = comp.text;
      if (!raw.trim()) return;
      // Slash dispatch: a few commands are handled locally; everything else
      // (plain prompts, /compact, unknown slashes) is submitted to the engine.
      const slash = parseSlash(raw);
      if (slash && handleSlash(slash)) {
        resetComposer();
        return;
      }
      engine.send(submit(expandPastes(raw, pastes)));
      setHistory((h) => [...h, raw]);
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
    // Ctrl+J inserts a newline (Ink delivers it as a lone '\n' with key.return
    // false — Enter is '\r'). Handled before the paste branch so a single
    // newline isn't mistaken for a multi-line paste.
    if (ch === '\n') {
      setComp((c) => ({ text: c.text.slice(0, c.cursor) + '\n' + c.text.slice(c.cursor), cursor: c.cursor + 1 }));
      return;
    }
    if (ch && !key.ctrl && !key.meta) {
      // A multi-CHARACTER chunk containing newlines is a paste — collapse it to
      // a `[paste #N · M lines]` chip; expanded at send.
      if (ch.length > 1 && ch.includes('\n')) {
        const idx = pastes.length + 1;
        const chip = `[paste #${idx} · ${ch.split('\n').length} lines]`;
        setPastes((ps) => [...ps, ch]);
        setComp((c) => ({ text: c.text.slice(0, c.cursor) + chip + c.text.slice(c.cursor), cursor: c.cursor + chip.length }));
        return;
      }
      setComp((c) => ({ text: c.text.slice(0, c.cursor) + ch + c.text.slice(c.cursor), cursor: c.cursor + ch.length }));
    }
  });

  const children = [];
  if (state.blocks.length === 0 && state.stream == null && !req) {
    children.push(e(Welcome, { key: 'welcome' }));
  }
  state.blocks.forEach((b, i) => children.push(e(Block, { key: `b${i}`, block: b, expanded: reasoningExpanded })));
  if (state.stream != null) {
    // The in-flight stream renders as live reasoning (rolling ✻ Thinking) or
    // streaming markdown, depending on what the engine opened.
    children.push(
      state.streamKind === 'reasoning'
        ? e(ReasoningView, { key: 'stream', text: state.stream, done: false, expanded: reasoningExpanded })
        : e(Markdown, { key: 'stream', text: state.stream }),
    );
  }
  if (req) {
    // Key by request id so a fresh request resets the flow's internal state.
    children.push(e(PickerFlow, { key: `picker-${req.id}`, req, engine, onDone: clearRequest }));
  } else if (localPicker === 'model') {
    children.push(
      e(ChoicePicker, {
        key: 'model-picker',
        title: 'Switch model',
        items: state.models,
        labelOf: (m) => `${m.provider}/${m.model}`,
        isCurrent: (m) => m.provider === state.provider && m.model === state.model,
        onPick: (m) => {
          engine.send(setModel(m.provider, m.model));
          setLocalPicker(null);
        },
        onCancel: () => setLocalPicker(null),
      }),
    );
  } else if (localPicker === 'afk') {
    children.push(
      e(ChoicePicker, {
        key: 'afk-picker',
        title: 'Permission mode',
        items: AFK_MODES,
        labelOf: (m) => m.label,
        isCurrent: (m) => m.mode === state.mode,
        onPick: (m) => {
          engine.send(setMode(m.mode));
          setLocalPicker(null);
        },
        onCancel: () => setLocalPicker(null),
      }),
    );
  } else if (localPicker === 'skills') {
    children.push(
      e(TogglePicker, {
        key: 'skills-picker',
        title: 'Skills (space to toggle)',
        items: state.skills,
        onToggle: (name) => engine.send(toggleSkill(name)),
        onClose: () => setLocalPicker(null),
      }),
    );
  } else if (localPicker === 'mcp') {
    children.push(
      e(TogglePicker, {
        key: 'mcp-picker',
        title: 'MCP servers (space to toggle)',
        items: state.mcp,
        onToggle: (name) => engine.send(toggleMcp(name)),
        onClose: () => setLocalPicker(null),
      }),
    );
  } else if (localPicker === 'sessions') {
    children.push(
      e(SessionPicker, {
        key: 'sessions-picker',
        items: state.sessions,
        onPick: (s) => {
          engine.send(resumeSession(s.id));
          setLocalPicker(null);
        },
        onCancel: () => setLocalPicker(null),
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

function Block({ block, expanded }) {
  switch (block.kind) {
    case 'user':
      return e(Text, { color: 'magenta' }, `▌ ${block.text}`);
    case 'assistant':
      return e(Markdown, { text: block.text });
    case 'reasoning':
      return e(ReasoningView, { text: block.text, done: true, expanded });
    case 'tool':
      return e(ToolBlock, { block });
    case 'inject':
      return e(Text, { color: 'cyan' }, `↳ ${block.text}`);
    default:
      return e(Text, { dimColor: true }, block.text);
  }
}

// Chain-of-thought block (✻ Thinking). While streaming, a rolling tail of the
// last few lines; once done, collapsed to a lead line + "(+N lines · ctrl+o)"
// breadcrumb unless expanded (Ctrl+O), mirroring the ratatui reasoning block.
const REASONING_PREVIEW = 3;
function ReasoningView({ text, done, expanded }) {
  const lines = String(text == null ? '' : text).split('\n');
  const header = e(Text, { key: 'h', color: 'magenta' }, done ? '✻ Thinking' : '✻ Thinking…');
  let body;
  if (!done) {
    // Live: rolling window of the last few lines.
    body = lines.slice(-REASONING_PREVIEW).map((ln, i) => e(Text, { key: `t${i}`, dimColor: true }, `  ${ln || ' '}`));
  } else if (expanded || lines.length <= REASONING_PREVIEW + 1) {
    body = lines.map((ln, i) => e(Text, { key: `f${i}`, dimColor: true }, `  ${ln || ' '}`));
  } else {
    body = [
      e(Text, { key: 'lead', dimColor: true }, `  ${lines[0]}`),
      e(Text, { key: 'more', dimColor: true }, `  … (+${lines.length - 1} lines · ctrl+o to expand)`),
    ];
  }
  return e(Box, { flexDirection: 'column' }, [header, ...body]);
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
  // Permission-mode badge (HANDS-FREE peach / AFK red), like the ratatui footer.
  const badge =
    state.mode === 'hands_free'
      ? e(Text, { color: 'yellow' }, ' HANDS-FREE ')
      : state.mode === 'fully_unattended'
        ? e(Text, { color: 'red' }, ' AFK ')
        : null;
  if (!segs.length && !badge) return null;
  return e(Box, { marginTop: 1 }, e(Text, { dimColor: true }, segs.join('  ·  ')), badge ? e(Text, null, '  ') : null, badge);
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
  // Multi-line aware: ❯ on the first line, 2-space indent on continuations
  // (Ctrl+J inserts newlines), with a faux block caret on the caret's line —
  // Ink hides the real cursor inline. The box grows with the line count.
  const prefix = status === 'idle' ? '› ' : '… ';
  const head = text.slice(0, cursor);
  const caretLine = head.split('\n').length - 1;
  const caretCol = cursor - (head.lastIndexOf('\n') + 1);
  const lines = text.split('\n');
  const rows = lines.map((ln, li) => {
    const lead = e(Text, { key: 'lead', color: 'gray' }, li === 0 ? prefix : '  ');
    if (li !== caretLine) {
      return e(Box, { key: `l${li}` }, lead, e(Text, { key: 't' }, ln));
    }
    const b = ln.slice(0, caretCol);
    const at = ln.slice(caretCol, caretCol + 1) || ' ';
    const a = ln.slice(caretCol + 1);
    return e(Box, { key: `l${li}` }, lead, e(Text, { key: 'b' }, b), e(Text, { key: 'at', inverse: true }, at), e(Text, { key: 'a' }, a));
  });
  return e(Box, { flexDirection: 'column', marginTop: 1 }, rows);
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
  // Preview pane for the focused option (code/ASCII), if it carries one.
  const focusedPreview = opts[cursor]?.preview;
  if (focusedPreview) {
    rows.push(
      e(
        Box,
        { key: 'preview', flexDirection: 'column', marginTop: 1 },
        String(focusedPreview)
          .split('\n')
          .map((ln, i) => e(Text, { key: `pv${i}`, color: 'cyan' }, ln.length ? ln : ' ')),
      ),
    );
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

// Generic single-select local picker (used by /model, /afk). The cursor
// preselects the current item via `isCurrent`.
function ChoicePicker({ title, items, labelOf, isCurrent, onPick, onCancel }) {
  const [cursor, setCursor] = useState(Math.max(0, items.findIndex((it) => isCurrent && isCurrent(it))));

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
      setCursor((c) => Math.min(Math.max(items.length - 1, 0), c + 1));
      return;
    }
    if (key.return && items[cursor]) onPick(items[cursor]);
  });

  const rows = [e(Text, { key: 'q', bold: true }, title)];
  if (!items.length) rows.push(e(Text, { key: 'empty', dimColor: true }, 'Nothing to choose.'));
  items.forEach((it, i) =>
    rows.push(e(PickerRow, { key: `i${i}`, label: labelOf(it), focused: i === cursor, checked: false, multi: false })),
  );
  rows.push(e(Text, { key: 'hint', dimColor: true }, '↑/↓ select · enter confirm · esc cancel'));
  return e(Box, { flexDirection: 'column', marginTop: 1, borderStyle: 'round', paddingX: 1 }, rows);
}

// Local multi-toggle picker (used by /skills, /mcp). Each space sends a toggle
// command; the engine re-snapshots and `items` reflects the new enabled state.
function TogglePicker({ title, items, onToggle, onClose }) {
  const [cursor, setCursor] = useState(0);
  useInput((ch, key) => {
    if (key.escape) {
      onClose();
      return;
    }
    if (key.upArrow) {
      setCursor((c) => Math.max(0, c - 1));
      return;
    }
    if (key.downArrow) {
      setCursor((c) => Math.min(Math.max(items.length - 1, 0), c + 1));
      return;
    }
    if (ch === ' ' && items[cursor]) onToggle(items[cursor].name);
  });
  const rows = [e(Text, { key: 'q', bold: true }, title)];
  if (!items.length) rows.push(e(Text, { key: 'empty', dimColor: true }, 'Nothing configured.'));
  items.forEach((it, i) =>
    rows.push(e(PickerRow, { key: `i${i}`, label: it.name, focused: i === cursor, checked: it.enabled, multi: true })),
  );
  rows.push(e(Text, { key: 'hint', dimColor: true }, '↑/↓ move · space toggle · esc close'));
  return e(Box, { flexDirection: 'column', marginTop: 1, borderStyle: 'round', paddingX: 1 }, rows);
}

function PickerRow({ label, focused, checked, multi }) {
  const box = multi ? (checked ? '[x] ' : '[ ] ') : '';
  return e(Text, { color: focused ? 'cyan' : undefined }, `${focused ? '❯ ' : '  '}${box}${label}`);
}

// Local session picker (/sessions, /resume). The list arrives asynchronously
// via the 'sessions' frame, so `items` starts empty and fills in. Two-line
// rows like the ratatui picker: derived title + a dim age·id·count meta line.
function SessionPicker({ items, onPick, onCancel }) {
  const [cursor, setCursor] = useState(0);
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
      setCursor((c) => Math.min(Math.max(items.length - 1, 0), c + 1));
      return;
    }
    if (key.return && items[cursor]) onPick(items[cursor]);
  });

  const rows = [e(Text, { key: 'q', bold: true }, 'Resume a session')];
  if (!items.length) {
    rows.push(e(Text, { key: 'empty', dimColor: true }, 'Loading sessions…'));
  } else {
    // Window the (two-line) rows so a long list never overflows the terminal —
    // keep the cursor centered, clamped to the ends.
    const VISIBLE = 8;
    const start =
      items.length <= VISIBLE
        ? 0
        : Math.max(0, Math.min(cursor - Math.floor(VISIBLE / 2), items.length - VISIBLE));
    if (start > 0) rows.push(e(Text, { key: 'above', dimColor: true }, `  ↑ ${start} more`));
    items.slice(start, start + VISIBLE).forEach((s, j) => {
      const i = start + j;
      const focused = i === cursor;
      const title = (s.preview && s.preview.trim()) || '(no preview)';
      const meta = `${ageStr(s.last_modified)} · ${s.id} · ${s.message_count} msg${s.message_count === 1 ? '' : 's'}`;
      rows.push(
        e(
          Box,
          { key: `s${i}`, flexDirection: 'column' },
          e(Text, { color: focused ? 'cyan' : undefined }, `${focused ? '❯ ' : '  '}${title}`),
          e(Text, { dimColor: true }, `    ${meta}`),
        ),
      );
    });
    const below = items.length - (start + VISIBLE);
    if (below > 0) rows.push(e(Text, { key: 'below', dimColor: true }, `  ↓ ${below} more`));
  }
  rows.push(e(Text, { key: 'hint', dimColor: true }, '↑/↓ select · enter resume · esc cancel'));
  return e(Box, { flexDirection: 'column', marginTop: 1, borderStyle: 'round', paddingX: 1 }, rows);
}

// Relative age from a Unix-seconds timestamp, mirroring SessionMeta::age_str.
function ageStr(secs) {
  const delta = Math.max(0, Math.floor(Date.now() / 1000) - Number(secs || 0));
  if (delta < 60) return 'just now';
  if (delta < 3600) return `${Math.floor(delta / 60)} min ago`;
  if (delta < 86400) {
    const h = Math.floor(delta / 3600);
    return h === 1 ? '1 hour ago' : `${h} hours ago`;
  }
  const d = Math.floor(delta / 86400);
  return d === 1 ? '1 day ago' : `${d} days ago`;
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

// Markdown table: per-cell inline markdown (bold/italic/code), columns padded
// to the VISIBLE (markdown-stripped) content width (capped), header bold, a dim
// rule under it. (No CJK width-fit yet.)
function MdTable({ header, rows }) {
  const cols = header.length;
  const cellSpans = (cell) => parseInline(cell ?? '');
  const visLen = (cell) => cellSpans(cell).reduce((n, s) => n + s.text.length, 0);
  const widths = [];
  for (let c = 0; c < cols; c++) {
    let w = visLen(header[c]);
    for (const r of rows) w = Math.max(w, visLen(r[c]));
    widths[c] = Math.min(w, 30);
  }
  // One row → flat list of <Text> segments: ` │ ` separators, each cell's
  // styled spans, then padding to the column width (computed from visible len).
  const rowEls = (cells, forceBold) => {
    const segs = [];
    widths.forEach((w, c) => {
      if (c > 0) segs.push(e(Text, { key: `sep${c}` }, ' │ '));
      const spans = truncateSpans(cellSpans(cells[c] ?? ''), w);
      const used = spans.reduce((n, s) => n + s.text.length, 0);
      spans.forEach((s, j) =>
        segs.push(
          e(
            Text,
            { key: `c${c}s${j}`, bold: forceBold || s.bold || undefined, italic: s.italic || undefined, color: s.code ? 'cyan' : undefined },
            s.text,
          ),
        ),
      );
      if (used < w) segs.push(e(Text, { key: `pad${c}` }, ' '.repeat(w - used)));
    });
    return segs;
  };
  const sep = widths.map((w) => '─'.repeat(w)).join('─┼─');
  const lines = [
    e(Text, { key: 'h' }, rowEls(header, true)),
    e(Text, { key: 's', dimColor: true }, sep),
    ...rows.map((r, i) => e(Text, { key: `r${i}` }, rowEls(r, false))),
  ];
  return e(Box, { flexDirection: 'column' }, lines);
}

/** Trim styled spans to at most `max` visible chars (slicing the last span). */
function truncateSpans(spans, max) {
  const out = [];
  let used = 0;
  for (const s of spans) {
    if (used >= max) break;
    const room = max - used;
    if (s.text.length <= room) {
      out.push(s);
      used += s.text.length;
    } else {
      out.push({ ...s, text: s.text.slice(0, room) });
      used = max;
    }
  }
  return out;
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
    case 'table':
      return e(MdTable, { header: block.header, rows: block.rows });
    case 'rule':
      return e(Text, { dimColor: true }, '─'.repeat(40));
    case 'blank':
      return e(Text, null, ' ');
    default:
      return e(Text, null, spanEls(block.spans));
  }
}
