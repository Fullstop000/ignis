// `node --test` — no dependencies, exercises the pure protocol reducer +
// command builders against the wire shapes the Rust engine emits/accepts.
import test from 'node:test';
import assert from 'node:assert/strict';
import {
  initialState,
  reduceOutbound,
  submit,
  cancel,
  inject,
  reply,
  setSession,
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
  answerSingle,
  answerCancelled,
  toolArgsSummary,
  toolOutputPreview,
  toolDiffPreview,
} from '../src/protocol.js';

test('a streaming assistant message accumulates deltas then finalizes', () => {
  let s = initialState();
  s = reduceOutbound(s, { kind: 'event', data: { type: 'turn_start' } });
  assert.equal(s.status, 'busy');
  s = reduceOutbound(s, { kind: 'event', data: { type: 'message_start' } });
  s = reduceOutbound(s, { kind: 'event', data: { type: 'message_update', payload: { delta: 'Hel' } } });
  s = reduceOutbound(s, { kind: 'event', data: { type: 'message_update', payload: { delta: 'lo' } } });
  assert.equal(s.stream, 'Hello');
  s = reduceOutbound(s, {
    kind: 'event',
    data: { type: 'message_end', payload: { message: { content: 'Hello' } } },
  });
  assert.equal(s.stream, null);
  assert.deepEqual(s.blocks.at(-1), { kind: 'assistant', text: 'Hello' });
  s = reduceOutbound(s, { kind: 'event', data: { type: 'turn_end' } });
  assert.equal(s.status, 'idle');
});

test('a reasoning stream finalizes to a reasoning block, not an assistant one', () => {
  // The engine opens a reasoning block with reasoning_content set + content
  // omitted (skip_serializing_if); text blocks open with content.
  let s = initialState();
  s = reduceOutbound(s, { kind: 'event', data: { type: 'message_start', payload: { message: { reasoning_content: '' } } } });
  assert.equal(s.streamKind, 'reasoning');
  s = reduceOutbound(s, { kind: 'event', data: { type: 'message_update', payload: { delta: 'let me ' } } });
  s = reduceOutbound(s, { kind: 'event', data: { type: 'message_update', payload: { delta: 'think' } } });
  assert.equal(s.stream, 'let me think');
  s = reduceOutbound(s, {
    kind: 'event',
    data: { type: 'message_end', payload: { message: { reasoning_content: 'let me think' } } },
  });
  assert.deepEqual(s.blocks.at(-1), { kind: 'reasoning', text: 'let me think' });
  assert.equal(s.stream, null);
  assert.equal(s.streamKind, null);

  // A subsequent text reply still becomes an assistant block.
  s = reduceOutbound(s, { kind: 'event', data: { type: 'message_start', payload: { message: { content: '' } } } });
  assert.equal(s.streamKind, 'assistant');
  s = reduceOutbound(s, { kind: 'event', data: { type: 'message_end', payload: { message: { content: 'the answer' } } } });
  assert.deepEqual(s.blocks.at(-1), { kind: 'assistant', text: 'the answer' });
});

test('user prompt and tool lifecycle render blocks', () => {
  // Append-only contract: tool_execution_start does NOT enter `blocks` (it
  // sets `state.activeTool` for the running status bar instead);
  // tool_execution_end is what pushes the completed tool block into `blocks`.
  let s = initialState();
  s = reduceOutbound(s, { kind: 'event', data: { type: 'user_prompt_committed', payload: { text: 'hi' } } });
  assert.deepEqual(s.blocks.at(-1), { kind: 'user', text: 'hi' });
  s = reduceOutbound(s, {
    kind: 'event',
    data: { type: 'tool_execution_start', payload: { tool_call_id: 't1', tool_name: 'bash', arguments: 'ls' } },
  });
  // Tool block is NOT in `blocks` yet — only `activeTool` carries it.
  assert.equal(s.blocks.length, 1, 'tool start does not enter blocks');
  assert.deepEqual(s.activeTool, { id: 't1', name: 'bash', args: 'ls' });
  s = reduceOutbound(s, {
    kind: 'event',
    data: { type: 'tool_execution_end', payload: { tool_call_id: 't1', result: {} } },
  });
  assert.equal(s.blocks.at(-1).done, true);
  assert.equal(s.blocks.at(-1).name, 'bash');
  assert.equal(s.activeTool, null, 'active tool cleared on _end');
});

