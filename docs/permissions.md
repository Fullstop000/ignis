# Permissions

Ignis gates every tool call the model wants to run. This page documents what's
shipped today (v0.17.0), what each control actually does, and where the safety
floor is — the things that always ask, regardless of mode.

The model never edits files, runs shell commands, or fetches the web without
going through the gate. The gate's decision is `Allow`, `Ask` (open a picker),
or `Deny`.

## Permission system

| Tool                                                                  | Default | Notes                          |
| --------------------------------------------------------------------- | ------- | ------------------------------ |
| `read_file`, `list_dir`, `grep`, `glob`, `web_search`, `skill`        | Allow   | Read-only, no prompt.          |
| `ask_user`                                                            | Allow   | Already user-facing by design. |
| `bash`                                                                | Ask*    | *Auto-allowed for ~30 read-only commands (`ls`, `cat`, `git status`, `pwd`, `which`, …). Anything else prompts. |
| `edit_file`, `create_file`                                            | Ask     | File modification.             |
| `web_fetch`                                                           | Ask     | Network fetch.                 |
| `agent`                                                               | Ask     | Spawn sub-agent.               |
| `mcp__<server>__<tool>`                                               | Ask     | All MCP server calls.          |

The read-only bash set is built in and not configurable.

## Permission modes

Set with `--permission-mode <mode>` on launch, persisted into `~/.ignis/state.json`.

| Mode                | Behavior                                                                              |
| ------------------- | ------------------------------------------------------------------------------------- |
| `default`           | Prompts on first use of each sensitive tool. This is the default.                     |
| `bypassPermissions` | Auto-allows everything *except* the [safety floor](#safety-floor) below.              |

> `bypassPermissions` is not "anything goes." Circuit breakers (`rm -rf /` family)
> and protected-path edits (`.git/**`, `.ignis/**`, shell init files) still ask.
> Use this mode only when you understand that contract.

A typo'd mode value errors at startup with the list of valid options — it does
not silently fall back to `default`.

`acceptEdits` and `plan` modes are planned; see [roadmap](#roadmap).

## AFK mode

AFK ("away from keyboard") is a separate toggle that automates user interaction
for headless or unattended runs. It is independent of permission mode.

When AFK is on:
- Tool calls that would `Ask` are auto-approved.
- `ask_user` returns a structured dismissal (`{dismissed: true, reason: …}`)
  instead of opening a picker. The model is told no user is present and to
  proceed with its best judgment.
- The safety floor still applies. Circuit breakers and protected-path edits
  become hard `Deny` under AFK (no human is there to confirm).

Enable with `--afk` on launch, or `/afk` in-session. One-shot CLI invocations
(`ignis "do X"`) enable AFK implicitly — there is no TTY to prompt on.

### The asymmetric `/afk` gate

- `/afk` to **enable** AFK in an interactive session opens a confirmation picker.
  Enabling AFK strictly increases what the model can do without your sign-off,
  so we ask first.
- `/afk` to **disable** AFK fires immediately. Disabling strictly increases
  safety, so no confirmation is needed.
- The `--afk` CLI flag bypasses the confirmation (you explicitly asked for AFK
  when launching).

## Permission picker

When the gate decides `Ask`, ignis pauses the agent and opens a picker with
three options:

| Option            | Effect                                                                                 |
| ----------------- | -------------------------------------------------------------------------------------- |
| `Approve once`    | Allow this single tool call. The next call to the same tool will ask again.            |
| `Approve session` | Allow **any** call to this tool for the rest of this process. Not persisted to disk.   |
| `Deny`            | Reject this call. The model receives the rejection and chooses what to do next.        |

> `Approve session` is tool-scoped, not command-scoped. Approving `bash` for the
> session allows every subsequent `bash` call this session, not just the one you
> saw. Per-command allowlist grammar (e.g. `Bash(git *)`) is on the roadmap.

The picker reuses the same channel as `/model`, `/skills`, etc. — `Esc` or
`Ctrl+C` cancels the call and returns the user-cancelled error to the model.

## Safety floor

Two patterns always trigger the gate, even under `bypassPermissions` or a prior
`Approve session`:

**Circuit breakers** — the `rm -rf /` family. Stripped of leading `sudo` and
`VAR=value` prefixes, and checked against each segment of a compound command
(`ls; rm -rf /`, `true && rm -rf $HOME`, etc.):

- `rm -rf /` (and `rm -fr /`)
- `rm -rf ~` (and `rm -fr ~`)
- `rm -rf $HOME` (and `rm -fr $HOME`)

**Protected paths** — edits via `edit_file` or `create_file` to:

- `.git/**` — your repository's metadata
- `.ignis/**` — ignis's own state and config
- `.bashrc`, `.zshrc`, `.profile`, `.gitconfig` — shell init files

Under `default` mode these prompt for explicit approval. Under AFK they hard
`Deny`.

The breaker is a floor, not a ceiling: it's deliberately small, covers the
catastrophic-and-easily-recognized cases, and doesn't try to reason about every
destructive command. Sandbox-level enforcement (Linux Landlock) is on the
roadmap.

## CLI flags

```bash
ignis --permission-mode default            # explicit
ignis --permission-mode bypassPermissions  # auto-allow except safety floor
ignis --afk                                # enable AFK at launch
```

Resolution order: `--permission-mode` CLI flag → `permission_mode` in
`~/.ignis/state.json` → `default`. The CLI flag also writes through to
`state.json`, so it sticks across restarts until changed.

`--afk` is per-invocation. AFK is also implicitly enabled for one-shot
`ignis "<prompt>"` runs.

## Slash commands

| Command | Effect                                                                                              |
| ------- | --------------------------------------------------------------------------------------------------- |
| `/afk`  | Toggle AFK mode. Enabling prompts confirmation; disabling fires immediately.                        |

`/permissions` (in-session inspection + ad-hoc rule edits) is on the roadmap.

## Roadmap

Planned for v0.18.0 and later (not shipped in v0.17.0):

- **Allowlist grammar** — settings.json `allow` / `ask` / `deny` arrays with
  patterns like `Bash(git *)`, `Edit(/src/**)`, `WebFetch(domain:example.com)`,
  `mcp__<server>` — the main vocabulary for fine-grained policy.
- `/permissions` — in-TUI inspection and ad-hoc rule edits.
- `acceptEdits` mode — auto-approve file edits inside the working directory.
- `plan` mode — read-only exploration with no edits.
- **Three-tier settings layering** — managed > project > user precedence for
  the allowlist grammar above.
- **OS-level sandboxing** — Linux Landlock filesystem restrictions for bash
  calls, as a defense-in-depth layer below the permission gate.

Anything not listed above is not on the near-term roadmap.
