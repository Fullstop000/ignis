#!/bin/sh
# PreToolUse hook — deny `rm -rf` style commands on `Bash`.
#
# Reads the envelope on stdin, extracts `tool_input.command`, and refuses
# any command matching a destructive-rm pattern. Returns
# `decision: "block"` with a reason the model sees as a system reminder
# on the next turn.
#
# Install:
#   cp -R examples/hooks/bash-deny-rm-rf ~/.ignis/hooks/bash-deny-rm-rf
#   chmod +x ~/.ignis/hooks/bash-deny-rm-rf/run.sh
#
# Wire in ~/.ignis/hooks.json:
#   { "hooks": { "PreToolUse": [
#       { "command": "~/.ignis/hooks/bash-deny-rm-rf/run.sh", "matcher": "Bash" }
#   ]}}

set -eu

# Read the JSON envelope. jq parses it.
envelope=$(cat)

# `tool_input.command` is the bash command the model is about to run.
cmd=$(printf '%s' "$envelope" | jq -r '.tool_input.command // ""')

# Match the destructive pattern. Accept common variants:
#   rm -rf /          rm -fr /
#   rm -rf $HOME      rm -r -f anything
#   sudo rm -rf ...
case "$cmd" in
    *"rm -rf"*|*"rm -fr"*|*"rm -r -f"*|*"rm -f -r"*)
        # Block the call. The reason is surfaced to the model as a
        # system reminder framed "Blocked by hook: ...".
        printf '{"decision":"block","reason":"refused destructive rm -rf pattern"}\n'
        exit 0
        ;;
esac

# Pass-through: empty stdout + exit 0.
exit 0