test('a request frame sets a pending picker; snapshot hydrates session + clears it', () => {
  let s = initialState();
  s = reduceOutbound(s, { kind: 'request', data: { id: 5, questions: [{ question: 'pick?', options: [] }] } });
  assert.equal(s.request.id, 5);
  s = reduceOutbound(s, { kind: 'snapshot', data: { session_id: 'sess-1', pending_request: null } });
  assert.equal(s.sessionId, 'sess-1');
  assert.equal(s.request, null);
});

test('unknown / ignored events leave state untouched', () => {
  const s0 = initialState();
  const s1 = reduceOutbound(s0, { kind: 'event', data: { type: 'usage', payload: {} } });
  assert.deepEqual(s1.blocks, []);
  assert.equal(s1.status, 'idle');
});

test('command builders match the Rust ClientCommand wire shapes', () => {
  assert.deepEqual(submit('hi'), { kind: 'submit', data: { text: 'hi' } });
  assert.deepEqual(inject('go left'), { kind: 'inject', data: { text: 'go left' } });
  assert.deepEqual(cancel(), { kind: 'cancel' });
  assert.deepEqual(setSession('s2'), { kind: 'set_session', data: { session_id: 's2' } });
  assert.deepEqual(reply(7, answerSingle('Yes')), {
    kind: 'reply',
    data: { id: 7, answer: { Answered: [{ Single: 'Yes' }] } },
  });
  assert.deepEqual(reply(7, answerCancelled()), { kind: 'reply', data: { id: 7, answer: 'Cancelled' } });
  assert.deepEqual(newSession(), { kind: 'new_session' });
  assert.deepEqual(setModel('deepseek', 'v4'), {
    kind: 'set_model',
    data: { provider: 'deepseek', model: 'v4', effort: null },
  });
  assert.deepEqual(setModel('deepseek', 'v4', 'high'), {
    kind: 'set_model',
    data: { provider: 'deepseek', model: 'v4', effort: 'high' },
  });
  assert.deepEqual(setMode('hands_free'), { kind: 'set_mode', data: { mode: 'hands_free' } });
  assert.deepEqual(toggleSkill('git'), { kind: 'toggle_skill', data: { name: 'git' } });
  assert.deepEqual(toggleMcp('fs'), { kind: 'toggle_mcp', data: { name: 'fs' } });
  assert.deepEqual(listSessions(), { kind: 'list_sessions' });
  assert.deepEqual(resumeSession('session-x'), { kind: 'resume_session', data: { session_id: 'session-x' } });
  assert.deepEqual(copy('hello'), { kind: 'copy', data: { text: 'hello' } });
});

test('a sessions frame stores the picker list', () => {
  const list = [{ id: 'session-a', preview: 'hi', message_count: 3, last_modified: 100 }];
  const s = reduceOutbound(initialState(), { kind: 'sessions', data: list });
  assert.deepEqual(s.sessions, list);
});

test('a transcript frame replaces blocks (tool blocks resume done) and adopts the id', () => {
  // A non-trivial starting state to prove the transcript REPLACES, not appends.
  let s = reduceOutbound(initialState(), { kind: 'event', data: { type: 'user_prompt_committed', payload: { text: 'stale' } } });
  s = reduceOutbound(s, {
    kind: 'transcript',
    data: {
      session_id: 'session-z',
      blocks: [
        { kind: 'user', text: 'do it' },
        { kind: 'assistant', text: 'done' },
        { kind: 'tool', name: 'bash', args: 'ls', result: { content: 'out', is_error: false } },
      ],
    },
  });
  assert.equal(s.sessionId, 'session-z');
  assert.equal(s.blocks.length, 3, 'replaced, not appended');
  assert.deepEqual(s.blocks[0], { kind: 'user', text: 'do it' });
  // The tool block is reconstructed as a completed one so it renders green.
  assert.deepEqual(s.blocks[2], {
    kind: 'tool',
    id: '',
    name: 'bash',
    args: 'ls',
    done: true,
    result: { content: 'out', is_error: false },
  });
  assert.equal(s.stream, null);
});

