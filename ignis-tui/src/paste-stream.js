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
 * mode (ESC[?2004h — see cli.js). Until then there are no markers, so every
 * byte passes straight through and ordinary typing is unaffected.
 *
 * Ink treats whatever stream it's given exactly like `process.stdin` — it reads
 * `isTTY`, toggles raw mode, refs/unrefs and pulls chunks via `read()`/'readable'.
 * The returned Readable provides read()/'readable' itself and proxies the TTY
 * bits to the real source.
 *
 * `escTimeoutMs` disambiguates a trailing ESC: a marker can be split across
 * reads right after its ESC byte, so a lone trailing ESC is held briefly in
 * case the rest of a marker follows. If nothing completes a marker before the
 * timer fires, the held bytes are flushed as ordinary input — so a real Escape
 * press isn't swallowed. `setTimer`/`clearTimer` are injectable for tests.
 */
export function createPasteStream(
  source = process.stdin,
  { escTimeoutMs = 50, setTimer = setTimeout, clearTimer = clearTimeout } = {},
) {
  const decoder = new StringDecoder('utf8');
  let pending = ''; // decoded bytes not yet classified (may end mid-marker)
  let inPaste = false;
  let paste = ''; // accumulated paste body while inside a span
  let timer = null;

  const shim = new Readable({ read() {} });
  const emit = (s) => {
    if (s) shim.push(s);
  };

  // Length of the longest suffix of `buf` that is a prefix of `marker`, so a
  // marker split across reads isn't mis-emitted as keystrokes. Includes a lone
  // ESC (length 1); the escape-timer below releases it if no marker follows.
  const heldPrefix = (buf, marker) => {
    const max = Math.min(buf.length, marker.length - 1);
    for (let k = max; k >= 1; k--) {
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

  const clearFlush = () => {
    if (timer) {
      clearTimer(timer);
      timer = null;
    }
  };

  source.on('data', (chunk) => {
    clearFlush();
    pending += decoder.write(chunk);
    drain();
    // Outside a paste, a leftover `pending` is a possible-marker prefix (maybe
    // just an ESC). Release it as input if no completing bytes arrive in time.
    if (!inPaste && pending) {
      timer = setTimer(() => {
        timer = null;
        emit(pending);
        pending = '';
      }, escTimeoutMs);
      if (typeof timer?.unref === 'function') timer.unref();
    }
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
