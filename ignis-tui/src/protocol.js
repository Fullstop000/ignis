// Pure, dependency-free protocol logic for the ignis Ink frontend.
//
// Mirrors the Rust wire types in ignis/src/console/frontend/protocol.rs:
//   Outbound      (engine→frontend): {kind:"event"|"request"|"snapshot", data}
//   ClientCommand (frontend→engine): {kind:"submit"|"inject"|"cancel"|"reply"|
//                                      "set_session"|"shutdown", data?}
// The AgentEvent inside an `event` frame is adjacently tagged: {type, payload?}
// (unit events like turn_start/turn_end have no payload).
//
// Kept free of `ink`/`react` so it runs under `node --test` with no install.

// ── Wire name constants ─────────────────────────────────────────────────────
// The single source of truth for the protocol's string tags, mirroring the Rust
// enums' `#[serde(rename = …)]` values. Frozen so they read as enums; used
// everywhere below instead of bare string literals.

/** `Outbound` frame kinds (engine → frontend). */
export const FRAME = Object.freeze({
  EVENT: 'event',
  REQUEST: 'request',
  SNAPSHOT: 'snapshot',
  SESSIONS: 'sessions',
  TRANSCRIPT: 'transcript',
});

/** `AgentEvent` types — the `type` inside an `event` frame. */
export const EVENT = Object.freeze({
  TURN_START: 'turn_start',
  TURN_END: 'turn_end',
  MESSAGE_START: 'message_start',
  MESSAGE_UPDATE: 'message_update',
  MESSAGE_END: 'message_end',
  USER_PROMPT_COMMITTED: 'user_prompt_committed',
  USER_INJECTED: 'user_injected',
  TOOL_EXECUTION_START: 'tool_execution_start',
  TOOL_EXECUTION_END: 'tool_execution_end',
  WARNING: 'warning',
  NOTICE: 'notice',
  RECONNECTING: 'reconnecting',
  USAGE: 'usage',
});

/** `ClientCommand` kinds (frontend → engine). */
export const CMD = Object.freeze({
  SUBMIT: 'submit',
  INJECT: 'inject',
  CANCEL: 'cancel',
  SET_SESSION: 'set_session',
  NEW_SESSION: 'new_session',
  SET_MODEL: 'set_model',
  SET_MODE: 'set_mode',
  TOGGLE_SKILL: 'toggle_skill',
  TOGGLE_MCP: 'toggle_mcp',
  LIST_SESSIONS: 'list_sessions',
  RESUME_SESSION: 'resume_session',
  COPY: 'copy',
  REPLY: 'reply',
  SHUTDOWN: 'shutdown',
});

export function initialState() {
  return {
    blocks: [],
    stream: null,
    // What the in-flight `stream` is: 'assistant' reply text or 'reasoning'
    // (chain-of-thought). Both arrive as MessageStart/Update/End; the start
    // event's message shape disambiguates them.
    streamKind: null,
    status: 'idle',
    sessionId: null,
    request: null,
    // Statusline meta (from the startup snapshot) + live counters.
    version: null,
    provider: null,
    model: null,
    cwd: null,
    effort: null,
    mode: null,
    models: [],
    skills: [],
    mcp: [],
    // Past sessions for the `/sessions` picker, hydrated by a 'sessions' frame.
    sessions: [],
    turns: 0,
    usage: null,
    // Monotonic count of turn-end events. The waiting-queue drain keys on this
    // (the turn-end EVENT, like the native runner's turn_in_flight flag) rather
    // than a busy→idle status change, so a queued message still drains after a
    // turn that failed before turn_start (a lone turn_end leaves status idle).
    turnEnds: 0,
    // Chars streamed this turn (reset at turn_start), for a live output-token
    // estimate in the running status bar — mirrors the native TUI's chars/4.
    streamChars: 0,
  };
}