test('slashSuggestions matches a typed /command, ignores normal text and args', () => {
  assert.deepEqual(slashSuggestions('hello'), []);
  assert.deepEqual(slashSuggestions('/model x'), [], 'a space ends suggestion mode');
  assert.ok(slashSuggestions('/').length >= 5, 'bare / lists commands');
  const m = slashSuggestions('/mo');
  assert.ok(
    m.some((c) => c.name === '/model') && !m.some((c) => c.name === '/clear'),
    'prefix-filters to /model',
  );
});

test('quoteSessionId leaves generated ids bare, single-quotes unsafe ones', () => {
  assert.equal(quoteSessionId('session-1700000000-ab12cd34'), 'session-1700000000-ab12cd34');
  assert.equal(quoteSessionId('a b'), "'a b'");
  assert.equal(quoteSessionId("it's"), "'it'\\''s'");
});

test('user_prompt_committed reconciles an optimistic pending user block', () => {
  let s = { ...initialState(), blocks: [{ kind: 'user', text: 'typed', pending: true }] };
  s = reduceOutbound(s, {
    kind: 'event',
    data: { type: 'user_prompt_committed', payload: { text: 'typed (after hook)' } },
  });
  assert.equal(s.blocks.length, 1, 'replaced the pending block, not appended');
  assert.deepEqual(s.blocks[0], { kind: 'user', text: 'typed (after hook)' });
});

test('streamChars accumulates message deltas and resets each turn', () => {
  let s = reduceOutbound(initialState(), { kind: 'event', data: { type: 'turn_start' } });
  s = reduceOutbound(s, { kind: 'event', data: { type: 'message_update', payload: { delta: 'abcd' } } });
  assert.equal(s.streamChars, 4);
  s = reduceOutbound(s, { kind: 'event', data: { type: 'turn_start' } });
  assert.equal(s.streamChars, 0, 'reset at the next turn');
});

test('parseSlash recognizes slash commands, ignores normal prompts', () => {
  assert.equal(parseSlash('hello world'), null);
  assert.equal(parseSlash('  not /a slash'), null);
  assert.deepEqual(parseSlash('/clear'), { name: 'clear' });
  assert.deepEqual(parseSlash('  /Model gpt '), { name: 'model' });
  assert.deepEqual(parseSlash('/compact'), { name: 'compact' });
});

test('expandPastes replaces chips with stored contents, leaves text otherwise', () => {
  const pastes = ['line1\nline2\nline3'];
  assert.equal(expandPastes('see [paste #1 · 3 lines] ok', pastes), 'see line1\nline2\nline3 ok');
  assert.equal(expandPastes('no chips here', pastes), 'no chips here');
  // Unknown index left as-is.
  assert.equal(expandPastes('[paste #9 · 2 lines]', pastes), '[paste #9 · 2 lines]');
});

test('toolArgsSummary shows values only, never param names', () => {
  assert.equal(toolArgsSummary('{"command":"sleep 6 && echo banana","timeout_secs":10}'), 'sleep 6 && echo banana, 10');
  assert.equal(toolArgsSummary('{"pattern":"foo"}'), 'foo');
  assert.equal(toolArgsSummary(''), '');
  assert.equal(toolArgsSummary(null), '');
  // Non-JSON falls back to the raw string; objects are compact-JSON'd.
  assert.equal(toolArgsSummary('not json'), 'not json');
  assert.equal(toolArgsSummary('{"opts":{"a":1}}'), '{"a":1}');
  // Capped to keep the header one line.
  assert.ok(toolArgsSummary(`{"x":"${'a'.repeat(200)}"}`).length <= 80);
});

test('toolOutputPreview caps lines (3 ok / 5 error) and counts the rest', () => {
  assert.deepEqual(toolOutputPreview(''), { lines: [], more: 0 });
  assert.deepEqual(toolOutputPreview('one\ntwo'), { lines: ['one', 'two'], more: 0 });
  assert.deepEqual(toolOutputPreview('a\nb\nc\nd\ne'), { lines: ['a', 'b', 'c'], more: 2 });
  assert.deepEqual(toolOutputPreview('a\nb\nc\nd\ne\nf\ng', true), { lines: ['a', 'b', 'c', 'd', 'e'], more: 2 });
  // Trailing whitespace trimmed before counting.
  assert.deepEqual(toolOutputPreview('only\n\n'), { lines: ['only'], more: 0 });
});

