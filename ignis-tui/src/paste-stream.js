import { Readable } from 'node:stream';
import { StringDecoder } from 'node:string_decoder';

// Bracketed-paste markers a terminal wraps a paste in once ESC[?2004h is set.
const START = '\x1b[200~';
const END = '\x1b[201~';

/**
 * Wrap a TTY input stream so a bracketed-paste span (ESC[200~ … ESC[201~) is
 * coalesced into a SINGLE chunk before Ink's per-chunk keypress parser sees it.
 *
 * Without this, a multi-line paste reaches the composer either as carriage
 * returns (each one looks like Enter and submits a line) or as fragments split
 * across reads — so the `[paste #N]` chip never forms. This mirrors what
 * crossterm's `Event::Paste` does for the native TUI: hand the UI the whole
 * paste at once, newlines intact.
 *
 * The terminal only emits the markers after the app enables bracketed-paste
 * mode (ESC[?2004h — see App's mount effect). Until then there are no markers,
 * so every byte passes straight through and ordinary typing is unaffected.
 *
 * Ink treats whatever stream it's given exactly like `process.stdin` — it reads
 * `isTTY`, toggles raw mode, refs/unrefs and pulls chunks via `read()`/'readable'.
 * The returned Readable provides read()/'readable' itself and proxies the TTY
 * bits to the real source.
 */
export function createPasteStream(source = process.stdin) {
  const decoder = new StringDecoder('utf8');
  let pending = ''; // decoded bytes not yet classified (may end mid-marker)
  let inPaste = false;
  let paste = ''; // accumulated paste body while inside a span

  const shim = new Readable({ read() {} });
  const emit = (s) => {
    if (s) shim.push(s);
  };

  // Length of the longest suffix of `buf` that is a prefix of `marker`, so a
  // marker split across two reads isn't mis-emitted as keystrokes. A lone ESC
  // (a 1-char prefix) is never held back — that would swallow the Escape key —
  // so a marker must arrive with at least its `ESC[` intact in one read, which
  // terminals always do (the 6-byte marker is written atomically).
  const heldPrefix = (buf, marker) => {
    const max = Math.min(buf.length, marker.length - 1);
    for (let k = max; k >= 2; k--) {
      if (buf.slice(-k) === marker.slice(0, k)) return k;
    }
    return 0;
  };

  const drain = () => {
    for (;;) {
      if (!inPaste) {
        const i = pending.indexOf(START);
        if (i === -1) {
          const hold = heldPrefix(pending, START);
          emit(pending.slice(0, pending.length - hold));
          pending = hold ? pending.slice(-hold) : '';
          return;
        }
        emit(pending.slice(0, i)); // text before the paste passes through
        pending = pending.slice(i + START.length);
        inPaste = true;
        paste = '';
      } else {
        const j = pending.indexOf(END);
        if (j === -1) {
          const hold = heldPrefix(pending, END);
          paste += pending.slice(0, pending.length - hold);
          pending = hold ? pending.slice(-hold) : '';
          return;
        }
        paste += pending.slice(0, j);
        pending = pending.slice(j + END.length);
        inPaste = false;
        emit(paste.replace(/\r\n?/g, '\n')); // one chunk, CR/CRLF → LF
        paste = '';
      }
    }
  };

  source.on('data', (chunk) => {
    pending += decoder.write(chunk);
    drain();
  });

  shim.isTTY = source.isTTY;
  shim.setRawMode = (mode) => {
    source.setRawMode?.(mode);
    return shim;
  };
  shim.ref = () => {
    source.ref?.();
    return shim;
  };
  shim.unref = () => {
    source.unref?.();
    return shim;
  };

  return shim;
}
