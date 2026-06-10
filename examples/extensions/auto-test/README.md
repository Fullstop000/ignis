# auto-test — PostToolUse test-after-edit hook

Runs `cargo test --workspace -q` after every `create_file` or
`edit_file` and injects PASS / FAIL into the model's next turn as a
system reminder.
Demonstrates the v2 `PostToolUse` event + `additionalContext`
injection.

## Install

```sh
mkdir -p ~/.ignis/extensions
cp -R examples/extensions/auto-test ~/.ignis/extensions/auto-test
chmod +x ~/.ignis/extensions/auto-test/run.sh
```

Requires `jq` on `PATH`.

## Wire in `~/.ignis/extensions.json`

```json
{
  "extensions": {
    "PostToolUse": [
      {
        "command": "~/.ignis/extensions/auto-test/run.sh",
        "matcher": "create_file|edit_file",
        "sandbox": false,
        "timeout_ms": 120000
      }
    ]
  }
}
```

`"sandbox": false` is **required** here: extensions are sandboxed by
default (reads confined to system libs + `/tmp`, writes to `/tmp`),
but `cargo test --workspace` must read your whole project tree and
write `target/`. The default sandbox would block it. This is the
intended escape hatch for extensions that legitimately need broad
filesystem access — see `docs/usage/extensions.md`.

The `matcher` `create_file|edit_file` confines the extension to
file-modifying tools — it doesn't run after `read_file`, `grep`, or
`bash` (ignis tool names are lowercase snake_case). `timeout_ms`
is 2 minutes because `cargo test --workspace` can take a while on cold
incremental builds.

Reload without restarting: type `/extensions reload`.

## How it shows up to the model

After every create_file/edit_file, the model's next turn sees:

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
soft failure (the original create_file/edit_file still completes; no test
feedback is injected).
