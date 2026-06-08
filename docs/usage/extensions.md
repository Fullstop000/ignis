# Extensions

Extensions let an external program subscribe to ignis lifecycle events.
Where the event permits it they can **rewrite** the data flowing
through, **block** the action, or **inject context** the model sees on
the next turn. (Formerly "hooks" — v1 configs at `~/.ignis/hooks.json`
still load as a back-compat fallback, and the slash command `/hooks` is
kept as a deprecated alias.)

> ## ⚠ Extensions run unsandboxed
>
> Each extension command runs with the full privileges of your `ignis`
> process. A malicious or buggy hook can:
>
> - Read `~/.ssh`, `~/.aws/credentials`, `~/.config/gh/`, `.netrc`,
>   any project source.
> - See and exfiltrate every env var ignis was started with —
>   including `ANTHROPIC_API_KEY`.
> - Spawn child processes; write/delete arbitrary files; make
>   arbitrary network calls.
>
> **Treat `~/.ignis/extensions.json` like `crontab`** — anything in there
> has root-equivalent power over your user account. Only install hooks
> whose source you have personally audited.
>
> A separate PR adds env-var scrubbing and a Linux Landlock / macOS
> Seatbelt sandbox; until it merges, the protocol relies on you reading
> the script.

## Events

All nine events ignis fires, in the rough order they appear during a
session:

| Event | When | Block | Rewrite | Inject context |
|---|---|---|---|---|
| `SessionStart` | Once at session open | — | — | ✓ `additionalContext` |
| `UserPromptSubmit` | Before user message reaches model | ✓ | ✓ `updatedInput` (string) | — |
| `SystemPromptCompose` | Before each LLM call, after prompt assembly | (degraded to warning) | ✓ `updatedSystemPrompt` | ✓ |
| `PreToolUse` | Before a tool runs | ✓ | ✓ `updatedInput` (object) | ✓ |
| `PostToolUse` | After tool succeeds / fails | ✓ (frames as tool error) | — | ✓ |
| `AssistantMessageRender` | Before TUI renders model reply | (degraded to warning) | ✓ `updatedOutput` | — |
| `PreCompact` | Before context compaction | ✓ (aborts compact) | — | ✓ |
| `PostCompact` | After compaction; sees summary | — | — | ✓ |
| `Stop` | On clean turn exit | **inverted** — keeps loop alive | — | ✓ |

### Per-event detail

