// Regression for the Ink picker resize bug: picker boxes used to shrink-wrap
// their content, so a long question or a wide terminal could make the border
// overflow the visible width or leave stale border characters behind on resize.
// The fix pins every picker Box to the current terminal width and listens for
// the `resize` event so the width updates.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import React from 'react';
import { render } from 'ink';
import { EventEmitter } from 'node:events';
import App from '../src/app.js';
import { mockEngine, request } from './harness.js';

function fakeStdout(cols) {
  const s = new EventEmitter();
  s.writes = [];
  s.columns = cols;
  s.rows = 24;
  s.write = (x) => { s.writes.push(x); return true; };
  s.setRawMode = () => s;
  s.setEncoding = () => s;
  s.resume = () => s;
  s.pause = () => s;
  s.read = () => null;
  s.ref = () => s;
  s.unref = () => s;
  s.isTTY = true;
  return s;
}

function fakeStdin() {
  const s = new EventEmitter();
  s.isTTY = true;
  s.setRawMode = () => s;
  s.setEncoding = () => s;
  s.resume = () => s;
  s.pause = () => s;
  s.read = () => null;
  s.ref = () => s;
  s.unref = () => s;
  return s;
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function visibleWidth(s) {
  // Strip ANSI color/style codes and count visible columns (basic CJK-wide).
  const stripped = s.replace(/\x1b\[[0-9;]*m/g, '');
  let w = 0;
  for (const ch of stripped) {
    const code = ch.codePointAt(0);
    if (
      code >= 0x1100 &&
      (code <= 0x115f ||
        code === 0x2329 ||
        code === 0x232a ||
        (code >= 0x2e80 && code <= 0x9fff) ||
        (code >= 0xac00 && code <= 0xd7af) ||
        (code >= 0xf900 && code <= 0xfaff) ||
        (code >= 0xfe30 && code <= 0xfe6f))
    ) {
      w += 2;
    } else {
      w += 1;
    }
  }
  return w;
}

function maxLineWidth(frame) {
  const normalized = frame
    .replace(/\x1b\[[0-9;]*[A-Za-z]/g, '')
    .replace(/\r\n/g, '\n')
    .replace(/\r/g, '\n');
  let max = 0;
  for (const line of normalized.split('\n')) {
    max = Math.max(max, visibleWidth(line));
  }
  return max;
}

const askUserRequest = request(1, [
  {
    question: 'How wide should the chat history left padding be?',
    kind: 'ask_user',
    header: 'Task 1 pad',
    multi_select: false,
    allow_other: true,
    options: [
      { label: '2 columns (Recommended)' },
      { label: '4 columns' },
      { label: 'Other' },
    ],
  },
]);

test('ask_user picker border stays within a narrow terminal width', async () => {
  const engine = mockEngine();
  const stdout = fakeStdout(40);
  const stdin = fakeStdin();
  const inst = render(React.createElement(App, { engine }), { stdout, stdin, patchConsole: false });

  await sleep(50);
  engine.emit(askUserRequest);
  await sleep(50);

  const frame = stdout.writes.join('');
  assert.ok(
    maxLineWidth(frame) <= 40,
    `picker frame exceeds 40 cols: ${maxLineWidth(frame)}`,
  );

  inst.unmount();
});

test('ask_user picker border stays within a wide terminal width', async () => {
  const engine = mockEngine();
  const stdout = fakeStdout(120);
  const stdin = fakeStdin();
  const inst = render(React.createElement(App, { engine }), { stdout, stdin, patchConsole: false });

  await sleep(50);
  engine.emit(askUserRequest);
  await sleep(50);

  const frame = stdout.writes.join('');
  assert.ok(
    maxLineWidth(frame) <= 120,
    `picker frame exceeds 120 cols: ${maxLineWidth(frame)}`,
  );

  inst.unmount();
});