/** Reduce one Outbound frame into a new view state. */
export function reduceOutbound(state, frame) {
  if (!frame || typeof frame !== 'object') return state;
  switch (frame.kind) {
    case FRAME.EVENT:
      return reduceEvent(state, frame.data || {});
    case FRAME.REQUEST:
      return { ...state, request: toRequest(frame.data) };
    case FRAME.SNAPSHOT:
      return {
        ...state,
        sessionId: frame.data?.session_id ?? state.sessionId,
        version: frame.data?.version ?? state.version,
        provider: frame.data?.provider ?? state.provider,
        model: frame.data?.model ?? state.model,
        cwd: frame.data?.cwd ?? state.cwd,
        effort: frame.data?.effort ?? state.effort,
        mode: frame.data?.mode ?? state.mode,
        models: frame.data?.models ?? state.models,
        skills: frame.data?.skills ?? state.skills,
        mcp: frame.data?.mcp ?? state.mcp,
        request: frame.data?.pending_request ? toRequest(frame.data.pending_request) : null,
      };
    case FRAME.SESSIONS:
      // The `/sessions` picker's list (answers a `list_sessions` command).
      return { ...state, sessions: frame.data ?? [] };
    case FRAME.TRANSCRIPT:
      // A resumed session replayed as render-ready blocks: replace the
      // transcript wholesale and adopt the retargeted session id. A streaming
      // turn can't be in flight (resume only fires while idle), so clear it.
      return {
        ...state,
        blocks: (frame.data?.blocks ?? []).map(toBlock),
        stream: null,
        streamKind: null,
        sessionId: frame.data?.session_id ?? state.sessionId,
        turns: 0,
        usage: null,
      };
    default:
      return state;
  }
}

/** Map a wire transcript block to a view block (tool blocks resume done). */
function toBlock(b) {
  if (b.kind === 'tool') {
    return { kind: 'tool', id: '', name: b.name, args: b.args, done: true, result: b.result };
  }
  return b; // user / assistant / reasoning pass through unchanged.
}

function toRequest(data) {
  return { id: data.id, questions: data.questions || [] };
}

function reduceEvent(state, ev) {
  const p = ev.payload || {};
  switch (ev.type) {
    case EVENT.TURN_START:
      return { ...state, status: 'busy', turns: state.turns + 1, streamChars: 0 };
    case EVENT.TURN_END:
      return { ...state, status: 'idle', turnEnds: state.turnEnds + 1 };
    case EVENT.MESSAGE_START: {
      // A reasoning block opens as { reasoning_content: "", content: null };
      // a reply opens with content. Track which the stream is so its deltas
      // and final block are routed correctly (and rendered differently).
      const m = p.message || {};
      const kind = m.reasoning_content != null && m.content == null ? 'reasoning' : 'assistant';
      return { ...state, stream: '', streamKind: kind };
    }
    case EVENT.MESSAGE_UPDATE:
      return {
        ...state,
        stream: (state.stream ?? '') + (p.delta ?? ''),
        streamChars: state.streamChars + (p.delta ?? '').length,
      };
    case EVENT.MESSAGE_END: {
      const isReasoning = state.streamKind === 'reasoning';
      const text = (isReasoning ? p.message?.reasoning_content : p.message?.content) ?? state.stream ?? '';
      const blocks = text.trim().length
        ? [...state.blocks, { kind: isReasoning ? 'reasoning' : 'assistant', text }]
        : state.blocks;
      return { ...state, blocks, stream: null, streamKind: null };
    }
    case EVENT.USER_PROMPT_COMMITTED: {
      // Reconcile with any optimistic block the frontend showed at submit time:
      // replace a trailing `pending` user block (the committed text may differ
      // after a UserPromptSubmit hook), else append.
      const text = p.text ?? '';
      const last = state.blocks[state.blocks.length - 1];
      if (last && last.kind === 'user' && last.pending) {
        const blocks = state.blocks.slice(0, -1);
        blocks.push({ kind: 'user', text });
        return { ...state, blocks };
      }
      return { ...state, blocks: [...state.blocks, { kind: 'user', text }] };
    }
    case EVENT.USER_INJECTED:
      return { ...state, blocks: [...state.blocks, { kind: 'inject', text: p.text ?? '' }] };
    case EVENT.TOOL_EXECUTION_START:
      return {
        ...state,
        blocks: [
          ...state.blocks,
          { kind: 'tool', id: p.tool_call_id, name: p.tool_name, args: p.arguments, done: false },
        ],
      };
    case EVENT.TOOL_EXECUTION_END:
      return {
        ...state,
        blocks: state.blocks.map((b) =>
          b.kind === 'tool' && b.id === p.tool_call_id ? { ...b, done: true, result: p.result } : b,
        ),
      };
    case EVENT.WARNING:
      return {
        ...state,
        blocks: [...state.blocks, { kind: 'notice', text: `[warn] ${p.source}: ${p.message}` }],
      };
    case EVENT.NOTICE:
      // Neutral out-of-band line (e.g. /connect's "✓ Connected") — no [warn].
      return { ...state, blocks: [...state.blocks, { kind: 'notice', text: p.message ?? '' }] };
    case EVENT.RECONNECTING:
      return {
        ...state,
        blocks: [...state.blocks, { kind: 'notice', text: `⟳ reconnecting ${p.attempt}/${p.max}: ${p.reason}` }],
      };
    case EVENT.USAGE:
      // AgentEvent::Usage(Usage) — the payload IS the Usage struct.
      return { ...state, usage: p };
    default:
      // run_start / run_end — not surfaced in the minimal UI.
      return state;
  }
}

