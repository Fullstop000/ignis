#!/bin/sh
# PostToolUse extension — auto-run `cargo test --workspace -q` after every
# create_file/edit_file and inject the pass/fail result as additionalContext
# for the next LLM call.
#
# Demonstrates the v2 `PostToolUse` event + `additionalContext`
# injection: the agent's next turn sees a system reminder framing what
# the test suite did, so it can react (fix the regression) without the
# user pasting test output in by hand.
#
# Install:
#   cp -R examples/extensions/auto-test ~/.ignis/extensions/auto-test
#   chmod +x ~/.ignis/extensions/auto-test/run.sh
#
# Wire in ~/.ignis/extensions.json:
#   { "extensions": { "PostToolUse": [
#       { "command": "~/.ignis/extensions/auto-test/run.sh",
#         "matcher": "create_file|edit_file", "sandbox": false, "timeout_ms": 120000 }
#   ]}}

set -eu

# Drain stdin — we don't read the envelope for this simple example; the
# matcher already guarantees we fire on create_file/edit_file only.
cat >/dev/null

cd "${IGNIS_HOOK_CWD:-$PWD}"

# Run the test suite. -q for compact output; pipe through tail so we
# only inject the summary, not the full log.
output=$(cargo test --workspace -q 2>&1 | tail -20)
status=$?

if [ "$status" = "0" ]; then
    context="cargo test --workspace -q: PASSED"
else
    context=$(printf 'cargo test --workspace -q: FAILED\n%s' "$output")
fi

# Emit the context as additionalContext. The agent loop drains this
# and prepends a `<system-reminder>` block before the next LLM call.
# Newlines in the context need escaping for JSON.
escaped=$(printf '%s' "$context" | jq -Rs .)
printf '{"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":%s}}\n' "$escaped"
