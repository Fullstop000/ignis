// End-to-end coverage of the Ink app's currently-built features: rendering of
// every block kind, markdown, the composer, the full picker matrix, and the
// live-turn controls (cancel/inject). Driven through ink-testing-library + a
// mock engine (see harness.js).
import test from 'node:test';
import assert from 'node:assert/strict';
import { renderApp, plain, tick, ev, request, snapshot, KEY } from './harness.js';

const askOptions = (multi) => ({
  question: 'Pick a color',
  kind: 'ask_user',
  header: 'Color',
  multi_select: multi,
  options: [{ label: 'Red' }, { label: 'Green' }, { label: 'Blue' }],
  allow_other: true,
  text_input: false,
  mask: false,
});

test('streaming turn renders user + assistant blocks', async () => {
  const { engine, lastFrame } = renderApp();
  await tick();
  engine.emit(ev('turn_start'));
  engine.emit(ev('user_prompt_committed', { text: 'hi' }));
  engine.emit(ev('message_start'));
  engine.emit(ev('message_update', { delta: 'hel' }));
  engine.emit(ev('message_update', { delta: 'lo' }));
  engine.emit(ev('message_end', { message: { content: 'hello' } }));
  engine.emit(ev('turn_end'));
  await tick();
  const f = plain(lastFrame());
  assert.match(f, /▌ hi/);
  assert.match(f, /hello/);
});

test('typing a prompt + Enter emits a Submit command and clears the composer', async () => {
  const { engine, lastFrame, stdin } = renderApp();
  await tick();
  stdin.write('do the thing');
  await tick();
  assert.match(plain(lastFrame()), /do the thing/);
  stdin.write(KEY.enter);
  await tick();
  assert.deepEqual(engine.last(), { kind: 'submit', data: { text: 'do the thing' } });
  assert.doesNotMatch(plain(lastFrame()), /do the thing/, 'composer cleared after submit');
});

test('assistant markdown: heading, bold, inline code, list, fence', async () => {
  const { engine, lastFrame } = renderApp();
  await tick();
  const md = '## Head\n\nA **b** and `c`.\n\n- one\n- two\n\n```\ncode\n```';
  engine.emit(ev('message_end', { message: { content: md } }));
  await tick();
  const f = plain(lastFrame());
  assert.match(f, /Head/);
  assert.match(f, /A b and c\./, 'markers stripped'); // ** and ` consumed
  assert.match(f, /• one/);
  assert.match(f, /• two/);
  assert.match(f, /code/);
  assert.doesNotMatch(f, /\*\*/, 'no literal bold markers');
});

test('tool block: pending shows …, done drops it, values-only args', async () => {
  const { engine, lastFrame } = renderApp();
  await tick();
  engine.emit(ev('tool_execution_start', { tool_call_id: 't1', tool_name: 'bash', arguments: '{"command":"ls -l","timeout_secs":10}' }));
  await tick();
  let f = plain(lastFrame());
  assert.match(f, /● bash\(ls -l, 10\) …/, 'pending: values-only + ellipsis');
  engine.emit(ev('tool_execution_end', { tool_call_id: 't1', result: { content: 'line1\nline2\nline3\nline4\nline5', is_error: false } }));
  await tick();
  f = plain(lastFrame());
  assert.match(f, /● bash\(ls -l, 10\)/);
  assert.match(f, /╰ line1/, 'output preview under a gutter');
  assert.match(f, /line3/);
  assert.doesNotMatch(f, /line4/, 'capped at 3 lines for success');
  assert.match(f, /\+2 more lines/);
});

test('tool error output renders red, up to 5 lines', async () => {
  const { engine, lastFrame } = renderApp();
  await tick();
  engine.emit(ev('tool_execution_start', { tool_call_id: 'e1', tool_name: 'bash', arguments: '{"command":"boom"}' }));
  engine.emit(ev('tool_execution_end', { tool_call_id: 'e1', result: { content: 'a\nb\nc\nd\ne\nf', is_error: true } }));
  await tick();
  const f = plain(lastFrame());
  assert.match(f, /╰ a/);
  assert.match(f, /e\b/);
  assert.doesNotMatch(f, /^.*\bf\b.*$/m, 'capped at 5 for errors');
  assert.match(f, /\+1 more lines/);
});

test('inject + warning + reconnecting render their own blocks', async () => {
  const { engine, lastFrame } = renderApp();
  await tick();
  engine.emit(ev('user_injected', { text: 'steer left' }));
  engine.emit(ev('warning', { source: 'hooks', message: 'soft fail' }));
  engine.emit(ev('reconnecting', { attempt: 1, max: 3, reason: 'connection reset' }));
  await tick();
  const f = plain(lastFrame());
  assert.match(f, /↳ steer left/);
  assert.match(f, /\[warn\] hooks: soft fail/);
  assert.match(f, /⟳ reconnecting 1\/3: connection reset/);
});

test('welcome banner shows on an empty transcript, then disappears', async () => {
  const { engine, lastFrame } = renderApp();
  await tick();
  assert.match(plain(lastFrame()), /ignis/);
  assert.match(plain(lastFrame()), /Type a message/);
  engine.emit(ev('user_prompt_committed', { text: 'go' }));
  await tick();
  assert.doesNotMatch(plain(lastFrame()), /Type a message/, 'welcome gone once the transcript starts');
});

test('Ctrl+C cancels a busy turn (Cancel command), exits when idle', async () => {
  const { engine, stdin } = renderApp();
  await tick();
  engine.emit(ev('turn_start'));
  await tick();
  stdin.write(KEY.ctrlC);
  await tick();
  assert.deepEqual(engine.last(), { kind: 'cancel' }, 'busy → Cancel');

  engine.emit(ev('turn_end'));
  await tick();
  stdin.write(KEY.ctrlC);
  await tick();
  assert.deepEqual(engine.last(), { kind: '_closed' }, 'idle → engine.close()');
});

