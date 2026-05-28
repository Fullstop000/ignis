# Permissions

Ignis gates every tool call the model wants to run. The gate decides
`Allow`, `Ask` (open a picker), or `Deny`. Sensitive tools (`bash`,
`edit_file`, `create_file`, `web_fetch`, `agent`, MCP) never run without
going through it.

This page covers what's shipped in v0.17.0: the three modes, the safety
floor, and how to turn modes on and off.

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

There is one axis — *"how much should ignis prompt me?"* — with three
points on it.

| Mode                 | Sensitive tools | Safety floor | `ask_user` | Mental model                                                  |
| -------------------- | --------------- | ------------ | ---------- | ------------------------------------------------------------- |
| **Off** (default)    | prompt          | ask          | prompt     | "Check each call with me."                                    |
| **Hands-free**       | auto-approve    | ask          | prompt     | "I'm at the keyboard, want flow, still consult me on design." |
| **Fully unattended** | auto-approve    | **deny**     | dismiss    | "Nobody's home — make your best judgment."                    |

The two AFK levels share auto-approve of sensitive tools but differ on
(a) the safety floor (Ask under Hands-free, hard Deny under Fully
unattended — there's no one to confirm), and (b) whether `ask_user`
prompts or auto-dismisses.

## Choosing a mode

### `/afk` (slash command)

`/afk` in the TUI opens a picker with the two AFK levels (when current
mode is Off), or turns AFK off immediately (when in any AFK level).
This *asymmetric gate* is on purpose — enabling AFK changes what the
model can do without your sign-off, so we confirm; disabling strictly
increases safety, so it fires immediately.

To switch *between* Hands-free and Fully unattended, `/afk` → off, then
`/afk` again and pick. The two-step matches the gate principle (any
escalation gets friction).

The chosen mode persists in `~/.ignis/state.json` (`mode` field) and is
restored at next launch.

### `--afk` (CLI flag)

`--afk` at launch pins **Fully unattended** for the session, bypassing
the picker. It's the right flag for CI, overnight, headless runs.

There is no CLI flag for Hands-free — at the keyboard, `/afk` is the
way in.

### One-shot CLI

`ignis "<prompt>"` implies **Fully unattended** automatically — there's
no TTY to open a picker on, and waiting on a missing user would hang.

## Permission picker

When the gate decides `Ask`, ignis pauses the agent and opens a picker
with three options:

| Option            | Effect                                                                                 |
| ----------------- | -------------------------------------------------------------------------------------- |
| `Approve once`    | Allow this single tool call. The next call to the same tool will ask again.            |
| `Approve session` | Allow **any** call to this tool for the rest of this process. Not persisted to disk.   |
| `Deny`            | Reject this call. The model receives the rejection and chooses what to do next.        |

> `Approve session` is tool-scoped, not command-scoped. Approving `bash`
> for the session allows every subsequent `bash` call this session, not
> just the one you saw. Per-command allowlist grammar (e.g.
> `Bash(git *)`) is on the roadmap.

`Esc` or `Ctrl+C` cancels the call and returns the user-cancelled error
to the model.

## Safety floor

Two patterns always trigger the gate, even under Hands-free or a prior
`Approve session`:

**Circuit breakers** — the `rm -rf /` family. Stripped of leading `sudo`
and `VAR=value` prefixes, and checked against each segment of a compound
command (`ls; rm -rf /`, `true && rm -rf $HOME`, etc.):

- `rm -rf /` (and `rm -fr /`)
- `rm -rf ~` (and `rm -fr ~`)
- `rm -rf $HOME` (and `rm -fr $HOME`)

**Protected paths** — edits via `edit_file` or `create_file` to:

- `.git/**` — your repository's metadata
- `.ignis/**` — ignis's own state and config
- `.bashrc`, `.zshrc`, `.profile`, `.gitconfig` — shell init files

Under Off or Hands-free these prompt for explicit approval. Under Fully
unattended they hard `Deny`.

The floor is intentionally small, covers catastrophic-and-easily-
recognized cases, and doesn't try to reason about every destructive
command. Sandbox-level enforcement (Linux Landlock) is on the roadmap.

## Roadmap

Planned for v0.18.0 and later (not shipped in v0.17.0):

- **Allowlist grammar** — settings.json `allow` / `ask` / `deny` arrays
  with patterns like `Bash(git *)`, `Edit(/src/**)`,
  `WebFetch(domain:example.com)`, `mcp__<server>` — the main vocabulary
  for fine-grained policy.
- `/permissions` — in-TUI inspection and ad-hoc rule edits.
- `acceptEdits` mode — auto-approve file edits inside the working
  directory, prompt for everything else.
- `plan` mode — read-only exploration with no edits.
- **Three-tier settings layering** — managed > project > user
  precedence for the allowlist grammar above.
- **OS-level sandboxing** — Linux Landlock filesystem restrictions for
  bash calls, as a defense-in-depth layer below the permission gate.
- **TUI mode badges** — colored footer indicator when in Hands-free or
  Fully unattended.

Anything not listed above is not on the near-term roadmap.
