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
  answerSingle,
  answerCancelled,
  toolArgsSummary,
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
