# system-prompt-trim — SystemPromptCompose token-efficiency hook

Strips the `Git Diff:` block from the assembled system prompt before
each LLM call. Demonstrates the v2 `SystemPromptCompose` event and the
`updatedSystemPrompt` rewrite path.

The point isn't that you should always strip the diff — it's that the
hook gives you the lever to A/B test it on your codebase, your model,
your kind of task. The token-efficiency research substrate that
motivated v2.

## Install

```sh
mkdir -p ~/.ignis/hooks
cp -R examples/hooks/system-prompt-trim ~/.ignis/hooks/system-prompt-trim
chmod +x ~/.ignis/hooks/system-prompt-trim/run.sh
```

Requires `jq` + `awk` on `PATH`.

## Wire in `~/.ignis/hooks.json`

```json
{
  "hooks": {
    "SystemPromptCompose": [
      { "command": "~/.ignis/hooks/system-prompt-trim/run.sh" }
    ]
  }
}
```

`SystemPromptCompose` fires once per LLM call — the assembled prompt
goes in, the rewritten prompt goes out. No matcher (it's not a tool
event).

Reload without restarting: type `/hooks reload`.

## What it strips

ignis's default system prompt includes a `Git Diff:` block surrounded
by triple-backtick code fences (see `build_system_prompt` in
`agent/mod.rs`). This hook removes that whole block.

## Extending

Variations worth trying for cost experiments:

- Strip `Git Status:` too (typically smaller, but always present).
- Truncate the diff to the first N hunks instead of removing it.
- Compress `AGENTS.md` if your project's instructions are long.
- Inject `additionalContext` instead of rewriting — useful for adding
  per-turn hints without modifying the base prompt.

Each variation is a one-line change to this script. Measure tokens
before/after via `/usage` or the per-run telemetry.
