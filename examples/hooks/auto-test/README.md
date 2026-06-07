# auto-test — PostToolUse test-after-edit hook

Runs `cargo test --workspace -q` after every `Write` or `Edit` and
injects PASS / FAIL into the model's next turn as a system reminder.
Demonstrates the v2 `PostToolUse` event + `additionalContext`
injection.

## Install

```sh
mkdir -p ~/.ignis/hooks
cp -R examples/hooks/auto-test ~/.ignis/hooks/auto-test
chmod +x ~/.ignis/hooks/auto-test/run.sh
```

Requires `jq` on `PATH`.

## Wire in `~/.ignis/hooks.json`

```json
{
  "hooks": {
    "PostToolUse": [
      {
        "command": "~/.ignis/hooks/auto-test/run.sh",
        "matcher": "Write|Edit",
        "timeout_ms": 120000
      }
    ]
  }
}
```

The `matcher` `Write|Edit` confines the hook to file-modifying tools —
it doesn't run after `Read`, `Grep`, or `Bash`. `timeout_ms` is 2
minutes because `cargo test --workspace` can take a while on cold
incremental builds.

Reload without restarting: type `/hooks reload`.

## How it shows up to the model

After every Write/Edit, the model's next turn sees:

```
<system-reminder>
hook PostToolUse (run): cargo test --workspace -q: PASSED
</system-reminder>
```

or on failure, the last 20 lines of the test output. The model treats
this as system-level feedback — typically reacting to a failure by
proposing a fix without the user asking.

## Tuning

The script tails the last 20 lines. For larger output bump that or
filter further (e.g., `grep -E "test result|FAILED|error\[E"`). On a
slow test suite, set `timeout_ms` generously — a timeout causes a
soft failure (the original Write/Edit still completes; no test
feedback is injected).
