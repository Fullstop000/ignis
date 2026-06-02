# Hooks

Hooks let an external program subscribe to ignis lifecycle events and,
where the event permits it, rewrite the data flowing through. v1 ships
two events — `UserPromptSubmit` (mutates the prompt before model send)
and `AssistantMessageRender` (mutates the assistant's text before TUI
render).

> ## Hook sandbox (v2)
>
> v2 confines each hook subprocess with four layers, defaults-on:
>
> 1. **Env-var allowlist (all platforms).** The child sees only
>    `PATH HOME USER LANG LC_ALL TZ` from ignis's own env, plus
>    whatever extra names the hook declares in `env: [...]`. Default
>    `env: []`. A hook that doesn't declare `ANTHROPIC_API_KEY` does
>    not see `ANTHROPIC_API_KEY`.
> 2. **Linux Landlock filesystem sandbox.** Per hook `sandbox: bool`,
>    default `true`. The hook can **read** its own folder, `/etc/ssl/certs`,
>    `/usr/lib`, `/lib`, `/lib64`, `/bin`, `/usr/bin`, `/sbin`,
>    `/usr/sbin`, `/etc/resolv.conf`, `$TMPDIR`, `/dev/urandom`; it can
>    **write** `$TMPDIR` and `/dev/null`. Net access is unrestricted —
>    that's fine because the env-var allowlist already stops it learning
>    a credential it could exfil. On non-Linux platforms the flag is a
>    no-op and a one-time `[warn] hook.sandbox: <hook>: Landlock
>    unavailable on this kernel; hook runs unconfined` notice fires per
>    session. Set `sandbox: false` if your hook genuinely needs broader
>    access (e.g. a project-indexer that reads the whole tree).
> 3. **SIGTERM → 1 s grace → SIGKILL on timeout.** A misbehaving hook
>    gets one second of "please clean up" before it's force-killed.
> 4. **1 MiB cap per stdout / stderr stream.** Bytes beyond the cap are
>    discarded after a `[warn] hook.buffer: <hook>: <stream>
>    truncated at 1 MiB` is committed to scrollback; the truncated
>    bytes never accumulate in memory.
>
> The defaults are tuned for the reference translator: a sandboxed
> Python hook can read its own files, call the Anthropic / OpenAI
> HTTPS API (TLS roots + DNS allowed), and write nothing outside
> `/tmp`. Hooks that need more must declare it.
>
> **You should still audit `~/.ignis/hooks.json` like a crontab.** The
> sandbox closes filesystem and credential exfil; it does not (yet)
> filter network egress. A hook with `network = unrestricted` that
> also declared `env: ["ANTHROPIC_API_KEY"]` can absolutely send the
> key off-host.

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

A hook that runs longer than its `timeout_ms` is sent `SIGTERM`,
given one second to exit cleanly, and then `SIGKILL`'d. Outcome is
`SoftFailed { reason: "timed out after Nms" }` — the original prompt
(or assistant text) passes through unchanged.

## Declaration — `~/.ignis/hooks.json`

```json
{
  "hooks": {
    "UserPromptSubmit": [
      {
        "command": "~/.ignis/hooks/translate-en/run.py",
        "env": ["ANTHROPIC_API_KEY", "IGNIS_TRANSLATE_FROM", "IGNIS_TRANSLATE_TO"],
        "sandbox": true,
        "timeout_ms": 30000
      }
    ],
    "AssistantMessageRender": [
      {
        "command": "~/.ignis/hooks/translate-en/run.py",
        "env": ["ANTHROPIC_API_KEY"],
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
- `env` is an array of env-var **names** ignis passes through from its
  own environment into the hook's. Defaults to `[]`. The universal
  allowlist (`PATH HOME USER LANG LC_ALL TZ`) is always passed in
  addition. A name listed but unset in ignis's env is silently
  skipped.
- `sandbox` toggles the Linux Landlock filesystem confinement.
  Defaults to `true`. Set `false` for hooks that legitimately need
  broader filesystem access. On non-Linux platforms the flag has no
  effect — the one-time `[warn] hook.sandbox` notice is your hint.
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
up. Reload also clears the per-session "Landlock unavailable" warning
suppression set, so a freshly-edited hook gets its degradation notice
again if the kernel doesn't support Landlock.

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
