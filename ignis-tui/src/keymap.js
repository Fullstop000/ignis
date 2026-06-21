// Single source of truth for ignis-tui keyboard shortcuts.
//
// Each shortcut is pure data: a `match(key, ch)` predicate that recognises the
// chord, plus a `run(ctx)` action. This module owns the *bindings* (which chord
// triggers which action); `app.js` owns the stateful action *implementations*
// and passes them in via `ctx`. Keeping the bindings here means every shortcut
// is declared in one place — add or rebind a chord by editing this table alone.
// `help` powers keybinding docs / a future /help screen.
//
// Scope: chorded shortcuts only (every Ctrl+* plus the Ctrl+J newline). Plain
// editor mechanics — Enter/submit, arrows, Backspace, history ↑/↓, slash-list
// and follow-up navigation, printable input — stay in app.js's input handler;
// they branch on local editor state and are not bindable chords.
//
// Every entry is self-gating and always consumes its chord (returns true), which
// preserves app.js's prior "return after handling" semantics: an idle Ctrl+S, for
// example, matches and runs a no-op rather than falling through to type an 's'.

export const SHORTCUTS = [
  {
    id: 'cancel-or-exit',
    help: 'Ctrl+C  cancel turn · idle: hint to exit',
    match: (key, ch) => key.ctrl && ch === 'c',
    run: (ctx) => ctx.cancelOrHint(),
  },
  {
    id: 'exit',
    help: 'Ctrl+D  exit (press twice)',
    match: (key, ch) => key.ctrl && ch === 'd',
    run: (ctx) => ctx.exitArm(),
  },
  {
    id: 'inject',
    help: 'Ctrl+S  send queued text to the running turn',
    match: (key, ch) => key.ctrl && ch === 's',
    run: (ctx) => ctx.inject(),
  },
  {
    id: 'reasoning',
    help: 'Ctrl+O  expand/collapse reasoning',
    match: (key, ch) => key.ctrl && ch === 'o',
    run: (ctx) => ctx.toggleReasoning(),
  },
  {
    id: 'line-start',
    help: 'Ctrl+A  cursor to line start',
    match: (key, ch) => key.ctrl && ch === 'a',
    run: (ctx) => ctx.lineStart(),
  },
  {
    id: 'line-end',
    help: 'Ctrl+E  cursor to line end',
    match: (key, ch) => key.ctrl && ch === 'e',
    run: (ctx) => ctx.lineEnd(),
  },
  {
    id: 'kill-line',
    help: 'Ctrl+U  clear the composer',
    match: (key, ch) => key.ctrl && ch === 'u',
    run: (ctx) => ctx.killLine(),
  },
  {
    id: 'kill-word',
    help: 'Ctrl+W  delete the word before the cursor',
    match: (key, ch) => key.ctrl && ch === 'w',
    run: (ctx) => ctx.killWord(),
  },
  {
    // Ink delivers Ctrl+J (and a literal LF) as a lone '\n' with key.return
    // false — Enter is '\r'. Matched here so a single newline is never mistaken
    // for a multi-line paste downstream.
    id: 'newline',
    help: 'Ctrl+J  insert a newline',
    match: (key, ch) => ch === '\n',
    run: (ctx) => ctx.newline(),
  },
];

// Run the first shortcut whose chord matches. Returns true if one consumed the
// key (the caller should then stop processing it), false if no chord matched.
export function dispatchShortcut(ch, key, ctx) {
  for (const s of SHORTCUTS) {
    if (s.match(key, ch)) {
      s.run(ctx);
      return true;
    }
  }
  return false;
}
