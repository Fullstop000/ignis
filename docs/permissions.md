# Permissions

Ignis gates every tool call the model wants to run. The gate decides
`Allow`, `Ask` (open a picker), or `Deny`. Sensitive tools (`bash`,
`edit_file`, `create_file`, `web_fetch`, `agent`, MCP) never run without
going through it.

This page covers the three modes, the user-declarable rule layer
(`[permissions]` + "Always allow"), the safety floor, and how to turn
modes on and off.

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
| `Always allow`    | Save a rule to `state.json` so matching calls run silently in every future session (see [Permission rules](#permission-rules)). |
| `Deny`            | Reject this call. The model receives the rejection and chooses what to do next.        |

> `Approve session` is tool-scoped, not command-scoped. Approving `bash`
> for the session allows every subsequent `bash` call this session, not
> just the one you saw. For finer, persistent control, use `Always allow`
> (it saves a command-scoped rule) or pre-declare rules in `config.toml`.

`Esc` or `Ctrl+C` cancels the call and returns the user-cancelled error
to the model.

## Permission rules

Beyond the per-tool defaults, you can pre-declare rules in
`~/.ignis/config.toml` so common calls stop prompting and dangerous ones
are blocked outright:

```toml
[permissions]
allow = ["bash(git *)", "bash(cargo *)", "edit_file(src/**)"]
ask   = ["bash(git push *)"]
deny  = ["bash(rm -rf *)", "read_file(.env)", "read_file(**/secrets/**)"]
```

Each entry is a `Tool(pattern)` string; a bare tool name (`"bash"`) matches
every use. Rules are evaluated **deny > ask > allow** — one matching `deny`
anywhere wins. The rule layer sits *beneath* the safety floor (which always
wins) and *above* session-allow and the AFK modes: a config `deny`
overrides a prior `Approve session`, and a config `ask` forces a prompt
even under Hands-free (and hard-denies under Fully unattended).

Pattern syntax by tool:

- **bash** — glob over the command; `*` matches any run of characters
  (including spaces). A trailing ` *` also matches the bare command, so
  `bash(git *)` covers both `git` and `git status -s`. Compound commands
  are split on `&& || ; |` and each segment is checked independently: an
  `allow` rule only allows a command when *every* segment is covered (or
  read-only), so `allow = ["bash(git *)"]` will **not** green-light
  `git x && rm -rf y`.
- **edit_file / create_file / read_file** — glob over the path. `**`
  crosses directories, `*` stays within one segment. Anchors: `//abs`,
  `~/home`, `/project-root`; bare or `./` is project-relative; a bare
  filename is recursive (`read_file(.env)` ≡ `**/.env`).
- **web_fetch** — `web_fetch(domain:HOST)`, host globbed
  (`web_fetch(domain:*.github.com)`).
- **anything else** (`agent`, `mcp__<server>__<tool>`) — bare tool-name
  match.

An unparseable rule is logged and skipped, never fatal.

### "Always allow"

The picker's `Always allow` option writes a rule to
`~/.ignis/state.json` (`permission_grants`) in the same grammar, folded
into `allow` at next launch. For bash it suggests an arity-trimmed prefix —
clicking it on `git status --porcelain` saves `bash(git status *)`, not all
of `bash`; for a file it saves the concrete path, for `web_fetch` the host.
Grants are plain strings you can read, edit, or delete.

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

Not yet shipped:

- `/permissions` — in-TUI inspection and ad-hoc rule edits.
- **Blanket-deny tool omission** — a bare `deny = ["bash"]` drops the tool
  from the model's context for the turn (today it's denied at call time).
- `acceptEdits` mode — auto-approve file edits inside the working
  directory, prompt for everything else.
- `plan` mode — read-only exploration with no edits.
- **Multi-scope rule layering** — managed > project > user precedence for
  the rule grammar (today there's one `config.toml`).
- **OS-level sandboxing** — Linux Landlock filesystem restrictions for
  bash calls, as a defense-in-depth layer below the permission gate.

Anything not listed above is not on the near-term roadmap.
