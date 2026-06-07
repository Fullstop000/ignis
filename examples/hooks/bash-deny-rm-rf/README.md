# bash-deny-rm-rf — PreToolUse safety hook

Refuses `rm -rf` (and common variants) before the bash tool runs.
Demonstrates the v2 `PreToolUse` event + `decision: "block"` flow.

When the model proposes `rm -rf /foo`, this hook returns
`{"decision": "block", "reason": "refused destructive rm -rf pattern"}`
and the agent loop surfaces the reason to the next turn as a system
reminder — the model sees a tool error and adapts (or asks the user).

## Install

```sh
mkdir -p ~/.ignis/hooks
cp -R examples/hooks/bash-deny-rm-rf ~/.ignis/hooks/bash-deny-rm-rf
chmod +x ~/.ignis/hooks/bash-deny-rm-rf/run.sh
```

Requires `jq` on `PATH`.

## Wire in `~/.ignis/hooks.json`

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "command": "~/.ignis/hooks/bash-deny-rm-rf/run.sh",
        "matcher": "Bash",
        "timeout_ms": 2000
      }
    ]
  }
}
```

The `matcher` field is a regex on the tool name — `"Bash"` means the
hook only fires for bash calls; PreToolUse on `Edit` or `Read` skips
this hook without paying a spawn cost.

Reload without restarting: type `/hooks reload`.

## What it blocks

The pattern match covers:

- `rm -rf <anything>`
- `rm -fr <anything>` (swapped flags)
- `rm -r -f` / `rm -f -r` (separated flags)
- `sudo rm -rf ...` (anything followed by the pattern)

It does NOT block `rm -r` or `rm -f` alone — only the destructive
combination. Tune the `case` patterns in `run.sh` for your project.
