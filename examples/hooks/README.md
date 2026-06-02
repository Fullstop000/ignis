# ignis hooks — examples

Hooks let an external program subscribe to ignis lifecycle events and,
where the event permits it, **rewrite** the data flowing through.
`v1` ships two events; this folder collects working hook scripts you can
copy, install in `~/.ignis/hooks/`, and reference from
`~/.ignis/hooks.json`.

| Hook | What it does |
|---|---|
| [`translate-en/`](./translate-en/) | Bilingual prompt + reply translator (Anthropic Haiku). |

## Writing your own

A hook is **any executable** ignis can `spawn`. The protocol is one
JSON message in on stdin, one JSON message out on stdout. Exit code
governs blocking semantics. Language-agnostic — Python, shell, Go,
Rust, whatever you like.

### Envelope — stdin

```json
{
  "hook_event_name": "UserPromptSubmit",
  "session_id": "session-…",
  "cwd": "/home/you/project",
  "prompt": "<user's text>"
}
```

`hook_event_name` is `"UserPromptSubmit"` or `"AssistantMessageRender"`.
`prompt` is set for the former; `content` is set for the latter. The
other field is omitted.

### Response — stdout (all fields optional)

```json
{
  "continue": true,
  "systemMessage": "optional one-liner displayed in TUI",
  "hookSpecificOutput": {
    "hookEventName": "UserPromptSubmit",
    "updatedInput": "<rewrite>"
  }
}
```

For `AssistantMessageRender`, the rewrite field is `updatedOutput`.
Absent `updatedInput`/`updatedOutput` = pass-through.

### Exit codes

| Code | Meaning |
|---|---|
| `0` | OK. Use stdout if it parses; pass-through if empty/no rewrite. |
| `2` | Block the chain. `UserPromptSubmit` honours this; `AssistantMessageRender` degrades it to a soft failure (we can't lose a message we already produced). |
| anything else | Soft failure — original text is kept and a `[warn]` line is committed to scrollback. |

### Declare it

`~/.ignis/hooks.json`:

```json
{
  "hooks": {
    "UserPromptSubmit": [
      {"command": "~/.ignis/hooks/your-hook/run.py"}
    ]
  }
}
```

`command` is split on whitespace at parse time — **no shell** is
involved, no `$VAR` expansion, no globbing. Only the leading `~/` is
expanded. Each entry may set `timeout_ms` (default 10000).

Type `/hooks reload` after editing the file to pick up changes without
restarting ignis.

> **Warning — hooks run unsandboxed in v1.** Each hook command runs
> with the full privileges of your `ignis` process: every env var (incl.
> API keys), every file ignis can read, full network access. Treat
> `hooks.json` like `crontab` — only install hooks whose source you've
> personally audited. v2 will add env scrubbing + a Linux Landlock
> sandbox; until then, the protocol relies on you reading the script.

See [`docs/usage/hooks.md`](../../docs/usage/hooks.md) for the full
user-facing reference.
