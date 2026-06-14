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

export function initialState() {
  return {
    blocks: [],
    stream: null,
    status: 'idle',
    sessionId: null,
    request: null,
    // Statusline meta (from the startup snapshot) + live counters.
    provider: null,
    model: null,
    cwd: null,
    mode: null,
    models: [],
    skills: [],
    mcp: [],
    // Past sessions for the `/sessions` picker, hydrated by a 'sessions' frame.
    sessions: [],
    turns: 0,
    usage: null,
  };
}

/** Reduce one Outbound frame into a new view state. */
export function reduceOutbound(state, frame) {
  if (!frame || typeof frame !== 'object') return state;
  switch (frame.kind) {
    case 'event':
      return reduceEvent(state, frame.data || {});
    case 'request':
      return { ...state, request: toRequest(frame.data) };
    case 'snapshot':
      return {
        ...state,
        sessionId: frame.data?.session_id ?? state.sessionId,
        provider: frame.data?.provider ?? state.provider,
        model: frame.data?.model ?? state.model,
        cwd: frame.data?.cwd ?? state.cwd,
        mode: frame.data?.mode ?? state.mode,
        models: frame.data?.models ?? state.models,
        skills: frame.data?.skills ?? state.skills,
        mcp: frame.data?.mcp ?? state.mcp,
        request: frame.data?.pending_request ? toRequest(frame.data.pending_request) : null,
      };
    case 'sessions':
      // The `/sessions` picker's list (answers a `list_sessions` command).
      return { ...state, sessions: frame.data ?? [] };
    case 'transcript':
      // A resumed session replayed as render-ready blocks: replace the
      // transcript wholesale and adopt the retargeted session id. A streaming
      // turn can't be in flight (resume only fires while idle), so clear it.
      return {
        ...state,
        blocks: (frame.data?.blocks ?? []).map(toBlock),
        stream: null,
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
  return b; // user / assistant pass through unchanged.
}

function toRequest(data) {
  return { id: data.id, questions: data.questions || [] };
}

function reduceEvent(state, ev) {
  const p = ev.payload || {};
  switch (ev.type) {
    case 'turn_start':
      return { ...state, status: 'busy', turns: state.turns + 1 };
    case 'turn_end':
      return { ...state, status: 'idle' };
    case 'message_start':
      return { ...state, stream: '' };
    case 'message_update':
      return { ...state, stream: (state.stream ?? '') + (p.delta ?? '') };
    case 'message_end': {
      const text = p.message?.content ?? state.stream ?? '';
      const blocks = text.trim().length
        ? [...state.blocks, { kind: 'assistant', text }]
        : state.blocks;
      return { ...state, blocks, stream: null };
    }
    case 'user_prompt_committed':
      return { ...state, blocks: [...state.blocks, { kind: 'user', text: p.text ?? '' }] };
    case 'user_injected':
      return { ...state, blocks: [...state.blocks, { kind: 'inject', text: p.text ?? '' }] };
    case 'tool_execution_start':
      return {
        ...state,
        blocks: [
          ...state.blocks,
          { kind: 'tool', id: p.tool_call_id, name: p.tool_name, args: p.arguments, done: false },
        ],
      };
    case 'tool_execution_end':
      return {
        ...state,
        blocks: state.blocks.map((b) =>
          b.kind === 'tool' && b.id === p.tool_call_id ? { ...b, done: true, result: p.result } : b,
        ),
      };
    case 'warning':
      return {
        ...state,
        blocks: [...state.blocks, { kind: 'notice', text: `[warn] ${p.source}: ${p.message}` }],
      };
    case 'reconnecting':
      return {
        ...state,
        blocks: [...state.blocks, { kind: 'notice', text: `⟳ reconnecting ${p.attempt}/${p.max}: ${p.reason}` }],
      };
    case 'usage':
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

// ── ClientCommand builders (return objects to JSON.stringify onto the wire) ──
export const submit = (text) => ({ kind: 'submit', data: { text } });
export const inject = (text) => ({ kind: 'inject', data: { text } });
export const cancel = () => ({ kind: 'cancel' });
export const setSession = (sessionId) => ({ kind: 'set_session', data: { session_id: sessionId } });
export const newSession = () => ({ kind: 'new_session' });
export const setModel = (provider, model) => ({ kind: 'set_model', data: { provider, model } });
export const setMode = (mode) => ({ kind: 'set_mode', data: { mode } });
export const toggleSkill = (name) => ({ kind: 'toggle_skill', data: { name } });
export const toggleMcp = (name) => ({ kind: 'toggle_mcp', data: { name } });
export const listSessions = () => ({ kind: 'list_sessions' });
export const resumeSession = (sessionId) => ({ kind: 'resume_session', data: { session_id: sessionId } });
export const reply = (id, answer) => ({ kind: 'reply', data: { id, answer } });

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
