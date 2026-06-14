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
  expandPastes,
  answerSingle,
  answerCancelled,
  toolArgsSummary,
  toolOutputPreview,
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
  let s = initialState();
  s = reduceOutbound(s, { kind: 'event', data: { type: 'user_prompt_committed', payload: { text: 'hi' } } });
  assert.deepEqual(s.blocks.at(-1), { kind: 'user', text: 'hi' });
  s = reduceOutbound(s, {
    kind: 'event',
    data: { type: 'tool_execution_start', payload: { tool_call_id: 't1', tool_name: 'bash', arguments: 'ls' } },
  });
  assert.equal(s.blocks.at(-1).done, false);
  assert.equal(s.blocks.at(-1).name, 'bash');
  s = reduceOutbound(s, {
    kind: 'event',
    data: { type: 'tool_execution_end', payload: { tool_call_id: 't1', result: {} } },
  });
  assert.equal(s.blocks.at(-1).done, true);
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
  assert.deepEqual(setModel('deepseek', 'v4'), { kind: 'set_model', data: { provider: 'deepseek', model: 'v4' } });
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