- **`SessionStart`** — fires once. Envelope has `source: "new"` /
  `"resume"`. Useful for prepending project-wide instructions ("user is
  bilingual, default to Chinese in replies") to the model's first turn.
- **`UserPromptSubmit`** — the v1 event. Fires before the user message
  is pushed to history. Hooks chain; each receives the previous hook's
  output. The final string is what the model sees and history stores.
  `decision: "block"` rejects the turn (the only event where blocking
  is meaningful for input).
- **`SystemPromptCompose`** — fires before **every** LLM call (not
  just session start), because the assembled prompt changes per turn
  (git status, git diff). Envelope has `system_prompt` and `model`.
  Hooks chain — the threaded `updatedSystemPrompt` is used for THIS
  call only; the base prompt isn't touched for next call. Use for
  token-efficiency experiments (strip the diff block, compress
  AGENTS.md, etc.). `decision: "block"` is degraded to a warning — the
  LLM call still needs some prompt.
- **`PreToolUse`** — fires before each tool call. Envelope has
  `tool_name` and `tool_input` (JSON object). `updatedInput` rewrites
  `tool_input` (must also be a JSON object); `decision: "block"`
  refuses the call and the reason flows to the model as a tool error.
  Use `matcher` to scope to specific tools.
- **`PostToolUse`** — fires after the tool finishes (success or
  failure). Envelope has `tool_name`, `tool_input`, `tool_response`
  (`{success, content}`). `additionalContext` is queued for the next
  LLM call as a `<system-reminder>` block — the "I ran tests for you,
  here's what happened" channel. `decision: "block"` reframes the tool
  result as an error with the reason appended.
- **`AssistantMessageRender`** — v1 event. Fires before the TUI
  commits the model's text. **History stores the original output**,
  not the rewrite — so prompt cache and replay stay exact. The
  rewrite appears as a `[hook rewrite]` block below the original in
  scrollback.
- **`PreCompact`** — fires before context compaction. Envelope has
  `trigger: "auto"` (threshold-driven) or `"manual"` (slash command).
  `decision: "block"` aborts the compact entirely.
- **`PostCompact`** — fires after compaction succeeds, with the
  generated `summary` in the envelope. Only `additionalContext`
  matters (the summary is already final).
- **`Stop`** — fires on the clean-exit branch of the agent loop (NOT
  on fatal errors). **The CC inversion applies:** `decision: "block"`
  means "keep looping" — the loop reads the reason as a system
  reminder and continues. The pattern: a stop-condition hook that
  says "your test suite is still failing, don't stop yet."

## Envelope

### stdin — JSON object

Base fields on every event:

```json
{
  "hook_event_name": "PreToolUse",
  "session_id": "session-…",
  "cwd": "/home/you/project",
  "triggered_at": "2026-06-07T13:00:00Z"
}
```

Per-event additions (only the fields its event populates are present):

| Event | Extra fields |
|---|---|
| `UserPromptSubmit` | `prompt` |
| `AssistantMessageRender` | `content` |
| `SystemPromptCompose` | `system_prompt`, `model` |
| `PreToolUse` | `tool_name`, `tool_input` |
| `PostToolUse` | `tool_name`, `tool_input`, `tool_response` |
| `PreCompact` | `trigger`, `transcript_path` |
| `PostCompact` | `trigger`, `summary` |
| `SessionStart` | `source` |
| `Stop` | `transcript_path` |

### stdout — JSON object (all fields optional)

```json
{
  "continue": true,
  "systemMessage": "Optional 1-line note shown in TUI",
  "suppressOutput": false,
  "decision": "block",
  "reason": "structured block reason — surfaced to model as system reminder",
  "stopReason": "shown in TUI when continue:false",
  "hookSpecificOutput": {
    "hookEventName": "PreToolUse",
    "updatedInput": { "command": "echo safe" },
    "updatedOutput": "rewritten assistant text",
    "updatedSystemPrompt": "rewritten system prompt",
    "additionalContext": "appears as <system-reminder> on next turn"
  }
}
```

Each `updated*` field is honored only for the event(s) listed in the
table above; unrelated fields on the wrong event are ignored with a
debug log.

## Matcher (PreToolUse / PostToolUse)

Tool events accept a `matcher` regex on `tool_name`:

```json
{ "command": "~/.ignis/extensions/bash-deny/run.sh", "matcher": "Bash" }
```

Hooks with a matcher only fire when the running tool's name matches —
unrelated calls don't pay the spawn cost. `matcher` is compiled at
parse, so a malformed regex is a startup error. Declaring `matcher`
on a non-tool event logs a `[warn]` at load and is otherwise ignored.

## `additionalContext` — injecting reminders

Hooks can return `additionalContext` instead of (or alongside) a
rewrite. The text is queued and, before the next LLM call, prepended
to history as a synthetic `<system-reminder>` block labelled with the
hook's display name and event class:

```
<system-reminder>
hook PostToolUse (auto-test): cargo test --workspace -q: PASSED
</system-reminder>
```

`PostToolUse`, `SessionStart`, `SystemPromptCompose`, `PreCompact`,
`PostCompact`, and `Stop` all support `additionalContext`. It is the
"talk to the model side-channel" — useful when you want to add
information without modifying the actual tool result, prompt, or
summary.

## Exit codes

| Code | Behaviour |
|---|---|
| `0` | OK. stdout is parsed; absent/empty stdout = pass-through. |
| `2` | Block the chain. Per-event semantics (see Block column above). |
| anything else | Soft failure: original payload kept; `[warn]` in scrollback. |

A hook that runs longer than its `timeout_ms` is killed (SIGKILL via
`kill_on_drop`) and treated as a soft failure.

## Declaration — `~/.ignis/extensions.json`

```json
{
  "extensions": {
    "UserPromptSubmit": [
      {
        "command": "~/.ignis/extensions/translate-en/run.py",
        "timeout_ms": 30000
      }
    ],
    "PreToolUse": [
      {
        "command": "~/.ignis/extensions/bash-deny-rm-rf/run.sh",
        "matcher": "Bash",
        "timeout_ms": 2000
      }
    ],
    "PostToolUse": [
      {
        "command": "~/.ignis/extensions/auto-test/run.sh",
        "matcher": "Write|Edit",
        "timeout_ms": 120000
      }
    ],
    "SystemPromptCompose": [
      { "command": "~/.ignis/extensions/system-prompt-trim/run.sh" }
    ]
  }
}
```

- `command` is **split on whitespace** at parse time and passed argv-
  style to `Command::new`. No shell is involved. No `$VAR` expansion.
  Only a leading `~/` is expanded (against the home dir).
- For program paths with whitespace, use `argv: [...]` instead:

  ```json
  { "argv": ["/Users/foo bar/run.py", "--display"], "timeout_ms": 30000 }
  ```

  `argv[0]` is the program; `command` and `argv` are mutually
  exclusive.
- `timeout_ms` defaults to `10000` (10 s).
- `matcher` is a regex on `tool_name`. Meaningful only for
  `PreToolUse` / `PostToolUse`; on other events it's logged at load.
- Each event takes a JSON array — multiple hooks chain left-to-right,
  each receiving the previous hook's output.
- The file is loaded at session start. Absent file = no hooks, no log
  noise. Malformed file = startup error.

### v1 → v2 back-compat

v2 reads v1 configs unchanged. Existing
`{"command": "...", "timeout_ms": N}` entries still parse with no
edits required. The `matcher` field is optional; absent matcher means
"every tool".

### Inspecting the active chains — `/extensions` (or `/extensions list`)

```
[info] 4 hooks registered · /extensions reload to re-read · run unsandboxed; audit before installing:
  UserPromptSubmit (1):
    · translate-en  ~/.ignis/extensions/translate-en/run.py  (timeout 30000ms)
  SystemPromptCompose (1):
    · run           ~/.ignis/extensions/system-prompt-trim/run.sh  (timeout 10000ms)
  PreToolUse (1):
    · run           ~/.ignis/extensions/bash-deny-rm-rf/run.sh  (timeout 2000ms)
  PostToolUse (1):
    · run           ~/.ignis/extensions/auto-test/run.sh  (timeout 120000ms)
```

The list reflects the in-memory state (last successful load or
`/extensions reload`), not a live disk probe.

### Hot-reload — `/extensions reload`

Type `/extensions reload` in the TUI after editing `extensions.json`. The parsed
config is swapped into the running registry; the next prompt picks it
up.

## Failure UI

Every soft failure commits a `[warn] <event>: <reason> (<hook-name>)`
line below the affected block. No rate-limiting — transparency over
visual cleanliness.

## Observability

Each hook invocation emits a `tracing` span named `ignis.hook` with
attributes `event`, `command`, `duration_ms`, `outcome`
(`mutated` / `mutated_json` / `inject_context` / `pass_through` /
`blocked` / `keep_looping` / `failed`). Enable
`IGNIS_ENABLE_TELEMETRY=1` to export via OpenTelemetry.

## Worked examples

- [`examples/extensions/translate-en/`](../../examples/extensions/translate-en/)
  — bilingual translator (the original ignis use case). Demonstrates
  `UserPromptSubmit` + `AssistantMessageRender`.
- [`examples/extensions/bash-deny-rm-rf/`](../../examples/extensions/bash-deny-rm-rf/)
  — `PreToolUse` with `matcher: "Bash"`, blocks `rm -rf`. Demonstrates
  `decision: "block"`.
- [`examples/extensions/auto-test/`](../../examples/extensions/auto-test/) —
  `PostToolUse` with `matcher: "Write|Edit"`, runs the test suite and
  injects PASS/FAIL via `additionalContext`.
- [`examples/extensions/system-prompt-trim/`](../../examples/extensions/system-prompt-trim/)
  — `SystemPromptCompose`, strips the `Git Diff:` block for
  token-efficiency experiments.
