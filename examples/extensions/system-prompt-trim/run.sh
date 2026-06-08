#!/bin/sh
# SystemPromptCompose hook — strip the `Git Diff:` block from the
# system prompt before sending it to the model.
#
# Demonstrates the v2 `SystemPromptCompose` event + `updatedSystemPrompt`
# rewrite — useful for token-efficiency experiments where you want to
# A/B test whether the diff block actually helps the model on YOUR
# codebase versus its cost in tokens.
#
# Install:
#   cp -R examples/extensions/system-prompt-trim ~/.ignis/extensions/system-prompt-trim
#   chmod +x ~/.ignis/extensions/system-prompt-trim/run.sh
#
# Wire in ~/.ignis/extensions.json:
#   { "extensions": { "SystemPromptCompose": [
#       { "command": "~/.ignis/extensions/system-prompt-trim/run.sh" }
#   ]}}

set -eu

envelope=$(cat)

# Read the assembled system prompt out of the envelope.
prompt=$(printf '%s' "$envelope" | jq -r '.system_prompt // ""')

# Strip the "Git Diff:" code block (```...```). The exact framing comes
# from ignis's build_system_prompt — if upstream changes the block's
# label, update this pattern.
trimmed=$(printf '%s' "$prompt" | awk '
    BEGIN { skip=0 }
    /^Git Diff:$/ { skip=1; next }
    skip && /^```$/ {
        if (closed) { skip=0; closed=0; next }
        closed=1; next
    }
    !skip { print }
')

# Emit the rewritten prompt. jq escapes for JSON.
escaped=$(printf '%s' "$trimmed" | jq -Rs .)
printf '{"hookSpecificOutput":{"hookEventName":"SystemPromptCompose","updatedSystemPrompt":%s}}\n' "$escaped"
