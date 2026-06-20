// `node --test` — drives createPasteStream with a fake source and captures the
// chunks it would hand Ink. Proves a bracketed-paste span is coalesced into ONE
// clean chunk (markers stripped, CR/CRLF → LF) regardless of how the terminal
// fragments it, while ordinary keystrokes pass straight through.
import test from 'node:test';
import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import { createPasteStream } from '../src/paste-stream.js';

const START = '\x1b[200~';
const END = '\x1b[201~';

// Wire up a shim over a fake source and capture every chunk it emits. push() is
// reassigned before any data flows, so capture is synchronous and per-chunk —
// the chunk boundaries are exactly what Ink's readable loop would see.
function harness() {
  const source = new EventEmitter();
  source.isTTY = true;
  const shim = createPasteStream(source);
  const chunks = [];
  shim.push = (s) => {
    if (s != null) chunks.push(s);
    return true;
  };
  return { feed: (...parts) => parts.forEach((p) => source.emit('data', Buffer.from(p))), chunks };
}

test('a CRLF paste arrives as one chunk with newlines normalized', () => {
  const { feed, chunks } = harness();
  feed(`${START}alpha\r\nbeta\r\ngamma${END}`);
  assert.deepEqual(chunks, ['alpha\nbeta\ngamma']);
});

test('a CR-only paste (the real-terminal failure case) is normalized to LF', () => {
  const { feed, chunks } = harness();
  feed(`${START}alpha\rbeta\rgamma${END}`);
  assert.deepEqual(chunks, ['alpha\nbeta\ngamma']);
});

test('paste body spanning multiple reads is coalesced into a single chunk', () => {
  const { feed, chunks } = harness();
  feed(`${START}alpha\nbe`, 'ta\ngamm', `a${END}`);
  assert.deepEqual(chunks, ['alpha\nbeta\ngamma'], 'one chip-worthy chunk, not three');
});

test('a marker split across reads (after ESC[) is still recognized', () => {
  const { feed, chunks } = harness();
  // START split as "\x1b[20" + "0~", END split as "\x1b[" + "201~"
  feed('\x1b[20', '0~alpha\nbeta\x1b[', '201~');
  assert.deepEqual(chunks, ['alpha\nbeta']);
});

test('text around a paste passes through and the paste stays its own chunk', () => {
  const { feed, chunks } = harness();
  feed(`hi ${START}x\ny${END} bye`);
  assert.deepEqual(chunks, ['hi ', 'x\ny', ' bye']);
});

test('ordinary typing passes straight through, byte for byte', () => {
  const { feed, chunks } = harness();
  feed('a', 'b', 'c');
  assert.deepEqual(chunks, ['a', 'b', 'c']);
});

test('a lone ESC keypress is never held back (Escape must still work)', () => {
  const { feed, chunks } = harness();
  feed('\x1b'); // a real Escape press: ESC alone, nothing follows
  assert.deepEqual(chunks, ['\x1b'], 'ESC delivered immediately, not swallowed');
});

test('an arrow-key sequence is not mistaken for a paste marker', () => {
  const { feed, chunks } = harness();
  feed('\x1b[A'); // up arrow
  assert.deepEqual(chunks, ['\x1b[A']);
});
