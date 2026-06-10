# translate-en — reference ignis hook

Bilingual prompt+response hook for [ignis](https://github.com/Fullstop000/ignis).
Reads the JSON envelope ignis sends on stdin, calls Anthropic Haiku to
translate the payload, and writes the rewrite back on stdout. The same
script handles both events — it routes on `hook_event_name`:

- `UserPromptSubmit` — translates the user's input (default `zh -> en`)
  before the model sees it.
- `AssistantMessageRender` — translates the assistant's reply (default
  `en -> zh`) before the TUI renders it. **History keeps the original
  English** so prompt cache and replay stay exact.

## Why it exists

It's the worked example for the ignis hook protocol — the protocol is
the feature; translation is the proof. Distill, copy, adapt for your
own use case (PII scrub, prompt enhancement, telemetry sidecar, …).

> **Hooks are sandboxed on Linux (v2).** Default Landlock confinement
> blocks writes outside `$TMPDIR` and reads outside the hook's folder
> + system lib paths. Env vars are scrubbed to the universal
> allowlist; this hook declares the three it needs (`ANTHROPIC_API_KEY`,
> `IGNIS_TRANSLATE_FROM`, `IGNIS_TRANSLATE_TO`) in `env: [...]`.
> Sandboxing is a no-op on macOS / Windows / older Linux kernels.

## Install

```sh
mkdir -p ~/.ignis/hooks
cp -R examples/hooks/translate-en ~/.ignis/hooks/translate-en
chmod +x ~/.ignis/hooks/translate-en/run.py
```

Python 3.10+. No third-party deps — the script uses the stdlib
`urllib.request`. Set `ANTHROPIC_API_KEY` in your shell.

Wire it in `~/.ignis/hooks.json`. `env` declares the variables this
hook needs (everything else is scrubbed); `sandbox` is the default
Landlock confinement (`true` on Linux, no-op elsewhere) — included
here only so the example is self-documenting.

```json
{
  "hooks": {
    "UserPromptSubmit": [
      {
        "command": "~/.ignis/hooks/translate-en/run.py",
        "env": ["ANTHROPIC_API_KEY", "IGNIS_TRANSLATE_FROM", "IGNIS_TRANSLATE_TO"],
        "sandbox": true,
        "timeout_ms": 30000
      }
    ],
    "AssistantMessageRender": [
      {
        "command": "~/.ignis/hooks/translate-en/run.py",
        "env": ["ANTHROPIC_API_KEY", "IGNIS_TRANSLATE_FROM", "IGNIS_TRANSLATE_TO"],
        "sandbox": true,
        "timeout_ms": 30000
      }
    ]
  }
}
```

Reload without restarting ignis: type `/hooks reload`.

## Configure

| Env var | Default | Effect |
|---|---|---|
| `ANTHROPIC_API_KEY` | — | Required. Missing → pass-through (no rewrite). |
| `IGNIS_TRANSLATE_FROM` | `zh` | Source language for `UserPromptSubmit`. |
| `IGNIS_TRANSLATE_TO` | `en` | Target language for `UserPromptSubmit`. |
| `ANTHROPIC_MODEL` | `claude-haiku-4-5` | Override the model. |

The display direction is the reverse pair: if you translate input
`zh -> en`, output translates `en -> zh`. Set both env vars to whatever
language pair you need.

## How it handles code

Code fences (triple-backtick) and inline backticks are masked with
`§§CODE0§§`-style sentinels before sending to the API, then restored
after. The system prompt tells the model not to translate the
sentinels.

## Run the tests

```sh
pip install pytest
pytest examples/hooks/translate-en/
```

The tests are **not** part of `cargo test --workspace`. They mock
`urllib.request.urlopen` so they never touch the real API.
