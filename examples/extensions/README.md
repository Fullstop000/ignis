# ignis extensions — examples

Extensions let an external program subscribe to ignis lifecycle events and,
where the event permits it, **rewrite** the data flowing through or
**inject context** the model sees on the next turn. This folder
collects working extension scripts you can copy, install in
`~/.ignis/extensions/`, and reference from `~/.ignis/extensions.json`.

## Catalog

| Hook | Event(s) | What it does |
|---|---|---|
| [`translate-en/`](./translate-en/) | `UserPromptSubmit` + `AssistantMessageRender` | Bilingual prompt + reply translator (Anthropic Haiku). The original ignis use case. |
| [`bash-deny-rm-rf/`](./bash-deny-rm-rf/) | `PreToolUse` (`matcher: "Bash"`) | Refuses `rm -rf` (and variants) before the bash tool runs. Demonstrates `decision: "block"`. |
| [`auto-test/`](./auto-test/) | `PostToolUse` (`matcher: "Write\|Edit"`) | Runs `cargo test --workspace -q` after every Write/Edit and injects PASS/FAIL into the next turn via `additionalContext`. |
| [`system-prompt-trim/`](./system-prompt-trim/) | `SystemPromptCompose` | Strips the `Git Diff:` block from the assembled system prompt per LLM call. The token-efficiency research substrate; A/B test prompt density on your project. |

## Events available (v2)

| Event | When it fires | Output verbs |
|---|---|---|
| `UserPromptSubmit` | User hits enter, before model send | `updatedInput`, `decision:"block"` |
| `AssistantMessageRender` | Before TUI renders model's reply | `updatedOutput` (block degraded to warning) |
| `SystemPromptCompose` | Before each LLM call, after prompt assembly | `updatedSystemPrompt`, `additionalContext` |
| `PreToolUse` | Before a tool runs | `updatedInput` (object), `decision:"block"`, `additionalContext` |
| `PostToolUse` | After a tool finishes | `additionalContext`, `decision:"block"` (frames result as error) |
| `PreCompact` | Before context compaction | `decision:"block"` (aborts compact), `additionalContext` |
| `PostCompact` | After compaction; sees summary text | `additionalContext` |
| `SessionStart` | Once at session open | `additionalContext` |
| `Stop` | On clean turn exit | `decision:"block"` (keeps loop alive — inverted), `additionalContext` |

## Writing your own

A hook is **any executable** ignis can `spawn`. The protocol is one
JSON envelope in on stdin, one JSON envelope out on stdout. Exit code
governs blocking semantics. Language-agnostic — Python, shell, Go,
Rust, whatever you like.

### Envelope — stdin

Base fields on every event:

```json
{
  "hook_event_name": "PreToolUse",
  "session_id": "session-…",
  "cwd": "/home/you/project",
  "triggered_at": "2026-06-07T13:00:00Z"
}
```

Per-event extras (only the fields its event populates are present):

- `UserPromptSubmit`: `prompt`
- `AssistantMessageRender`: `content`
- `SystemPromptCompose`: `system_prompt`, `model`
- `PreToolUse`: `tool_name`, `tool_input`
- `PostToolUse`: `tool_name`, `tool_input`, `tool_response`
- `PreCompact`: `trigger` (`"auto"` / `"manual"`), `transcript_path`
- `PostCompact`: `trigger`, `summary`
- `SessionStart`: `source` (`"new"` / `"resume"`)
- `Stop`: `transcript_path`

### Response — stdout (all fields optional)

```json
{
  "continue": true,
  "systemMessage": "optional one-liner displayed in TUI",
  "suppressOutput": false,
  "decision": "block",
  "reason": "structured block reason for the model",
  "stopReason": "displayed in TUI when continue:false",
  "hookSpecificOutput": {
    "hookEventName": "PreToolUse",
    "updatedInput": { "command": "echo safe" },
    "updatedOutput": "rewritten assistant text",
    "updatedSystemPrompt": "rewritten system prompt",
    "additionalContext": "appears as <system-reminder> on next turn"
  }
}
```

Each `updated*` field belongs to specific events (see the table above);
unrelated fields are ignored.

### Exit codes

| Code | Meaning |
|---|---|
| `0` | OK. Parse stdout if non-empty; pass-through otherwise. |
| `2` | Block the chain. Honoured per event (see Output verbs above). |
| anything else | Soft failure — original payload kept; `[warn]` line in scrollback. |

### Matcher (PreToolUse / PostToolUse)

Tool events accept a `matcher` regex on `tool_name`. Hooks with a
matcher only fire when the running tool's name matches, so unrelated
calls don't pay the spawn cost:

```json
{
  "command": "~/.ignis/extensions/bash-deny/run.sh",
  "matcher": "Bash"
}
```

`matcher` is compiled at parse — a bad regex is a startup error.
Declaring `matcher` on a non-tool event triggers a `[warn]` at load and
the field is ignored.

### Declare it

`~/.ignis/extensions.json`:

```json
{
  "extensions": {
    "UserPromptSubmit": [
      {"command": "~/.ignis/extensions/your-hook/run.py"}
    ],
    "PreToolUse": [
      {"command": "~/.ignis/extensions/bash-judge/run.sh", "matcher": "Bash"}
    ]
  }
}
```

`command` is split on whitespace at parse time — **no shell** is
involved, no `$VAR` expansion, no globbing. Only the leading `~/` is
expanded. For program paths with spaces, use the explicit `argv`
form. Each entry may set `timeout_ms` (default 10000).

Type `/extensions reload` after editing the file to pick up changes without
restarting ignis.

### `additionalContext` — injecting reminders

Hooks can return `additionalContext` instead of (or alongside) a
rewrite. The text is queued and prepended to the model's next turn as
a `<system-reminder>` block labelled with the hook's display name and
event class. Useful for "I ran the tests for you, here's what
happened" patterns without modifying the actual tool result.

See `auto-test/` and `system-prompt-trim/` for working examples.

See [`docs/usage/extensions.md`](../../docs/usage/extensions.md) for the
full user-facing reference.
