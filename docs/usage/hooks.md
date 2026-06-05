# Hooks

Hooks let an external program subscribe to ignis lifecycle events and,
where the event permits it, rewrite the data flowing through. v1 ships
two events — `UserPromptSubmit` (mutates the prompt before model send)
and `AssistantMessageRender` (mutates the assistant's text before TUI
render).

> ## ⚠ Hooks run unsandboxed in v1
>
> Each hook command runs with the full privileges of your `ignis`
> process. A malicious or buggy hook can:
>
> - Read `~/.ssh`, `~/.aws/credentials`, `~/.config/gh/`, `.netrc`,
>   any project source.
> - See and exfiltrate every env var ignis was started with —
>   including `ANTHROPIC_API_KEY`.
> - Spawn child processes; write/delete arbitrary files; make
>   arbitrary network calls.
>
> **Treat `~/.ignis/hooks.json` like `crontab`** — anything in there
> has root-equivalent power over your user account. Only install hooks
> whose source you have personally audited.
>
> v2 will add env-var scrubbing and a Linux filesystem sandbox; until
> then, the protocol relies on you reading the script.

## Events

### `UserPromptSubmit`

Fires in `Session::prompt` immediately before the user message is
pushed to history. Hooks run in declared order; each receives the
output of the previous (chaining). The final string is stored.

### `AssistantMessageRender`

Fires on `MessageEnd` for an assistant message, before the TUI commits
the rewritten block. Hooks chain in declared order. **History stores
the model's original output**, not the rewritten render — so prompt
cache stays clean and replay is exact. The rewrite shows as a
labelled `[hook rewrite]` block immediately below the model's original.

## Envelope

### stdin — JSON object

```json
{
  "hook_event_name": "UserPromptSubmit",
  "session_id": "session-…",
  "cwd": "/home/you/project",
  "prompt": "<user's text>"
}
```

- `prompt` is present for `UserPromptSubmit`.
- `content` is present for `AssistantMessageRender`.
- The other field is omitted.

### stdout — JSON object (all fields optional)

```json
{
  "continue": true,
  "systemMessage": "Optional 1-line note shown in TUI",
  "hookSpecificOutput": {
    "hookEventName": "UserPromptSubmit",
    "updatedInput": "<rewritten prompt>"
  }
}
```

For `AssistantMessageRender`, the rewrite field is `updatedOutput`.
Absent rewrite field, or `continue: false`, means "no rewrite from this
hook" — but `continue: false` is also a block signal (see exit codes).

## Exit codes

| Code | Behaviour |
|---|---|
| `0` | OK. stdout is parsed; absent/empty stdout = pass-through. |
| `2` | Block the chain. Honoured for `UserPromptSubmit` (turn does not send). Degraded to a soft failure for `AssistantMessageRender`. |
| anything else | Soft failure: original text kept; a `[warn]` line is committed to scrollback. |

A hook that runs longer than its `timeout_ms` is killed (SIGKILL,
via `kill_on_drop`) and treated as a soft failure. v2 will add a
SIGTERM grace window before the SIGKILL.

## Declaration — `~/.ignis/hooks.json`

```json
{
  "hooks": {
    "UserPromptSubmit": [
      {
        "command": "~/.ignis/hooks/translate-en/run.py",
        "timeout_ms": 30000
      }
    ],
    "AssistantMessageRender": [
      {
        "command": "~/.ignis/hooks/translate-en/run.py",
        "timeout_ms": 30000
      }
    ]
  }
}
```

- `command` is **split on whitespace** at parse time and passed argv-
  style to `Command::new`. No shell is involved. No `$VAR` expansion.
  Only a leading `~/` is expanded (against the home dir).
- For program paths that **contain whitespace** (e.g.
  `/Users/foo bar/run.py`), use the explicit `argv` form instead:

  ```json
  { "argv": ["/Users/foo bar/run.py", "--display"], "timeout_ms": 30000 }
  ```

  `argv[0]` is the program; subsequent entries are arguments. `~/` is
  expanded on `argv[0]`. `command` and `argv` are mutually exclusive.
- `timeout_ms` defaults to `10000` (10 s).
- Each event takes a JSON array — multiple hooks chain left-to-right,
  each receiving the previous hook's output.
- The file is loaded at session start. An absent file means no hooks
  and no log noise. A malformed file is a startup error — ignis exits
  before the first prompt (same posture as a broken `config.toml`).

### Inspecting the active chains — `/hooks` (or `/hooks list`)

Type `/hooks` (or its explicit alias `/hooks list`) to print the
chains that the running session is actually using — one block per
event, each entry showing the program path, its argv tail, and the
per-hook timeout. The leftmost column is the hook's `display_name()`
(its program file's stem, no directory or extension):

```
[info] 3 hooks registered · /hooks reload to re-read · run unsandboxed; audit before installing:
  UserPromptSubmit (2):
    · translate-en  ~/.ignis/hooks/translate-en/run.py  (timeout 10000ms)
    · redact        /opt/ignis/hooks/redact.sh --strict  (timeout 30000ms)
  AssistantMessageRender (1):
    · translate-en  ~/.ignis/hooks/translate-en/run.py  (timeout 10000ms)
```

(The `translate-en` in the name column there assumes your program
lives at `…/translate-en/run` — the name is the stem, not the
directory. If your hook is `…/translate-en/translate.py`, the column
will show `translate`.)

When no hooks are registered, the command prints a single
`[info] no hooks registered` line pointing at the file path and the
`/hooks reload` action. The list reflects the in-memory state — the
last successful load or `/hooks reload` — not a live disk probe, so
`/hooks reload` first if you just edited the file.

### Hot-reload — `/hooks reload`

Type `/hooks reload` in the TUI after editing `hooks.json`. The parsed
config is swapped into the running registry; the next prompt picks it
up. The confirmation line includes the unsandboxed reminder.

## Failure UI

Every soft failure commits a `[warn] <event>: <reason> (<hook-name>)`
line below the affected block. No rate-limiting — transparency over
visual cleanliness. If you'd rather not see them, audit and disable
the misbehaving hook.

## Observability

Each hook invocation emits a `tracing` span named `ignis.hook` with
attributes `event`, `command`, `duration_ms`, `outcome` (`mutated` /
`pass_through` / `blocked` / `failed`). Enable
`IGNIS_ENABLE_TELEMETRY=1` to export them via OpenTelemetry.

## Reference translator

A worked example lives at `examples/hooks/translate-en/`. It's a
single Python script (~80 LOC) that routes on `hook_event_name`,
masks code blocks with sentinels, and calls Anthropic Haiku. See its
README for install/run instructions.
