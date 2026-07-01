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
  TODOS: 'todos',
  BACKGROUND_SHELLS: 'background_shells',
  FOLLOW_UPS: 'follow_ups',
  COMPACT_START: 'compact_start',
  COMPACT_END: 'compact_end',
  COMPACT_REPORT: 'compact_report',
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
  SET_SETTING: 'set_setting',
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
    // True while the engine is compacting context (summarizing old history).
    // Drives a dedicated "Compacting…" spinner that shows even when status is
    // still 'idle' (auto-compact fires before turn_start). Toggled by
    // compact_start / compact_end events; reset on transcript replace.
    compacting: false,
    // Statusline meta (from the startup snapshot) + live counters.
    version: null,
    provider: null,
    model: null,
    cwd: null,
    // Current git branch of `cwd` (oh-my-zsh `git:(branch)` segment); `null`
    // outside a work tree. Updated by every snapshot (the engine refreshes it
    // at turn-end so a mid-session `git checkout` is reflected).
    gitBranch: null,
    effort: null,
    mode: null,
    // Generic config knobs for the /settings panel (engine-owned registry).
    // Each: { id, label, help, section, kind, value }. Footer also reads the
    // statusline.* entries to decide which segments to show.
    settings: [],
    models: [],
    skills: [],
    mcp: [],
    // Past sessions for the `/sessions` picker, hydrated by a 'sessions' frame.
    sessions: [],
    turns: 0,
    usage: null,
    // The agent's task list (`todo_write`). Rendered as a checklist panel above
    // the composer while non-empty; set by the `todos` event, reset on resume.
    todos: [],
    // Number of live background shells (bash run_in_background); the footer shows
    // `⚙ N bg` while > 0. Updated by the `background_shells` event.
    bgShells: 0,
    // Suggested next prompts for the just-ended turn (from the model's stripped
    // <follow_ups> block). Ephemeral: shown idle-only, cleared on the next turn.
    followUps: [],
    // Currently-running tool calls, keyed by tool_call_id, for the running
    // status bar's inline indicator. The agent runs tools concurrently
    // (`buffer_unordered` in agent/mod.rs), so this is a map, not a single
    // slot — overwriting on each `_start` would lose the name/args of the
    // earlier tool by the time its `_end` arrived. Each entry is
    // { name, args, startedAt }; insertion order is preserved (Map semantics
    // via plain object + ECMAScript string-keyed insertion order). The
    // committed tool block is pushed to `blocks` on `_end`, then committed to
    // scrollback as a single <Static> item — pending tool calls never enter
    // the transcript.
    activeTools: {},
    // Monotonic count of turn-end events. The waiting-queue drain keys on this
    // (the turn-end EVENT, like the native runner's turn_in_flight flag) rather
    // than a busy→idle status change, so a queued message still drains after a
    // turn that failed before turn_start (a lone turn_end leaves status idle).
    turnEnds: 0,
    // Chars streamed this turn (reset at turn_start), for a live output-token
    // estimate in the running status bar — mirrors the native TUI's chars/4.
    streamChars: 0,
    // Bumped whenever the transcript is replaced wholesale (/clear, resume) so
    // the committed <Static> scrollback region remounts and re-renders from
    // scratch — Ink's <Static> is append-only and won't otherwise reset.
    generation: 0,
    // Whether the welcome banner has been committed to scrollback. Once true,
    // the banner lives in <Static> and is no longer rendered in the dynamic
    // region, matching the native TUI behavior where the banner persists in
    // scrollback after the first prompt.
    welcomeShown: false,
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
        // Snapshot's `git_branch` (snake_case on the wire). `null` is a valid
        // value (cwd outside a work tree) and overrides the prior value;
        // `undefined` (field absent) preserves it — matching effort/cwd/etc.
        gitBranch: 'git_branch' in (frame.data || {}) ? frame.data.git_branch : state.gitBranch,
        effort: frame.data?.effort ?? state.effort,
        mode: frame.data?.mode ?? state.mode,
        settings: frame.data?.settings ?? state.settings,
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
      const newBlocks = (frame.data?.blocks ?? []).map(toBlock);
      return {
        ...state,
        blocks: newBlocks,
        stream: null,
        streamKind: null,
        sessionId: frame.data?.session_id ?? state.sessionId,
        turns: 0,
        usage: null,
        // Resume/clear replaces the whole context; the resumed session's todos
        // (if any) re-emit on its next prompt.
        todos: [],
        followUps: [],
        activeTools: {},
        compacting: false,
        generation: state.generation + 1,
        // On /clear the banner returns to the dynamic region; on resume it
        // stays in scrollback if it was already committed.
        welcomeShown: newBlocks.length > 0 ? state.welcomeShown : false,
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
      // A new turn invalidates the previous turn's suggestions and any stale
      // active-tool indicators (a turn that crashed mid-tool may have left
      // some).
      return { ...state, status: 'busy', turns: state.turns + 1, streamChars: 0, followUps: [], activeTools: {} };
    case EVENT.TURN_END:
      return { ...state, status: 'idle', compacting: false, turnEnds: state.turnEnds + 1 };
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
      const content = p.message?.content;
      const reasoning = p.message?.reasoning_content;
      const blocks = [...state.blocks];
      const pushBlock = (kind, text) => {
        if ((text ?? '').trim().length) blocks.push({ kind, text });
      };

      if (state.streamKind === 'reasoning') {
        pushBlock('reasoning', reasoning ?? state.stream ?? '');
        // Some providers' final assistant snapshot can carry both the completed
        // reasoning and the visible answer. Do not let a stale reasoning stream
        // classification hide the answer.
        pushBlock('assistant', content);
      } else if (state.streamKind === 'assistant') {
        pushBlock('assistant', content ?? state.stream ?? '');
      } else if (content != null) {
        pushBlock('assistant', content);
      } else {
        pushBlock('reasoning', reasoning ?? state.stream ?? '');
      }
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
        return { ...state, blocks, welcomeShown: true };
      }
      return { ...state, blocks: [...state.blocks, { kind: 'user', text }], welcomeShown: true };
    }
    case EVENT.USER_INJECTED:
      return { ...state, blocks: [...state.blocks, { kind: 'inject', text: p.text ?? '' }] };
    case EVENT.TOOL_EXECUTION_START:
      // Append-only: the tool call is NOT added to `blocks` here. While it
      // runs, the running status bar shows it inline (`● bash(...) running`)
      // via `state.activeTools[tool_call_id]`. The full block — header +
      // result preview — is pushed to `blocks` on `_end`, then committed to
      // scrollback as a single <Static> item. This keeps the dynamic region
      // bounded so Ink never trips its `outputHeight >= rows`
      // full-screen-clear path.
      //
      // The agent runs tools concurrently (buffer_unordered in
      // agent/mod.rs), so the engine emits multiple Starts before the first
      // End: every active tool's name/args must survive until ITS own End,
      // not be overwritten by the next Start.
      return {
        ...state,
        activeTools: {
          ...state.activeTools,
          [p.tool_call_id]: { name: p.tool_name, args: p.arguments, startedAt: Date.now() },
        },
      };
    case EVENT.TOOL_EXECUTION_END: {
      const id = p.tool_call_id;
      const active = state.activeTools[id];
      // Use the active-tool name/args if we have them; otherwise the call may
      // pre-date this session (defensive — shouldn't happen).
      const name = active?.name ?? '';
      const args = active?.args ?? '';
      const nextActive = { ...state.activeTools };
      delete nextActive[id];
      return {
        ...state,
        blocks: [
          ...state.blocks,
          { kind: 'tool', id, name, args, done: true, result: p.result },
        ],
        activeTools: nextActive,
      };
    }
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
    case EVENT.TODOS:
      // AgentEvent::Todos { items } — the complete task list (full replace).
      return { ...state, todos: p.items ?? [] };
    case EVENT.BACKGROUND_SHELLS:
      // AgentEvent::BackgroundShells { running } — live background-shell count.
      return { ...state, bgShells: p.running ?? 0 };
    case EVENT.FOLLOW_UPS:
      // AgentEvent::FollowUps { items } — suggested next prompts for this turn.
      return { ...state, followUps: p.items ?? [] };
    case EVENT.COMPACT_START:
      return { ...state, compacting: true };
    case EVENT.COMPACT_END:
      return { ...state, compacting: false };
    case EVENT.COMPACT_REPORT:
      // Compaction replaced the old conversation prefix with a summary in the
      // engine's history. Mirror that on screen: drop the old blocks, leave
      // only the compaction report, and bump generation so <Static> remounts
      // and the terminal scrollback is wiped (same pattern as /clear and
      // transcript-replace). Same event on both the auto-compact and manual
      // /compact paths, rendered identically.
      return {
        ...state,
        blocks: [
          {
            kind: 'compaction',
            before: p.before ?? 0,
            after: p.after ?? 0,
            summary: p.summary ?? '',
          },
        ],
        generation: state.generation + 1,
      };
    default:
      // run_start / run_end — not surfaced in the minimal UI.
      return state;
  }
}

