# Slash commands

Ignis's TUI accepts a small set of built-in slash commands. Start typing `/`
to open the autocomplete menu — use `Up`/`Down` to highlight a suggestion and
`Enter` to run it. Fuzzy matching also lets a command surface from a substring
of its description (e.g. typing `/new` highlights `/clear`).

User-installed skills also appear in the menu as `/<skill-name>` once enabled.
See [skills](#skills) for management.

---

### `/resume`

List previously saved sessions for this project and open one. Selecting a row
swaps the live session for the chosen one; the current session is kept and
shows up at the top of the list if it isn't already there.

```
/resume
```

---

### `/clear`

Start a fresh session. The current session is preserved on disk; the TUI just
swaps to a new one.

```
/clear
```

---

### `/compact`

Summarize the earlier turns of the current session and replace them with that
summary, freeing context for the rest of the conversation. The compacted
session is saved over the existing session id.

```
/compact
```

---

### `/copy`

Copy the last assistant message to the system clipboard. Uses the platform
clipboard CLI (`pbcopy` / `clip` / `clip.exe` / `wl-copy` / `xclip`) — no
native dependency.

```
/copy
```

---

### `/model`

Open the model picker. Lists every model declared under each configured
provider in `~/.ignis/config.toml` and the active selection. Picking a row
switches the running session and persists the choice to
`~/.ignis/state.json`.

```
/model
```

---

### `/skills`

Open the skills picker — toggle individual skills on or off. Disabled skills
are hidden from the tool catalog and from `/<skill-name>` autocomplete. State
persists per project.

```
/skills
```

Skills are discovered from four roots in this precedence (project beats
global, `.ignis` beats `.agents` on name collision):

1. `~/.agents/skills/`
2. `~/.ignis/skills/`
3. `./.agents/skills/`
4. `./.ignis/skills/`

---

### `/mcp`

Open the MCP-servers picker — enable or disable configured Model Context
Protocol servers without leaving the TUI. Use the
`ignis mcp add|list|get|remove|enable|disable` subcommand to manage
the configuration itself.

```
/mcp
```

---

### `/afk`

Toggle **AFK mode**. While on, tool calls auto-approve and `ask_user`
prompts auto-dismiss so the agent can run unattended. Off by default — opens a
confirmation picker before flipping state.

```
/afk
```

---

### `/telemetry`

Print the current OpenTelemetry exporter status (endpoint, headers redacted,
sample run-time counters) as an assistant notice. Read-only.

```
/telemetry
```

See [telemetry.md](./telemetry.md) for setup.

---

### `/sessions`

Print a one-shot summary of this project's stored sessions (count, total
message count, on-disk size) as an assistant notice. Read-only.

```
/sessions
```

---

### `/<skill-name>`

For any enabled skill, typing its name as a slash command force-loads the
skill body into the next turn and skips the model's need to call the `skill`
tool itself. Anything after the command is appended as the user's prompt.

```
/<skill-name> [optional prompt]
```

Bundled-file skills additionally get their resource directory + file list
injected so the model can `Read` referenced assets.

---

## Keybindings

Slash commands have no dedicated per-command keybinds. The following global
keys apply while the TUI input is active:

| Key | Action |
|---|---|
| `/` | Open the slash autocomplete menu |
| `Up` / `Down` | Highlight previous / next suggestion (or history when input is empty) |
| `Tab` / `Enter` | Run the highlighted suggestion |
| `Esc` | Close picker or autocomplete |
| `Ctrl+J` | Insert a newline (without submitting) |
| `Ctrl+U` | Clear the input line |
| `Ctrl+A` / `Ctrl+E` | Move cursor to start / end |
| `Ctrl+W` | Delete previous word |
| `Ctrl+S` | Steer the running turn (queue an instruction mid-stream) |
| `Ctrl+C` | Cancel the running turn / clear the input |
| `Ctrl+D` | Exit ignis |
