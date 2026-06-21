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