test('toolDiffPreview parses unified-diff hunks with line numbers', () => {
  // Empty body: no rows, nothing to count.
  assert.deepEqual(toolDiffPreview(''), { adds: 0, dels: 0, lines: [], more: 0 });

  // Single hunk: each row carries the source-file line number from the `@@`
  // header, the leading sign is stripped from `text`, and the `kind` is
  // exactly one of add | del | ctx.
  const single = '@@ -7,3 +7,4 @@\n let selectedContext = null;\n+let selectedChatId = null;\n let feedStream = null;\n let a = 1;\n';
  assert.deepEqual(toolDiffPreview(single), {
    adds: 1,
    dels: 0,
    lines: [
      { kind: 'ctx', text: 'let selectedContext = null;', lineNo: 7 },
      { kind: 'add', text: 'let selectedChatId = null;', lineNo: 8 },
      { kind: 'ctx', text: 'let feedStream = null;', lineNo: 9 },
      { kind: 'ctx', text: 'let a = 1;', lineNo: 10 },
    ],
    more: 0,
  });

  // Two non-contiguous hunks: a `'gap'` row is synthesized between them so
  // the view can render `⋮`. Line numbers reset to each hunk header.
  const twoHunks =
    '@@ -1,2 +1,2 @@\n a\n-b\n+B\n@@ -10,2 +10,2 @@\n y\n-z\n+Z\n';
  const out = toolDiffPreview(twoHunks);
  assert.equal(out.adds, 2);
  assert.equal(out.dels, 2);
  // First hunk rows.
  assert.deepEqual(out.lines[0], { kind: 'ctx', text: 'a', lineNo: 1 });
  assert.deepEqual(out.lines[1], { kind: 'del', text: 'b', lineNo: 2 });
  assert.deepEqual(out.lines[2], { kind: 'add', text: 'B', lineNo: 2 });
  // The synthesized gap separating the two hunks.
  assert.deepEqual(out.lines[3], { kind: 'gap', text: '', lineNo: null });
  // Second hunk rows.
  assert.deepEqual(out.lines[4], { kind: 'ctx', text: 'y', lineNo: 10 });
  assert.deepEqual(out.lines[5], { kind: 'del', text: 'z', lineNo: 11 });
  assert.deepEqual(out.lines[6], { kind: 'add', text: 'Z', lineNo: 11 });

  // Cap at 30 rows; the rest spill into `more`. Construct one giant hunk so
  // the cap is exercised on a single hunk.
  const adds = Array.from({ length: 32 }, (_, i) => `+line ${i}`).join('\n');
  const giant = `@@ -1,0 +1,32 @@\n${adds}\n`;
  const big = toolDiffPreview(giant);
  assert.equal(big.adds, 32);
  assert.equal(big.lines.length, 30);
  assert.equal(big.more, 2);
  assert.equal(big.lines[0].kind, 'add');
  assert.equal(big.lines[0].lineNo, 1);
  assert.equal(big.lines[29].lineNo, 30);

  // `\ No newline at end of file` is metadata, not a real diff row, and must
  // not show up in the output.
  const trailer = '@@ -1,1 +1,1 @@\n-old\n\\ No newline at end of file\n+new\n';
  const tr = toolDiffPreview(trailer);
  assert.equal(tr.lines.length, 2);
  assert.equal(tr.lines[0].kind, 'del');
  assert.equal(tr.lines[1].kind, 'add');

  // Defensive fallback: a body without any `@@` header still classifies by
  // sign so the view renders something. `lineNo` stays null in this path.
  const headerless = '+a\n-b\n c';
  const hl = toolDiffPreview(headerless);
  assert.equal(hl.adds, 1);
  assert.equal(hl.dels, 1);
  assert.equal(hl.lines.length, 3);
  assert.equal(hl.lines[0].lineNo, null);

  // The ratatui engine appends a `… N more diff lines truncated` notice after
  // the real diff body when it exceeds its internal cap. It must not be parsed
  // as a context line with a synthesized line number.
  const truncated = '@@ -1,2 +1,2 @@\n-old\n+new\n… 5 more diff lines truncated';
  const trunc = toolDiffPreview(truncated);
  assert.equal(trunc.adds, 1);
  assert.equal(trunc.dels, 1);
  assert.equal(trunc.lines.length, 2);
});
