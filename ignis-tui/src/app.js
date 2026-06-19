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
import React, { useState, useEffect, useRef } from 'react';
import { Box, Text, Static, useInput, useApp, useStdout } from 'ink';
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
  slashSuggestions,
  quoteSessionId,
  expandPastes,
  pickSingle,
  pickMulti,
  answered,
  answerCancelled,
  toolArgsSummary,
  toolOutputPreview,
} from './protocol.js';
import DiffView from './diff-view.js';
import { parseMarkdown, parseInline } from './markdown.js';

const e = React.createElement;

// Permission modes for the /afk picker (mode strings match the Rust enum).
const AFK_MODES = [
  { mode: 'off', label: 'Off — approve each tool' },
  { mode: 'hands_free', label: 'Hands-free — auto-approve, stay interactive' },
  { mode: 'fully_unattended', label: 'AFK — fully unattended' },
];

export default function App({ engine, onExit }) {
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
  const [slashSel, setSlashSel] = useState(0); // selected row in the slash-command suggestions
  const [spin, setSpin] = useState(0); // running-spinner frame
  const turnStart = useRef(0); // ms timestamp the current turn began (0 = idle)
  const [queue, setQueue] = useState([]); // messages typed while busy, drained 1/turn-end

  useEffect(() => {
    engine.onFrame((frame) => setState((s) => reduceOutbound(s, frame)));
    engine.onClose(() => exit());
  }, [engine, exit]);

  // Drive the running status bar while a turn is in flight: stamp the start and
  // tick the spinner (which also re-renders the elapsed clock). No timer when idle.
  useEffect(() => {
    if (state.status !== 'busy') {
      turnStart.current = 0;
      return;
    }
    turnStart.current = Date.now();
    const id = setInterval(() => setSpin((s) => s + 1), 90);
    // Don't let the spinner timer hold the event loop open (real app stays
    // alive via the engine pipe; tests/`node --test` would otherwise hang).
    if (typeof id.unref === 'function') id.unref();
    return () => clearInterval(id);
  }, [state.status]);

  // /clear, resume and Ctrl+O replace or repaint the transcript wholesale and
  // bump `generation`. The committed blocks live in <Static> (already flushed to
  // scrollback), so a remount alone would leave the stale rows on screen — wipe
  // screen+scrollback so the fresh transcript starts clean. This runs in the
  // render body (ref-guarded, once per generation) so the wipe precedes Ink's
  // frame write; doing it in an effect would clear the screen *after* the new
  // frame was painted, blanking it until the next render.
  const { stdout } = useStdout();
  const prevGen = useRef(0);
  if (state.generation !== prevGen.current) {
    prevGen.current = state.generation;
    stdout?.write?.('\x1b[2J\x1b[3J\x1b[H');
  }

  const req = state.request;
  const clearRequest = () => setState((s) => ({ ...s, request: null }));
  const resetComposer = () => {
    setComp({ text: '', cursor: 0 });
    setHistIdx(-1);
    setPastes([]);
    setSlashSel(0);
  };

  // Clean exit: hand the `ignis --resume <id>` hint to the launcher (printed
  // after Ink tears down), but only once there's a session worth resuming.
  const cleanExit = () => {
    const turns = state.blocks.filter((b) => b.kind === 'user').length;
    if (onExit && turns > 0 && state.sessionId) onExit(resumeHint(state.sessionId));
    engine.close();
    exit();
  };

  // Locally-handled slash commands. Returns true if handled here (so the line
  // is NOT submitted to the engine); false falls through to a normal submit.
  const handleSlash = (slash) => {
    switch (slash.name) {
      case 'clear':
        engine.send(newSession());
        // Clear the local transcript; the engine re-snapshots with the new id.
        // Bump `generation` so the committed <Static> region remounts (and the
        // screen is wiped) instead of leaving the old transcript flushed.
        setState((s) => ({ ...s, blocks: [], stream: null, turns: 0, usage: null, generation: s.generation + 1 }));
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

  // Dispatch one fully-resolved (paste-expanded) line as if Enter were pressed
  // while idle: run locally-handled slash commands, else submit to the engine
  // with an optimistic user block. Shared by the idle Enter path and the
  // queue drain.
  const dispatchResolved = (text) => {
    const slash = parseSlash(text);
    if (slash && handleSlash(slash)) return;
    engine.send(submit(text));
    if (!slash) {
      setState((s) => ({ ...s, blocks: [...s.blocks, { kind: 'user', text, pending: true }] }));
    }
  };

  // Drain exactly ONE queued message on each turn-end, mirroring the native
  // runner's edge-triggered `pump_queued`. Keyed on the turn-end EVENT
  // (`state.turnEnds`), NOT a busy→idle status change: several engine paths
  // (provider/session errors) emit a lone turn_end that leaves status `idle`, so
  // a status-keyed effect would strand the next queued message. Submitting flips
  // the engine back to busy; the next item waits for the following turn-end.
  useEffect(() => {
    if (state.turnEnds === 0 || queue.length === 0) return;
    const [next, ...rest] = queue;
    setQueue(rest);
    dispatchResolved(next);
  }, [state.turnEnds]); // eslint-disable-line react-hooks/exhaustive-deps

  useInput((ch, key) => {
    // While a picker is open it owns all keys (PickerFlow / ChoicePicker each
    // have their own useInput).
    if (req || localPicker) return;

    if (key.ctrl && ch === 'c') {
      if (state.status !== 'idle') engine.send(cancel());
      else cleanExit();
      return;
    }
    // Ctrl+D exits cleanly when idle (prints the resume hint), like the native TUI.
    if (key.ctrl && ch === 'd') {
      if (state.status === 'idle' && !comp.text) cleanExit();
      return;
    }
    if (key.ctrl && ch === 's') {
      // Inject only steers an IN-FLIGHT turn; when idle there's no inject sink
      // and the engine would silently drop the text. Gate on busy (matching the
      // native TUI's mode != Idle) so an idle Ctrl+S is a no-op that keeps the
      // composer intact.
      if (state.status === 'busy' && comp.text.trim()) {
        engine.send(inject(expandPastes(comp.text, pastes)));
        resetComposer();
      }
      return;
    }
    // Ctrl+O expands/collapses ✻ Thinking (reasoning) blocks, like ratatui.
    // Committed reasoning lives in <Static> (frozen once flushed), so bump
    // `generation` to remount + repaint with the new expand state — the same
    // full-repaint the native TUI does via its re-anchor.
    if (key.ctrl && ch === 'o') {
      setReasoningExpanded((x) => !x);
      setState((s) => ({ ...s, generation: s.generation + 1 }));
      return;
    }
    if (key.return) {
      const raw = comp.text;
      if (!raw.trim()) return;
      // If the slash-suggestion list is open, Enter runs the selected command.
      const sugg = slashSuggestions(raw);
      const line = sugg.length ? sugg[Math.min(slashSel, sugg.length - 1)].name : raw;
      // Resolve pastes now: the composer clears on Enter, so a queued line must
      // already carry its expanded content.
      const sent = expandPastes(line, pastes);
      if (state.status === 'busy') {
        // Busy: hold it in the waiting queue (no submit, no transcript block).
        // It drains one-per-turn at turn-end. Ctrl+S sends now, ↑ edits last.
        setQueue((q) => [...q, sent]);
      } else {
        // Idle: dispatch immediately (local slash command, or submit + block).
        dispatchResolved(sent);
      }
      setHistory((h) => [...h, raw]);
      resetComposer();
      return;
    }
    // ↑/↓ navigate the slash suggestions when open, else recall input history.
    if (key.upArrow) {
      if (slashSuggestions(comp.text).length) {
        setSlashSel((s) => Math.max(0, s - 1));
        return;
      }
      // While busy with a queue, ↑ pulls the most recent queued message back
      // into the composer to edit (matches the native TUI's "edit last").
      if (state.status === 'busy' && queue.length > 0 && !comp.text) {
        const last = queue[queue.length - 1];
        setQueue((q) => q.slice(0, -1));
        setComp({ text: last, cursor: last.length });
        return;
      }
      if (!history.length) return;
      const ni = histIdx < 0 ? history.length - 1 : Math.max(0, histIdx - 1);
      setHistIdx(ni);
      setComp({ text: history[ni], cursor: history[ni].length });
      return;
    }
    if (key.downArrow) {
      const sugg = slashSuggestions(comp.text);
      if (sugg.length) {
        setSlashSel((s) => Math.min(sugg.length - 1, s + 1));
        return;
      }
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
      setSlashSel(0); // a fresh keystroke re-tops the slash-suggestion selection
    }
  });

  const children = [];
  // Commit the *settled* prefix of the transcript to <Static>: Ink prints each
  // Static item to the terminal exactly once (real scrollback) and never
  // re-renders it, so the live region below stays short and Ink never falls back
  // to its full-screen-clear path (the flicker) once the transcript outgrows the
  // window. A still-mutating tail — a pending tool, or the optimistic user block
  // before `user_prompt_committed` — must stay dynamic until it reaches final
  // form, so the boundary is the first such block. Committed reasoning is frozen
  // at its current expand state; live/tail reasoning still honours Ctrl+O.
  let firstLive = state.blocks.length;
  for (let i = 0; i < state.blocks.length; i++) {
    const b = state.blocks[i];
    if ((b.kind === 'tool' && !b.done) || (b.kind === 'user' && b.pending)) {
      firstLive = i;
      break;
    }
  }
  const settled = state.blocks.slice(0, firstLive);
  const tail = state.blocks.slice(firstLive);
  children.push(
    e(Static, { key: `tx-${state.generation}`, items: settled }, (b, i) =>
      e(Block, { key: i, block: b, expanded: reasoningExpanded }),
    ),
  );
  if (state.blocks.length === 0 && state.stream == null && !req) {
    children.push(e(Welcome, { key: 'welcome', version: state.version, cwd: state.cwd }));
  }
  tail.forEach((b, i) => children.push(e(Block, { key: `t${i}`, block: b, expanded: reasoningExpanded })));
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
      e(ModelPicker, {
        key: 'model-picker',
        models: state.models,
        provider: state.provider,
        model: state.model,
        effort: state.effort,
        onPick: (m, effort) => {
          engine.send(setModel(m.provider, m.model, effort));
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
    // Task list (todo_write) above everything — persists across the turn,
    // unlike the idle-only / busy-only strips below.
    if (state.todos && state.todos.length) {
      children.push(e(TodosStrip, { key: 'todos', todos: state.todos }));
    }
    // Running status bar above the composer while a turn is in flight.
    if (state.status === 'busy') {
      children.push(e(RunningBar, { key: 'running', state, spin, startedAt: turnStart.current }));
    }
    // Waiting queue: messages typed while busy, drained one per turn-end.
    if (queue.length > 0) {
      children.push(e(QueuedStrip, { key: 'queued', queue }));
    }
    // Slash-command suggestions sit *under* the composer while typing a
    // `/command` (Claude-Code / shell-completion style) so the list grows
    // downward as you type and never pushes the cursor.
    const sugg = comp.text ? slashSuggestions(comp.text) : [];
    children.push(e(Composer, { key: 'composer', text: comp.text, cursor: comp.cursor, status: state.status }));
    if (sugg.length) {
      children.push(e(SlashSuggestions, { key: 'slash', items: sugg, selected: Math.min(slashSel, sugg.length - 1) }));
    }
  }
  children.push(e(Footer, { key: 'footer', state }));
  return e(Box, { flexDirection: 'column' }, children);
}

// `ignis --resume <id>` hint printed after a clean exit (Ctrl+C/Ctrl+D when
// idle). ANSI: "Resume…" dim grey, the command mauve — matching the native TUI.
function resumeHint(sessionId) {
  const grey = (s) => `\x1b[90m${s}\x1b[0m`;
  const mauve = (s) => `\x1b[38;2;203;166;247m${s}\x1b[0m`;
  return `\n${grey('Resume this session with:')}\n${mauve(`  ignis --resume ${quoteSessionId(sessionId)}`)}\n`;
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
//
// The `edit_file` result gets a dedicated treatment: its header reads
// `◆ Edited <path> (+adds -dels)` and the body is a line-numbered unified-diff
// view (gutter shows real source line numbers, `⋮` separates non-contiguous
// hunks, foreground-only red/green for `-`/`+` rows — no background bars).
function ToolBlock({ block }) {
  const isError = block.result?.is_error;
  if (!isError && block.name === 'edit_file') {
    return e(EditFileBlock, { block });
  }
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

/**
 * Render an `edit_file` tool call as a Claude-Code / Codex-style diff view.
 * The heavy lifting lives in `<DiffView>`; this wrapper just handles the
 * in-flight spinner header and the path extraction.
 */
function EditFileBlock({ block }) {
  const path = parseEditPath(block.args);
  const inFlight = !block.done || !block.result;
  if (inFlight) {
    return e(
      Text,
      { color: 'yellow' },
      `◆ Editing ${path || block.name} …`,
    );
  }
  return e(DiffView, { content: block.result.content, path });
}

/** Pull `path` (or `file_path`) out of a tool-args JSON blob. */
function parseEditPath(argsJson) {
  if (!argsJson) return '';
  try {
    const obj = JSON.parse(argsJson);
    if (obj && typeof obj === 'object') {
      return obj.path || obj.file_path || '';
    }
  } catch {
    // Malformed JSON — fall through to the empty path; the header still
    // reads `◆ Edited  (+a -d)` which is ugly but never breaks the layout.
  }
  return '';
}

// Statusline footer: provider/model · cwd · turns · context tokens — fed by the
// startup snapshot and the `usage`/`turn_start` events (engine-owned data).
function Footer({ state }) {
  const segs = [];
  if (state.provider || state.model) {
    const effort = state.effort ? ` (${state.effort})` : '';
    segs.push(`${state.provider || '?'}/${state.model || '?'}${effort}`);
  }
  if (state.cwd) segs.push(baseName(state.cwd));
  segs.push(`${state.turns} turn${state.turns === 1 ? '' : 's'}`);
  // Context-fill gauge: tokens used vs the active model's window (its `context`,
  // carried in the models list), as `N tok (X%)` — mirrors the native footer.
  const active = state.models.find((m) => m.provider === state.provider && m.model === state.model);
  const window = active?.context || 120000; // last-resort fallback, like the native TUI
  const ctxTokens = state.usage?.input_tokens > 0 ? state.usage.input_tokens : estimateContextTokens(state.blocks);
  const pct = window > 0 ? Math.min(100, Math.floor((ctxTokens * 100) / window)) : 0;
  segs.push(`${ctxTokens} tok (${pct}%)`);
  // Background-shell indicator: `⚙ N bg` while any bash(run_in_background) shells
  // are live (hidden at 0).
  if (state.bgShells > 0) segs.push(`⚙ ${state.bgShells} bg`);
  // Permission-mode badge (HANDS-FREE peach / AFK red), like the ratatui footer.
  const badge =
    state.mode === 'hands_free'
      ? e(Text, { color: 'yellow' }, ' HANDS-FREE ')
      : state.mode === 'fully_unattended'
        ? e(Text, { color: 'red' }, ' AFK ')
        : null;
  if (!segs.length && !badge) return null;
  return e(Box, null, e(Text, { dimColor: true }, segs.join('  ·  ')), badge ? e(Text, null, '  ') : null, badge);
}

function baseName(p) {
  const parts = String(p).replace(/[/\\]+$/, '').split(/[/\\]/);
  return parts[parts.length - 1] || p;
}

// Estimated context tokens from the transcript (chars/4), the fallback when the
// engine hasn't reported real usage yet — mirrors the native App::context_tokens.
function estimateContextTokens(blocks) {
  let chars = 0;
  for (const b of blocks) {
    if (b.kind === 'tool') chars += (b.args?.length || 0) + (b.result?.content?.length || 0);
    else chars += b.text?.length || 0;
  }
  return Math.floor(chars / 4);
}

// ASCII-art IGNIS banner (ANSI-Shadow figlet) over the engine version + cwd,
// shown once on the empty startup screen. `version`/`cwd` arrive with the
// startup snapshot, so they fill in a frame after mount.
const IGNIS_ART = [
  '██╗ ██████╗ ███╗   ██╗██╗███████╗',
  '██║██╔════╝ ████╗  ██║██║██╔════╝',
  '██║██║  ███╗██╔██╗ ██║██║███████╗',
  '██║██║   ██║██║╚██╗██║██║╚════██║',
  '██║╚██████╔╝██║ ╚████║██║███████║',
  '╚═╝ ╚═════╝ ╚═╝  ╚═══╝╚═╝╚══════╝',
];

function Welcome({ version, cwd }) {
  const meta = [version ? `v${version}` : null, cwd].filter(Boolean).join('  ·  ');
  return e(
    Box,
    {
      flexDirection: 'column',
      alignSelf: 'flex-start', // hug the art width, don't stretch to the terminal
      borderStyle: 'round',
      borderColor: 'magenta',
      paddingX: 2,
      paddingY: 1,
      marginY: 1,
    },
    ...IGNIS_ART.map((row, i) => e(Text, { key: `art${i}`, color: 'magenta' }, row)),
    meta ? e(Box, { key: 'meta', marginTop: 1 }, e(Text, { dimColor: true }, meta)) : null,
  );
}

// Braille spinner frames for the running status bar (matches the native TUI).
const SPINNER = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/** Compact token count: 1234 → "1.2k", 12345 → "12k". */
function fmtTokens(n) {
  if (n >= 10000) return `${Math.round(n / 1000)}k`;
  if (n >= 1000) return `${(n / 1000).toFixed(1)}k`;
  return String(n);
}

// Running status bar shown while a turn is in flight: animated spinner, elapsed
// clock, and live ↑ input / ↓ output token counts (output estimated from the
// streamed chars until the engine's usage event lands), + an interrupt hint.
function RunningBar({ state, spin, startedAt }) {
  const frame = SPINNER[spin % SPINNER.length];
  const elapsed = startedAt ? Math.floor((Date.now() - startedAt) / 1000) : 0;
  const inTok = state.usage?.input_tokens || 0;
  const outTok = Math.max(state.usage?.output_tokens || 0, Math.ceil((state.streamChars || 0) / 4));
  const toks = [];
  if (inTok) toks.push(`↑ ${fmtTokens(inTok)}`);
  if (outTok) toks.push(`↓ ${fmtTokens(outTok)}`);
  const tail = `${toks.length ? `  ·  ${toks.join(' ')} tok` : ''}  ·  ctrl+c to interrupt`;
  return e(
    Box,
    { marginTop: 1 },
    e(Text, { color: 'cyan' }, `${frame} `),
    e(Text, { color: 'gray' }, `Working… ${elapsed}s`),
    e(Text, { dimColor: true }, tail),
  );
}

// Task-list panel (todo_write): a checklist the agent maintains for multi-step
// work. Shown above the composer whenever the list is non-empty, in every state
// (persists across the turn). ✓ completed (dim), ◐ in_progress (highlighted),
// ◻ pending (dim). In-progress rows prefer the present-continuous `activeForm`.
const TODO_GLYPH = { completed: '✓', in_progress: '◐', pending: '◻' };
function TodosStrip({ todos }) {
  const done = todos.filter((t) => t.status === 'completed').length;
  const rows = [
    e(Text, { key: 'hdr', dimColor: true }, `  Tasks ${done}/${todos.length}`),
  ];
  todos.forEach((t, i) => {
    const glyph = TODO_GLYPH[t.status] ?? '◻';
    const label = t.status === 'in_progress' && t.activeForm ? t.activeForm : t.content;
    if (t.status === 'in_progress') {
      rows.push(e(Text, { key: `t${i}`, color: 'cyan', bold: true }, `  ${glyph} ${label}`));
    } else {
      const strike = t.status === 'completed';
      rows.push(
        e(Text, { key: `t${i}`, dimColor: true, strikethrough: strike }, `  ${glyph} ${label}`),
      );
    }
  });
  return e(Box, { flexDirection: 'column', marginTop: 1 }, rows);
}

// Waiting-queue strip: messages typed while a turn is in flight, drained one per
// turn-end. Mirrors the native TUI's queued strip (cap + overflow + hint).
const MAX_QUEUE_ROWS = 5;
function QueuedStrip({ queue }) {
  const shown = queue.slice(0, MAX_QUEUE_ROWS);
  const overflow = queue.length - shown.length;
  const rows = shown.map((t, i) =>
    e(Text, { key: `q${i}`, dimColor: true }, `  ⏳ ${t.split('\n')[0]}`));
  if (overflow > 0) {
    rows.push(e(Text, { key: 'of', dimColor: true }, `  +${overflow} more queued`));
  }
  rows.push(e(Text, { key: 'hint', dimColor: true }, '  ↑ edit last · Enter queue · Ctrl+S send now'));
  return e(Box, { flexDirection: 'column', marginTop: 1 }, rows);
}

// Slash-command autocomplete dropdown — sits *under* the composer, mirroring
// the native TUI. Capped at MAX_SLASH_ROWS=8 visible rows and scrolls so the
// selected entry stays in view (relevant once skills + `/skills` push the list
// past 8 entries). ↑/↓ move, Enter runs the highlighted command.
const MAX_SLASH_ROWS = 8;
function SlashSuggestions({ items, selected }) {
  const sel = Math.min(selected, items.length - 1);
  // Window math mirrors ratatui's `slash_window_start`: keep `sel` inside
  // `[start, start+visible)` so a long list scrolls instead of clipping the
  // selection.
  const visible = Math.min(items.length, MAX_SLASH_ROWS);
  const start = sel >= visible ? sel - visible + 1 : 0;
  const end = Math.min(start + visible, items.length);
  return e(
    Box,
    { flexDirection: 'column' },
    items.slice(start, end).map((c, i) => {
      const idx = start + i;
      const isSel = idx === sel;
      return e(
        Text,
        { key: c.name, color: isSel ? 'cyan' : undefined, dimColor: !isSel },
        `${isSel ? '❯ ' : '  '}${c.name.padEnd(11)} ${c.description}`,
      );
    }),
  );
}

function Composer({ text, cursor, status }) {
  // Rounded input box (blue border + ❯ when idle, dim when busy), multi-line
  // aware (❯ on line 0, 2-space-indented continuations from Ctrl+J), with a
  // faux block caret on the caret's line and a placeholder when empty.
  const idle = status === 'idle';
  const accent = idle ? 'cyan' : 'gray';
  let rows;
  if (!text) {
    rows = [
      e(
        Box,
        { key: 'l0' },
        e(Text, { color: accent }, '❯ '),
        e(Text, { inverse: true }, ' '),
        e(Text, { dimColor: true }, idle ? 'Type a message…' : 'Type your next message…'),
      ),
    ];
  } else {
    const head = text.slice(0, cursor);
    const caretLine = head.split('\n').length - 1;
    const caretCol = cursor - (head.lastIndexOf('\n') + 1);
    const lines = text.split('\n');
    rows = lines.map((ln, li) => {
      const lead = e(Text, { key: 'lead', color: li === 0 ? accent : 'gray' }, li === 0 ? '❯ ' : '  ');
      if (li !== caretLine) {
        return e(Box, { key: `l${li}` }, lead, e(Text, { key: 't' }, ln));
      }
      const b = ln.slice(0, caretCol);
      const at = ln.slice(caretCol, caretCol + 1) || ' ';
      const a = ln.slice(caretCol + 1);
      return e(Box, { key: `l${li}` }, lead, e(Text, { key: 'b' }, b), e(Text, { key: 'at', inverse: true }, at), e(Text, { key: 'a' }, a));
    });
  }
  return e(Box, { flexDirection: 'column', borderStyle: 'round', borderColor: accent, paddingX: 1 }, rows);
}

// ── Picker (ask_user): single/multi-select, free-text "Other", multi-question ──

/** Current terminal width in columns; defaults to 80 when stdout is unavailable
 *  (tests) so picker borders still have a stable geometry. Listens for the
 *  terminal `resize` event so picker borders update when the window changes. */
function useTerminalWidth() {
  const { stdout } = useStdout();
  const [cols, setCols] = useState(stdout?.columns || 80);
  useEffect(() => {
    if (!stdout) return;
    const onResize = () => setCols(stdout.columns || 80);
    stdout.on('resize', onResize);
    return () => stdout.off('resize', onResize);
  }, [stdout]);
  return cols;
}

function PickerFlow({ req, engine, onDone }) {
  const cols = useTerminalWidth();
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
    // Text-input question (e.g. /connect's API key): no rows to navigate — the
    // whole keyboard feeds a free-text buffer; Enter submits it as Single.
    if (q.text_input) {
      if (key.return) {
        if (other.trim()) finish(pickSingle(other));
        return;
      }
      if (key.backspace || key.delete) {
        setOther((t) => t.slice(0, -1));
        return;
      }
      if (ch && ch.charCodeAt(0) >= 0x20 && !key.ctrl && !key.meta) setOther((t) => t + ch);
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
  if (q.text_input) {
    // Masked (●) when q.mask so an API key isn't shoulder-surfable; ▏ is the
    // caret. Empty buffer still shows the caret so the field reads as active.
    const shown = q.mask ? '●'.repeat(other.length) : other;
    rows.push(e(Text, { key: 'input', color: 'cyan' }, `❯ ${shown}▏`));
  } else {
    opts.forEach((o, i) => rows.push(e(PickerRow, { key: `o${i}`, label: o.label, focused: i === cursor, checked: selected.includes(i), multi: q.multi_select })));
    if (q.allow_other) {
      const label = other ? `Other: ${other}` : 'Other (type custom)…';
      rows.push(e(PickerRow, { key: 'other', label, focused: cursor === otherIdx, checked: selected.includes(otherIdx), multi: q.multi_select }));
    }
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
  const hint = q.text_input
    ? 'type · enter submit · esc cancel'
    : q.multi_select
      ? '↑/↓ move · space toggle · enter confirm · esc cancel'
      : '↑/↓ select · enter confirm · esc cancel';
  rows.push(e(Text, { key: 'hint', dimColor: true }, hint));
  return e(Box, { flexDirection: 'column', width: cols, marginTop: 1, borderStyle: 'round', paddingX: 1 }, rows);
}

// Generic single-select local picker (used by /model, /afk). The cursor
// preselects the current item via `isCurrent`.
function ChoicePicker({ title, items, labelOf, isCurrent, onPick, onCancel }) {
  const cols = useTerminalWidth();
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
  return e(Box, { flexDirection: 'column', width: cols, marginTop: 1, borderStyle: 'round', paddingX: 1 }, rows);
}

// `/model` picker: ↑/↓ picks the model, ←/→ cycles that model's reasoning-effort
// levels (mirrors the native ratatui picker). Enter applies (model, effort) —
// effort is `null` for a model with no levels. Preselects the active model+effort.
function ModelPicker({ models, provider, model, effort, onPick, onCancel }) {
  const cols = useTerminalWidth();
  const curIdx = Math.max(0, models.findIndex((m) => m.provider === provider && m.model === model));
  const [cursor, setCursor] = useState(curIdx);
  const initLevels = models[curIdx]?.effort_levels || [];
  const [effortIdx, setEffortIdx] = useState(Math.max(0, initLevels.indexOf(effort)));

  const levels = models[cursor]?.effort_levels || [];
  // effortIdx is kept across model moves and clamped to the focused model's
  // levels on use, so cycling never points past a shorter model's list.
  const idx = Math.min(effortIdx, Math.max(0, levels.length - 1));

  useInput((ch, key) => {
    if (key.escape) return onCancel();
    if (key.upArrow) return setCursor((c) => Math.max(0, c - 1));
    if (key.downArrow) return setCursor((c) => Math.min(Math.max(models.length - 1, 0), c + 1));
    if (key.leftArrow) return setEffortIdx(() => Math.max(0, idx - 1));
    if (key.rightArrow) return setEffortIdx(() => Math.min(Math.max(levels.length - 1, 0), idx + 1));
    if (key.return && models[cursor]) onPick(models[cursor], levels.length ? levels[idx] : null);
  });

  const rows = [e(Text, { key: 'q', bold: true }, 'Switch model')];
  if (!models.length) {
    rows.push(e(Text, { key: 'empty', dimColor: true }, 'No models configured.'));
    return e(Box, { flexDirection: 'column', width: cols, marginTop: 1, borderStyle: 'round', paddingX: 1 }, rows);
  }
  if (levels.length) {
    rows.push(
      e(
        Box,
        { key: 'effort' },
        e(Text, { dimColor: true }, 'effort: '),
        ...levels.map((lv, i) =>
          e(Text, { key: `e${i}`, ...(i === idx ? { color: 'black', backgroundColor: 'cyan' } : { dimColor: true }) }, ` ${lv} `),
        ),
        e(Text, { dimColor: true }, '  ←/→'),
      ),
    );
  }
  models.forEach((m, i) => {
    const hasEffort = (m.effort_levels || []).length > 0;
    const ctx = m.context ? `  ${Math.round(m.context / 1000)}K ctx` : '';
    const label = `${m.provider}/${m.model}${hasEffort ? ' ◆' : ''}${ctx}`;
    rows.push(e(PickerRow, { key: `i${i}`, label, focused: i === cursor, checked: false, multi: false }));
  });
  rows.push(e(Text, { key: 'hint', dimColor: true }, '↑/↓ model · ←/→ effort · enter apply · esc cancel'));
  return e(Box, { flexDirection: 'column', width: cols, marginTop: 1, borderStyle: 'round', paddingX: 1 }, rows);
}

// Local multi-toggle picker (used by /skills, /mcp). Each space sends a toggle
// command; the engine re-snapshots and `items` reflects the new enabled state.
function TogglePicker({ title, items, onToggle, onClose }) {
  const cols = useTerminalWidth();
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
  return e(Box, { flexDirection: 'column', width: cols, marginTop: 1, borderStyle: 'round', paddingX: 1 }, rows);
}

function PickerRow({ label, focused, checked, multi }) {
  const box = multi ? (checked ? '[x] ' : '[ ] ') : '';
  return e(Text, { color: focused ? 'cyan' : undefined }, `${focused ? '❯ ' : '  '}${box}${label}`);
}

// Local session picker (/sessions, /resume). The list arrives asynchronously
// via the 'sessions' frame, so `items` starts empty and fills in. Two-line
// rows like the ratatui picker: derived title + a dim age·id·count meta line.
function SessionPicker({ items, onPick, onCancel }) {
  const cols = useTerminalWidth();
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
  return e(Box, { flexDirection: 'column', width: cols, marginTop: 1, borderStyle: 'round', paddingX: 1 }, rows);
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
