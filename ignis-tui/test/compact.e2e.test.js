// e2e for the compaction indicator: a `compact_start` event shows a dedicated
// "Compacting…" spinner that takes priority over the generic RunningBar, and
// `compact_end` hides it. Covers both the manual /compact path (status is
// 'busy') and the auto-compact path (status is still 'idle' because
// compact_start fires before turn_start).
import test from 'node:test';
import assert from 'node:assert/strict';
import { renderApp, plain, tick, ev } from './harness.js';

test('compact_start shows the Compacting indicator while idle', async () => {
  const { engine, lastFrame } = renderApp();
  await tick();
  // Auto-compact fires before turn_start, so status is still 'idle'.
  engine.emit(ev('compact_start'));
  await tick();
  const f = plain(lastFrame());
  assert.match(f, /Compacting context/);
  // The generic "Working…" bar must NOT appear — compaction is more specific.
  assert.doesNotMatch(f, /Working/);
});

test('compact_end hides the Compacting indicator', async () => {
  const { engine, lastFrame } = renderApp();
  await tick();
  engine.emit(ev('compact_start'));
  await tick();
  assert.match(plain(lastFrame()), /Compacting context/);
  engine.emit(ev('compact_end'));
  await tick();
  const f = plain(lastFrame());
  assert.doesNotMatch(f, /Compacting/);
});

test('compacting takes priority over the busy RunningBar', async () => {
  const { engine, lastFrame } = renderApp();
  await tick();
  // Manual /compact: turn_start sets status busy, then compact_start fires.
  engine.emit(ev('turn_start'));
  await tick();
  assert.match(plain(lastFrame()), /Working/);
  engine.emit(ev('compact_start'));
  await tick();
  const f = plain(lastFrame());
  assert.match(f, /Compacting context/);
  assert.doesNotMatch(f, /Working/);
  // After compact_end the generic bar returns (the notice message streams).
  engine.emit(ev('compact_end'));
  await tick();
  assert.match(plain(lastFrame()), /Working/);
});

test('transcript replace resets compacting', async () => {
  const { engine, lastFrame } = renderApp();
  await tick();
  engine.emit(ev('compact_start'));
  await tick();
  assert.match(plain(lastFrame()), /Compacting context/);
  // A resume/clear replaces the transcript and must clear the indicator.
  engine.emit({ kind: 'transcript', data: { session_id: 's1', blocks: [] } });
  await tick();
  assert.doesNotMatch(plain(lastFrame()), /Compacting/);
});

test('turn_end resets a stuck compacting flag (cancel during auto-compact)', async () => {
  // If the user presses Ctrl+C during auto-compact, tokio::select! drops the
  // compact() future so CompactEnd never fires — only TurnEnd arrives. The
  // reducer must clear compacting on turn_end so the spinner can't stick.
  const { engine, lastFrame } = renderApp();
  await tick();
  engine.emit(ev('compact_start'));
  await tick();
  assert.match(plain(lastFrame()), /Compacting context/);
  engine.emit(ev('turn_end'));
  await tick();
  assert.doesNotMatch(plain(lastFrame()), /Compacting/);
});

test('compact_report renders the token reduction and full summary (manual /compact)', async () => {
  const { engine, lastFrame } = renderApp();
  await tick();
  // Manual /compact path: turn_start → compact_start → compact_end → report.
  engine.emit(ev('turn_start'));
  await tick();
  engine.emit(ev('compact_start'));
  await tick();
  engine.emit(ev('compact_end'));
  await tick();
  engine.emit(
    ev('compact_report', {
      before: 42318,
      after: 8104,
      summary: 'Built a compaction indicator. Touched app.js and protocol.js.',
    }),
  );
  await tick();
  engine.emit(ev('turn_end'));
  await tick();
  const f = plain(lastFrame());
  assert.match(f, /Compacted context/);
  assert.match(f, /42k → 8\.1k tokens/);
  assert.match(f, /−81%/);
  assert.match(f, /Built a compaction indicator/);
});

test('compact_report renders on the auto-compact path (idle status)', async () => {
  // Auto-compact fires before turn_start, so status is 'idle' when the report
  // lands — the block is a committed transcript item, independent of status.
  const { engine, lastFrame } = renderApp();
  await tick();
  engine.emit(ev('compact_start'));
  await tick();
  engine.emit(ev('compact_end'));
  await tick();
  engine.emit(
    ev('compact_report', { before: 50000, after: 10000, summary: 'Summary of earlier work.' }),
  );
  await tick();
  const f = plain(lastFrame());
  assert.match(f, /Compacted context/);
  assert.match(f, /50k → 10k tokens/);
  assert.match(f, /−80%/);
  assert.match(f, /Summary of earlier work/);
  // The spinner is gone (compact_end fired); only the committed block remains.
  assert.doesNotMatch(f, /Compacting context/);
});

test('compact_report clears the previous history from the render zone', async () => {
  const { engine, lastFrame } = renderApp();
  await tick();
  // Seed some conversation history.
  engine.emit(ev('user_prompt_committed', { text: 'what is two plus two' }));
  await tick();
  engine.emit(ev('message_end', { message: { role: 'assistant', content: 'the answer is four' } }));
  await tick();
  let f = plain(lastFrame());
  assert.match(f, /what is two plus two/);
  assert.match(f, /the answer is four/);
  // Compaction fires — old history must be wiped, only the report remains.
  engine.emit(ev('compact_start'));
  await tick();
  engine.emit(ev('compact_end'));
  await tick();
  engine.emit(ev('compact_report', { before: 42318, after: 8104, summary: 'Discussed arithmetic.' }));
  await tick();
  f = plain(lastFrame());
  assert.match(f, /Compacted context/);
  assert.match(f, /Discussed arithmetic/);
  // NOTE: ink-testing-library accumulates <Static> output and does not
  // interpret the \x1b[2J screen-wipe, so old blocks remain in the test
  // frame. In production the generation bump (verified in protocol.test.js)
  // wipes scrollback and remounts <Static> with only the compaction block.
  // The reducer test asserts blocks.length === 1 + generation bump.
});
