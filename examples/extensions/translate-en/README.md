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

> **Extensions are sandboxed by default.** A filesystem sandbox (Linux
> Landlock / macOS Seatbelt) blocks writes outside `/tmp` and reads
> outside the extension's folder + system lib paths. Env vars are
> scrubbed to the universal allowlist; this extension declares the three
> it needs (`ANTHROPIC_API_KEY`, `IGNIS_TRANSLATE_FROM`,
> `IGNIS_TRANSLATE_TO`) in `env: [...]`. Sandboxing is a no-op on Windows
> and older Linux kernels.

## Install

```sh
mkdir -p ~/.ignis/extensions
cp -R examples/extensions/translate-en ~/.ignis/extensions/translate-en
chmod +x ~/.ignis/extensions/translate-en/run.py
```

Python 3.10+. No third-party deps — the script uses the stdlib
`urllib.request`. Set `ANTHROPIC_API_KEY` in your shell.

Wire it in `~/.ignis/extensions.json`. `env` declares the variables this
extension needs (everything else is scrubbed); `sandbox` is the default
filesystem confinement (Linux Landlock / macOS Seatbelt) — included here
only so the example is self-documenting.

```json
{
  "extensions": {
    "UserPromptSubmit": [
      {
        "command": "~/.ignis/extensions/translate-en/run.py",
        "env": ["ANTHROPIC_API_KEY", "IGNIS_TRANSLATE_FROM", "IGNIS_TRANSLATE_TO"],
        "sandbox": true,
        "timeout_ms": 30000
      }
    ],
    "AssistantMessageRender": [
      {
        "command": "~/.ignis/extensions/translate-en/run.py",
        "env": ["ANTHROPIC_API_KEY", "IGNIS_TRANSLATE_FROM", "IGNIS_TRANSLATE_TO"],
        "sandbox": true,
        "timeout_ms": 30000
      }
    ]
  }
}
```

Reload without restarting ignis: type `/extensions reload`.

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
pytest examples/extensions/translate-en/
```

The tests are **not** part of `cargo test --workspace`. They mock
`urllib.request.urlopen` so they never touch the real API.