/**
 * Tool-call header summary: argument VALUES only, never param names — matching
 * the ratatui TUI (`grep("x")`, not `grep(pattern="x")`). Objects/arrays are
 * compact-JSON'd; the whole thing is capped so the header stays one line.
 *
 * `todo_write` is special-cased: its `todos` array would otherwise dump as raw
 * JSON, so it renders a friendly task tally instead (`3 tasks · 1✓ 1◐ 1◻`),
 * using the same glyphs as the checklist panel.
 */
export function toolArgsSummary(argsJson, toolName, cap = 80) {
  if (argsJson == null || argsJson === '') return '';
  let obj;
  try {
    obj = JSON.parse(argsJson);
  } catch {
    return clip(String(argsJson), cap);
  }
  if (obj == null || typeof obj !== 'object') return clip(String(obj), cap);
  if (toolName === 'todo_write' && Array.isArray(obj.todos)) {
    return clip(todoTally(obj.todos), cap);
  }
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

/** `{n} tasks · {done}✓ {active}◐ {pending}◻` — non-zero statuses only. */
function todoTally(todos) {
  const n = todos.length;
  const head = `${n} ${n === 1 ? 'task' : 'tasks'}`;
  if (n === 0) return head;
  const count = (status) => todos.filter((t) => t.status === status).length;
  const tally = [];
  const done = count('completed');
  const active = count('in_progress');
  const pending = n - done - active;
  if (done) tally.push(`${done}✓`);
  if (active) tally.push(`${active}◐`);
  if (pending) tally.push(`${pending}◻`);
  return tally.length ? `${head} · ${tally.join(' ')}` : head;
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

/**
 * Parse the unified-diff body that `edit_file` returns into a render-ready
 * sequence of rows for the Ink ToolBlock view. Each non-header row carries:
 *   - `kind`: `'add' | 'del' | 'ctx' | 'gap'`
 *   - `text`: the line content with the leading `+`/`-`/` ` sign stripped
 *   - `lineNo`: the source-file line number to show in the gutter (the
 *     **new-file** line for `add`/`ctx`, the **old-file** line for `del`).
 *     `null` for `'gap'` rows.
 *
 * `'gap'` rows are synthesized between consecutive hunks so the view can
 * render a `⋮` separator (the unified diff itself just emits a fresh
 * `@@ … @@` header without an explicit gap marker).
 *
 * Hunk headers (`@@ -a,b +c,d @@`) seed line counters; an absent `,N` count
 * defaults to 1 per the unidiff spec. Lines outside any hunk are ignored
 * (shouldn't appear in our output, but be defensive — the engine sometimes
 * splices in an `\\ No newline at end of file` marker which is not a hunk
 * line and shouldn't carry a line number).
 *
 * `cap = 30` matches the native TUI; overflow is reported as `more`.
 */
export function toolDiffPreview(content) {
  const text = (content ?? '').replace(/\s+$/, '');
  if (!text) return { adds: 0, dels: 0, lines: [], more: 0 };
  const HUNK_RE = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/;
  let adds = 0;
  let dels = 0;
  let oldLn = 0;
  let newLn = 0;
  let inHunk = false;
  let sawAnyHunk = false;
  const classified = [];
  for (const raw of text.split('\n')) {
    const m = raw.match(HUNK_RE);
    if (m) {
      if (sawAnyHunk) classified.push({ kind: 'gap', text: '', lineNo: null });
      oldLn = parseInt(m[1], 10);
      newLn = parseInt(m[2], 10);
      inHunk = true;
      sawAnyHunk = true;
      continue;
    }
    // Skip the `\ No newline at end of file` marker (`\\` is the unidiff
    // convention) — it's metadata, not a real diff line.
    if (raw.startsWith('\\')) continue;
    // Skip the ratatui-engine truncation notice (`… N more diff lines truncated`);
    // it is appended after the real diff body and should not carry a line number.
    if (raw.startsWith('…')) continue;
    if (!inHunk) continue;
    const sign = raw[0];
    const body = raw.slice(1);
    if (sign === '+') {
      adds += 1;
      classified.push({ kind: 'add', text: body, lineNo: newLn });
      newLn += 1;
    } else if (sign === '-') {
      dels += 1;
      classified.push({ kind: 'del', text: body, lineNo: oldLn });
      oldLn += 1;
    } else {
      // Context line: leading space (or empty line in the diff body).
      classified.push({ kind: 'ctx', text: body, lineNo: newLn });
      oldLn += 1;
      newLn += 1;
    }
  }
  // Fallback: if the body had no `@@` header at all (e.g. a stub `(no
  // changes)` payload, or an old format), classify each row by its sign so
  // the view still renders something useful — `lineNo` stays null.
  if (!sawAnyHunk) {
    for (const raw of text.split('\n')) {
      if (raw.startsWith('+')) {
        adds += 1;
        classified.push({ kind: 'add', text: raw.slice(1), lineNo: null });
      } else if (raw.startsWith('-')) {
        dels += 1;
        classified.push({ kind: 'del', text: raw.slice(1), lineNo: null });
      } else {
        classified.push({ kind: 'ctx', text: raw, lineNo: null });
      }
    }
  }
  const cap = 30;
  return {
    adds,
    dels,
    lines: classified.slice(0, cap),
    more: Math.max(0, classified.length - cap),
  };
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
export const setSetting = (id, value) => ({ kind: CMD.SET_SETTING, data: { id, value } });
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
// remaining deferred ones (/telemetry, /hooks) aren't listed.
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
  { name: '/settings', description: 'Settings — sandbox, statusline, …' },
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
