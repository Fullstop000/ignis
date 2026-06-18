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
  const f = plain(lastFrame());
  assert.match(f, /▌ do the thing/, 'message shows immediately as an optimistic user block');
  assert.match(f, /Type a message…/, 'composer reset to its placeholder');
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

test('assistant markdown table renders aligned header + rows', async () => {
  const { engine, lastFrame } = renderApp();
  await tick();
  engine.emit(ev('message_end', { message: { content: '| Name | Age |\n|---|---|\n| Ann | 30 |\n| Bob | 25 |' } }));
  await tick();
  const f = plain(lastFrame());
  assert.match(f, /Name.*Age/);
  assert.match(f, /Ann.*30/);
  assert.match(f, /Bob.*25/);
  assert.match(f, /─/, 'header rule rendered');
});

test('table cells render inline markdown (no literal ** markers)', async () => {
  const { engine, lastFrame } = renderApp();
  await tick();
  engine.emit(
    ev('message_end', {
      message: { content: '| Prime | Divisible? |\n|---|---|\n| **7** | **Yes** |\n| `5` | No |' },
    }),
  );
  await tick();
  const f = plain(lastFrame());
  // The bold/code markers are consumed by the inline parser, not shown raw.
  assert.match(f, /7.*Yes/);
  assert.match(f, /5.*No/);
  assert.doesNotMatch(f, /\*\*/, 'no literal ** in cells');
  assert.doesNotMatch(f, /`/, 'no literal backtick in cells');
});

test('picker shows the focused option preview pane', async () => {
  const { engine, lastFrame, stdin } = renderApp();
  await tick();
  engine.emit(
    request(8, [
      {
        question: 'Pick a layout',
        multi_select: false,
        allow_other: false,
        options: [
          { label: 'A', preview: 'AAAA\nAAAA' },
          { label: 'B', preview: 'BBBB\nBBBB' },
        ],
      },
    ]),
  );
  await tick();
  assert.match(plain(lastFrame()), /AAAA/, 'preview of the focused option (A)');
  stdin.write(KEY.down);
  await tick();
  assert.match(plain(lastFrame()), /BBBB/, 'preview follows focus to B');
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

test('edit_file tool result renders a diff summary and hunk instead of generic preview', async () => {
  const { engine, lastFrame } = renderApp();
  await tick();
  engine.emit(
    ev('tool_execution_start', {
      tool_call_id: 'd1',
      tool_name: 'edit_file',
      arguments: '{"path":"src/main.rs","old_text":"old","new_text":"new"}',
    }),
  );
  engine.emit(
    ev('tool_execution_end', {
      tool_call_id: 'd1',
      result: { content: '- old\n+ new\n- removed\n+ added', is_error: false },
    }),
  );
  await tick();
  const f = plain(lastFrame());
  assert.match(f, /\+2 -2/, 'shows added/deleted line summary');
  assert.match(f, /╰ - old/, 'shows removed diff line');
  assert.match(f, /\+ added/, 'shows added diff line');
  assert.doesNotMatch(f, /more lines/, 'edit diffs use the larger diff cap, not the generic 3-line cap');
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
  // The welcome is the ASCII-art IGNIS banner (block glyphs), shown only on the
  // empty startup screen.
  assert.match(plain(lastFrame()), /██/);
  engine.emit(ev('user_prompt_committed', { text: 'go' }));
  await tick();
  assert.doesNotMatch(plain(lastFrame()), /██/, 'welcome gone once the transcript starts');
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

test('Ctrl+S injects the composer text during a turn', async () => {
  const { engine, stdin } = renderApp();
  await tick();
  engine.emit(ev('turn_start')); // busy — inject only steers an in-flight turn
  await tick();
  stdin.write('go right');
  await tick();
  stdin.write(KEY.ctrlS);
  await tick();
  assert.deepEqual(engine.last(), { kind: 'inject', data: { text: 'go right' } });
});

test('Ctrl+S while idle is a no-op that keeps the composer text', async () => {
  const { engine, stdin, lastFrame } = renderApp();
  await tick();
  stdin.write('not an inject');
  await tick();
  stdin.write(KEY.ctrlS); // idle: no inject sink, so must not send + must not clear
  await tick();
  assert.ok(!engine.sent.some((c) => c.kind === 'inject'), 'no inject sent when idle');
  assert.match(plain(lastFrame()), /not an inject/, 'composer text preserved');
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

test('a masked text-input picker (/connect API key) hides the typed key and replies with it', async () => {
  const { engine, lastFrame, stdin } = renderApp();
  await tick();
  engine.emit(
    request(20, [
      {
        question: 'Paste your API key for OpenAI',
        kind: 'connect',
        header: 'API Key',
        multi_select: false,
        options: [],
        allow_other: false,
        text_input: true,
        mask: true,
      },
    ]),
  );
  await tick();
  stdin.write('sk-secret');
  await tick();
  const f = plain(lastFrame());
  assert.match(f, /Paste your API key/);
  assert.match(f, /●{9}/, 'typed key is masked as dots');
  assert.doesNotMatch(f, /sk-secret/, 'plaintext key never rendered');
  stdin.write(KEY.enter);
  await tick();
  assert.deepEqual(engine.last(), { kind: 'reply', data: { id: 20, answer: { Answered: [{ Single: 'sk-secret' }] } } });
});

test('a notice event renders a clean line without the [warn] prefix', async () => {
  const { engine, lastFrame } = renderApp();
  await tick();
  engine.emit(ev('notice', { message: '✓ Connected to openai. Active model: openai/gpt-5.5.' }));
  await tick();
  const f = plain(lastFrame());
  assert.match(f, /✓ Connected to openai/);
  assert.doesNotMatch(f, /\[warn\]/, 'notice is not framed as a warning');
});

test('/connect is submitted to the engine (engine owns the wizard)', async () => {
  const { engine, stdin } = renderApp();
  await tick();
  stdin.write('/connect');
  await tick();
  stdin.write(KEY.enter);
  await tick();
  assert.deepEqual(engine.last(), { kind: 'submit', data: { text: '/connect' } });
});

test('/clear sends new_session, wipes the screen, and shows the empty welcome', async () => {
  const { engine, lastFrame, frames, stdin } = renderApp();
  await tick();
  engine.emit(ev('user_prompt_committed', { text: 'earlier message' }));
  engine.emit(ev('message_end', { message: { content: 'earlier reply' } }));
  await tick();
  assert.match(plain(lastFrame()), /earlier message/);

  const clearsBefore = frames.filter((f) => f.includes('\x1b[2J')).length;
  stdin.write('/clear');
  await tick();
  stdin.write(KEY.enter);
  await tick();
  assert.deepEqual(engine.last(), { kind: 'new_session' });
  // The committed transcript was flushed to <Static> (permanent scrollback), so
  // /clear can't unwrite it in-buffer — it emits a screen+scrollback wipe and
  // re-shows the empty welcome. (Debug-mode lastFrame retains flushed static
  // output, so the visual clear is asserted via the wipe escape, not text absence.)
  assert.ok(frames.filter((f) => f.includes('\x1b[2J')).length > clearsBefore, 'screen wiped on /clear');
  assert.match(plain(lastFrame()), /Type a message/, 'welcome banner returns on the empty transcript');
});

test('/model opens a picker of engine-supplied models; selection sends set_model', async () => {
  const { engine, lastFrame, stdin } = renderApp();
  await tick();
  engine.emit(
    snapshot({
      session_id: 's1',
      provider: 'deepseek',
      model: 'v4',
      cwd: '/p',
      models: [
        { provider: 'deepseek', model: 'v4' },
        { provider: 'minimax', model: 'M3' },
        { provider: 'kimi', model: 'k2' },
      ],
    }),
  );
  await tick();
  stdin.write('/model');
  await tick();
  stdin.write(KEY.enter);
  await tick();
  const f = plain(lastFrame());
  assert.match(f, /Switch model/);
  assert.match(f, /deepseek\/v4/);
  assert.match(f, /minimax\/M3/);
  // Cursor starts on the active model (deepseek/v4); move down twice → kimi/k2.
  stdin.write(KEY.down);
  await tick();
  stdin.write(KEY.down);
  await tick();
  stdin.write(KEY.enter);
  await tick();
  // No effort levels on kimi/k2 → effort is null (matches the native picker,
  // which returns None for a model without declared levels).
  assert.deepEqual(engine.last(), { kind: 'set_model', data: { provider: 'kimi', model: 'k2', effort: null } });
  // Picker closed → composer back.
  assert.doesNotMatch(plain(lastFrame()), /Switch model/);
});

test('/model picker cycles a model effort with ←/→ and sends the picked level', async () => {
  const { engine, lastFrame, stdin } = renderApp();
  await tick();
  engine.emit(
    snapshot({
      session_id: 's1',
      provider: 'deepseek',
      model: 'v4',
      effort: 'low',
      models: [{ provider: 'deepseek', model: 'v4', context: 128000, effort_levels: ['low', 'high', 'max'] }],
    }),
  );
  await tick();
  stdin.write('/model');
  await tick();
  stdin.write(KEY.enter); // open the picker
  await tick();
  const f = plain(lastFrame());
  assert.match(f, /effort:/, 'effort chips row shown for a model with levels');
  assert.match(f, /deepseek\/v4 ◆/, 'effort-capable model marked with ◆');
  // Active effort is 'low' (idx 0). →→ moves to 'max' (idx 2), apply.
  stdin.write(KEY.right);
  await tick();
  stdin.write(KEY.right);
  await tick();
  stdin.write(KEY.enter);
  await tick();
  assert.deepEqual(engine.last(), {
    kind: 'set_model',
    data: { provider: 'deepseek', model: 'v4', effort: 'max' },
  });
});

test('/skills picker shows enabled state and space toggles via toggle_skill', async () => {
  const { engine, lastFrame, stdin } = renderApp();
  await tick();
  engine.emit(
    snapshot({
      session_id: 's1',
      skills: [
        { name: 'git', enabled: true },
        { name: 'web', enabled: false },
      ],
    }),
  );
  await tick();
  stdin.write('/skills');
  await tick();
  stdin.write(KEY.enter);
  await tick();
  const f = plain(lastFrame());
  assert.match(f, /\[x\] git/);
  assert.match(f, /\[ \] web/);
  // Space on the first row (git) → toggle_skill.
  stdin.write(KEY.space);
  await tick();
  assert.deepEqual(engine.last(), { kind: 'toggle_skill', data: { name: 'git' } });
  // Engine re-snapshots with git now disabled → checkbox flips.
  engine.emit(snapshot({ session_id: 's1', skills: [{ name: 'git', enabled: false }, { name: 'web', enabled: false }] }));
  await tick();
  assert.match(plain(lastFrame()), /\[ \] git/);
});

test('/afk picker switches permission mode and the footer shows a badge', async () => {
  const { engine, lastFrame, stdin } = renderApp();
  await tick();
  engine.emit(snapshot({ session_id: 's1', mode: 'off' }));
  await tick();
  assert.doesNotMatch(plain(lastFrame()), /HANDS-FREE|AFK/, 'no badge while off');
  stdin.write('/afk');
  await tick();
  stdin.write(KEY.enter);
  await tick();
  assert.match(plain(lastFrame()), /Permission mode/);
  stdin.write(KEY.down); // off → hands_free
  await tick();
  stdin.write(KEY.enter);
  await tick();
  assert.deepEqual(engine.last(), { kind: 'set_mode', data: { mode: 'hands_free' } });
  // Engine confirms via a re-snapshot → footer badge appears.
  engine.emit(snapshot({ session_id: 's1', mode: 'hands_free' }));
  await tick();
  assert.match(plain(lastFrame()), /HANDS-FREE/);
});

test('/model picker cancels on Esc without sending a command', async () => {
  const { engine, lastFrame, stdin } = renderApp();
  await tick();
  engine.emit(snapshot({ session_id: 's1', models: [{ provider: 'a', model: 'b' }] }));
  await tick();
  stdin.write('/model');
  await tick();
  stdin.write(KEY.enter);
  await tick();
  assert.match(plain(lastFrame()), /Switch model/);
  stdin.write(KEY.esc);
  await tick();
  assert.doesNotMatch(plain(lastFrame()), /Switch model/);
  assert.equal(engine.sent.filter((c) => c.kind === 'set_model').length, 0);
});

test('a non-handled slash (/compact) and plain text both submit', async () => {
  const { engine, stdin } = renderApp();
  await tick();
  stdin.write('/compact');
  await tick();
  stdin.write(KEY.enter);
  await tick();
  assert.deepEqual(engine.last(), { kind: 'submit', data: { text: '/compact' } });
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
  assert.match(f, /1234 tok \(\d+%\)/, 'tokens with a context-fill %');
});

test('footer context gauge shows % of the active model window', async () => {
  const { engine, lastFrame } = renderApp();
  await tick();
  engine.emit(
    snapshot({
      session_id: 's1',
      provider: 'p',
      model: 'm',
      cwd: '/x',
      models: [{ provider: 'p', model: 'm', context: 1000 }],
    }),
  );
  engine.emit(ev('usage', { input_tokens: 250 }));
  await tick();
  assert.match(plain(lastFrame()), /250 tok \(25%\)/, '250 / 1000 window = 25%');
});

test('snapshot hydrates a pending request (handover) → picker shown', async () => {
  const { engine, lastFrame } = renderApp();
  await tick();
  engine.emit(snapshot({ session_id: 's1', pending_request: { id: 9, questions: [askOptions(false)] } }));
  await tick();
  assert.match(plain(lastFrame()), /Pick a color/);
});
