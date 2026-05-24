---
name: dogfood
description: Use when verifying an ignis change by running it like a real user before shipping — especially TUI/visual changes, where you must SEE the rendered output. The /ship step 3 routes here. Triggers include "dogfood", "does it actually work", "see the TUI", "screenshot the diff/picker".
---

# Dogfood (ignis)

Run the change the way a user would and confirm it actually works — before it ships.

**Core honesty rule:** tool output is plain text. You cannot perceive terminal
colors or layout from it. For a visual change, do **not** claim it looks right
from a unit test or an ANSI byte dump — render the real TUI to a PNG and *look at
it* (`Read` views images). Aesthetic judgment still belongs to the human; your
job is to make the artifact and surface it.

Pick the mode that fits the change. Build the binary first: `cargo build --release`.

## Mode A — Behavioral (CLI / state, no visuals)

For logic that has an observable result without color/layout. Run the real binary
one-shot against a configured provider and check the result or the files it wrote.

```bash
# Example: /model persistence — does a written state route the next run?
printf '{"model":"deepseek/deepseek-v4-pro"}\n' > ~/.ignis/state.json
./target/release/ignis "what is 6*7? reply with only the number"   # → Provider: deepseek/deepseek-v4-pro
rm -f ~/.ignis/state.json
```

Report what you exercised and the actual output. Needs a working provider in
`~/.ignis/config.toml` (network, spends tokens).

## Mode B — Visual (TUI) — capture a real screenshot

Use `tui_shot.py` (in this skill dir). It launches the **real** binary in a pty,
replays timed input, captures the raw terminal bytes, reconstructs the screen
with `pyte`, and renders it with Pillow — exact per-cell colors. Then `Read` the
PNG and actually look.

```bash
pip install -q pyte pillow    # one-time; DejaVuSansMono ships on Linux

# Drive the /model picker (no network): open it, move down, cycle effort
python3 .claude/skills/dogfood/tui_shot.py \
  --bin target/release/ignis --cwd /tmp/scratch --out /tmp/shot.png \
  --step 'wait:1.5' --step $'type:/model\r' --step 'wait:0.6' \
  --step 'key:down' --step 'key:right' --step 'wait:0.6'

# Exercise an edit_file diff via a real model edit (needs a provider + a scratch file)
python3 .claude/skills/dogfood/tui_shot.py \
  --bin target/release/ignis --cwd /tmp/scratch --out /tmp/shot.png \
  --step 'wait:1.5' \
  --step $'type:use edit_file to replace "hello world" with "hi" in greet.rs\r' \
  --step 'wait:25'
```

Then **`Read /tmp/shot.png`** — verify the actual colors, backgrounds, alignment,
highlighting. If it's a taste call (palette, spacing), show the PNG path to the
human and ask.

Steps: `wait:<sec>` · `type:<text>` (backslash escapes honored) · `key:<name>`
(`up|down|left|right|enter|esc|tab|backspace|ctrl-d|pageup|pagedown`).

## Notes

- The screenshot reconstructs the **real binary's** output (a crash or stray log
  shows up too — that's a feature: it surfaces real breakage).
- `TestBackend` cell-style assertions (inspect `cell.fg`/`cell.bg` in a unit test)
  are a good *automated* complement — they prove the mechanism (e.g. a `+` row has
  the diff background) but not that it looks good. Use both: a test for the
  mechanism, a screenshot for the look.
- A scratch dir + tiny source file lets the model perform a deterministic edit
  without touching the repo.