test('Ctrl+S injects the composer text', async () => {
  const { engine, stdin } = renderApp();
  await tick();
  stdin.write('go right');
  await tick();
  stdin.write(KEY.ctrlS);
  await tick();
  assert.deepEqual(engine.last(), { kind: 'inject', data: { text: 'go right' } });
});

test('picker single-select → Answered[Single]', async () => {
  const { engine, lastFrame, stdin } = renderApp();
  await tick();
  engine.emit(request(1, [askOptions(false)]));
  await tick();
  assert.match(plain(lastFrame()), /Pick a color/);
  stdin.write(KEY.down); // Red → Green
  await tick();
  stdin.write(KEY.enter);
  await tick();
  assert.deepEqual(engine.last(), { kind: 'reply', data: { id: 1, answer: { Answered: [{ Single: 'Green' }] } } });
});

test('picker multi-select → Answered[Multi] in selection order', async () => {
  const { engine, lastFrame, stdin } = renderApp();
  await tick();
  engine.emit(request(2, [askOptions(true)]));
  await tick();
  assert.match(plain(lastFrame()), /\[ \] Red/);
  // Tick between keys: a real terminal delivers them as separate events with a
  // re-render in between (so each handler sees fresh state).
  stdin.write(KEY.space); await tick(); // toggle Red (cursor 0)
  stdin.write(KEY.down); await tick();
  stdin.write(KEY.down); await tick(); // → Blue (cursor 2)
  stdin.write(KEY.space); await tick(); // toggle Blue
  assert.match(plain(lastFrame()), /\[x\] Blue/);
  stdin.write(KEY.enter);
  await tick();
  assert.deepEqual(engine.last(), { kind: 'reply', data: { id: 2, answer: { Answered: [{ Multi: ['Red', 'Blue'] }] } } });
});

test('picker "Other" free-text → Answered[Single(typed)]', async () => {
  const { engine, lastFrame, stdin } = renderApp();
  await tick();
  engine.emit(request(3, [askOptions(false)]));
  await tick();
  stdin.write(KEY.down);
  stdin.write(KEY.down);
  stdin.write(KEY.down); // → Other (row 3)
  await tick();
  stdin.write('Cyan');
  await tick();
  assert.match(plain(lastFrame()), /Other: Cyan/);
  stdin.write(KEY.enter);
  await tick();
  assert.deepEqual(engine.last(), { kind: 'reply', data: { id: 3, answer: { Answered: [{ Single: 'Cyan' }] } } });
});

test('picker Esc → Cancelled', async () => {
  const { engine, stdin } = renderApp();
  await tick();
  engine.emit(request(4, [askOptions(false)]));
  await tick();
  stdin.write(KEY.esc);
  await tick();
  assert.deepEqual(engine.last(), { kind: 'reply', data: { id: 4, answer: 'Cancelled' } });
});

test('multi-question picker advances and collects one answer each', async () => {
  const { engine, stdin } = renderApp();
  await tick();
  engine.emit(request(5, [askOptions(false), askOptions(false)]));
  await tick();
  stdin.write(KEY.enter); await tick(); // Q1 → Red (cursor 0)
  stdin.write(KEY.down); await tick();
  stdin.write(KEY.enter); await tick(); // Q2 → Green
  assert.deepEqual(engine.last(), {
    kind: 'reply',
    data: { id: 5, answer: { Answered: [{ Single: 'Red' }, { Single: 'Green' }] } },
  });
});

test('no React warnings (e.g. missing keys) leak to stderr while rendering rich blocks', async () => {
  const orig = console.error;
  const errs = [];
  console.error = (...a) => errs.push(a.join(' '));
  try {
    const { engine, stdin } = renderApp();
    await tick();
    engine.emit(ev('user_prompt_committed', { text: 'go' }));
    engine.emit(ev('tool_execution_start', { tool_call_id: 't', tool_name: 'bash', arguments: '{"command":"ls"}' }));
    engine.emit(ev('tool_execution_end', { tool_call_id: 't', result: { content: 'a\nb\nc\nd', is_error: false } }));
    engine.emit(ev('message_end', { message: { content: '# H\n- x\n- y\n\n`code`' } }));
    await tick();
    engine.emit(request(77, [{ question: 'q', multi_select: true, allow_other: true, options: [{ label: 'A' }, { label: 'B' }] }]));
    await tick();
    stdin.write(KEY.down);
    await tick();
  } finally {
    console.error = orig;
  }
  assert.deepEqual(errs, [], `unexpected console.error output: ${errs.join(' | ')}`);
});

test('statusline footer shows model, cwd, turns, and context tokens', async () => {
  const { engine, lastFrame } = renderApp();
  await tick();
  engine.emit(snapshot({ session_id: 's1', provider: 'minimax', model: 'M3', cwd: '/home/me/proj' }));
  engine.emit(ev('turn_start'));
  engine.emit(ev('usage', { input_tokens: 1234, output_tokens: 50 }));
  await tick();
  const f = plain(lastFrame());
  assert.match(f, /minimax\/M3/);
  assert.match(f, /proj/, 'cwd basename');
  assert.match(f, /1 turn\b/);
  assert.match(f, /1234 tok/);
});

test('snapshot hydrates a pending request (handover) → picker shown', async () => {
  const { engine, lastFrame } = renderApp();
  await tick();
  engine.emit(snapshot({ session_id: 's1', pending_request: { id: 9, questions: [askOptions(false)] } }));
  await tick();
  assert.match(plain(lastFrame()), /Pick a color/);
});