/**
 * Tool-call header summary: argument VALUES only, never param names — matching
 * the ratatui TUI (`grep("x")`, not `grep(pattern="x")`). Objects/arrays are
 * compact-JSON'd; the whole thing is capped so the header stays one line.
 */
export function toolArgsSummary(argsJson, cap = 80) {
  if (argsJson == null || argsJson === '') return '';
  let obj;
  try {
    obj = JSON.parse(argsJson);
  } catch {
    return clip(String(argsJson), cap);
  }
  if (obj == null || typeof obj !== 'object') return clip(String(obj), cap);
  const vals = Object.values(obj)
    .map((v) =>
      typeof v === 'string'
        ? v
        : v == null
          ? ''
          : typeof v === 'object'
            ? JSON.stringify(v)
            : String(v),
    )
    .filter((s) => s !== '');
  return clip(vals.join(', '), cap);
}

function clip(s, cap) {
  return s.length > cap ? s.slice(0, cap - 1) + '…' : s;
}

/**
 * Tool-result preview: the first few lines of `content` plus a "… N more lines"
 * count, mirroring the ratatui tool block (3 lines for success, 5 for errors).
 */
export function toolOutputPreview(content, isError = false) {
  const text = (content ?? '').replace(/\s+$/, '');
  if (!text) return { lines: [], more: 0 };
  const cap = isError ? 5 : 3;
  const all = text.split('\n');
  return { lines: all.slice(0, cap), more: Math.max(0, all.length - cap) };
}

/** Diff preview for edit_file results, matching the native TUI's larger cap. */
export function toolDiffPreview(content) {
  const text = (content ?? '').replace(/\s+$/, '');
  if (!text) return { adds: 0, dels: 0, lines: [], more: 0 };
  const all = text.split('\n');
  let adds = 0;
  let dels = 0;
  const classified = all.map((line) => {
    if (line.startsWith('+')) {
      adds += 1;
      return { text: line, kind: 'add' };
    }
    if (line.startsWith('-')) {
      dels += 1;
      return { text: line, kind: 'del' };
    }
    return { text: line, kind: 'ctx' };
  });
  const cap = 30;
  return { adds, dels, lines: classified.slice(0, cap), more: Math.max(0, classified.length - cap) };
}

// ── ClientCommand builders (return objects to JSON.stringify onto the wire) ──
export const submit = (text) => ({ kind: CMD.SUBMIT, data: { text } });
export const inject = (text) => ({ kind: CMD.INJECT, data: { text } });
export const cancel = () => ({ kind: CMD.CANCEL });
export const setSession = (sessionId) => ({ kind: CMD.SET_SESSION, data: { session_id: sessionId } });
export const newSession = () => ({ kind: CMD.NEW_SESSION });
// `effort` is the picked reasoning level (`null` = the model has no effort
// control); the engine applies + persists it exactly like the native picker.
export const setModel = (provider, model, effort = null) => ({
  kind: CMD.SET_MODEL,
  data: { provider, model, effort },
});
export const setMode = (mode) => ({ kind: CMD.SET_MODE, data: { mode } });
export const toggleSkill = (name) => ({ kind: CMD.TOGGLE_SKILL, data: { name } });
export const toggleMcp = (name) => ({ kind: CMD.TOGGLE_MCP, data: { name } });
export const listSessions = () => ({ kind: CMD.LIST_SESSIONS });
export const resumeSession = (sessionId) => ({ kind: CMD.RESUME_SESSION, data: { session_id: sessionId } });
export const copy = (text) => ({ kind: CMD.COPY, data: { text } });
export const reply = (id, answer) => ({ kind: CMD.REPLY, data: { id, answer } });

/**
 * Classify a submitted line. Returns `null` for a normal prompt, or `{ name }`
 * for a slash command (lowercased, no leading slash). The app decides which
 * commands it handles locally; `/compact` and unknown ones fall through to a
 * normal submit (the engine special-cases `/compact`).
 */
export function parseSlash(text) {
  const t = (text ?? '').trim();
  if (!t.startsWith('/')) return null;
  return { name: t.slice(1).split(/\s+/)[0].toLowerCase() };
}

// The slash commands the Ink frontend actually handles (a subset of the native
// TUI's). /connect is engine-driven (submitted, not handled locally); the
// remaining deferred ones (/telemetry, /hooks, /settings) aren't listed.
export const SLASH_COMMANDS = [
  { name: '/sessions', description: 'List sessions; Enter to resume' },
  { name: '/resume', description: 'Resume a past session' },
  { name: '/clear', description: 'Start a new session' },
  { name: '/compact', description: 'Summarize earlier history to free up context' },
  { name: '/copy', description: 'Copy the last assistant message to clipboard' },
  { name: '/model', description: 'Switch model' },
  { name: '/connect', description: 'Connect a provider (set API key + model)' },
  { name: '/skills', description: 'Manage skills (enable/disable)' },
  { name: '/mcp', description: 'Manage MCP servers (enable/disable)' },
  { name: '/afk', description: 'Toggle AFK / hands-free mode' },
];

/**
 * Slash-command autocomplete: matches for a line being typed (`/` + no space),
 * ranked prefix → name-substring → description-substring (case-insensitive),
 * mirroring the native TUI's `slash_suggestions`. Returns [] for normal text.
 */
export function slashSuggestions(text, commands = SLASH_COMMANDS) {
  const t = (text ?? '').trim();
  if (!t.startsWith('/') || /\s/.test(t)) return [];
  const q = t.toLowerCase();
  return commands
    .map((c) => {
      const n = c.name.toLowerCase();
      const rank = n.startsWith(q)
        ? 0
        : n.includes(q)
          ? 1
          : c.description.toLowerCase().includes(q.slice(1))
            ? 2
            : -1;
      return { c, rank };
    })
    .filter((x) => x.rank >= 0)
    .sort((a, b) => a.rank - b.rank)
    .map((x) => x.c);
}

/** Quote a session id for an `ignis --resume <id>` hint (bare if safe). */
export function quoteSessionId(id) {
  if (id && /^[A-Za-z0-9._/-]+$/.test(id)) return id;
  return `'${String(id ?? '').replace(/'/g, "'\\''")}'`;
}

/** Expand `[paste #N · M lines]` chips back to their stored paste contents. */
export function expandPastes(text, pastes) {
  return String(text ?? '').replace(/\[paste #(\d+) · [^\]]*\]/g, (m, n) => pastes[Number(n) - 1] ?? m);
}

// ReplyAnswer shapes (externally-tagged, matching the Rust enum):
//   Answered(vec) → {Answered:[…]} ; Cancelled → "Cancelled"
// Each element of the vec is one question's PickerAnswer:
//   Single(label)  → {Single: label}
//   Multi(labels)  → {Multi: [label, …]}   (selection order, non-empty)
export const pickSingle = (label) => ({ Single: label });
export const pickMulti = (labels) => ({ Multi: labels });
export const answered = (picks) => ({ Answered: picks });
export const answerCancelled = () => 'Cancelled';
// Convenience for the common single-question / single-select case.
export const answerSingle = (label) => answered([pickSingle(label)]);
