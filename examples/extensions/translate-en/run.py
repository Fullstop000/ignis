#!/usr/bin/env python3
"""Reference translator hook for ignis.

Reads a JSON envelope from stdin, routes on `hook_event_name`, calls
Anthropic Haiku to translate the payload, and writes the rewritten
envelope back on stdout.

Env vars:
  ANTHROPIC_API_KEY   - required.
  IGNIS_TRANSLATE_FROM (default: zh) - source for UserPromptSubmit.
  IGNIS_TRANSLATE_TO   (default: en) - target for UserPromptSubmit.
                                       Display direction reverses both.
  ANTHROPIC_MODEL     - override (default: claude-haiku-4-5).

Code fences and inline backticks are masked with §§CODE0§§-style
sentinels before sending and restored verbatim after — translators
mangle code if you let them touch it.
"""

from __future__ import annotations

import json
import os
import re
import sys
import urllib.request


MODEL = os.environ.get("ANTHROPIC_MODEL", "claude-haiku-4-5")
API_URL = "https://api.anthropic.com/v1/messages"
FENCED = re.compile(r"```[\s\S]*?```")
INLINE = re.compile(r"`[^`\n]+`")


def mask(text: str) -> tuple[str, list[str]]:
    """Replace code blocks with sentinels and return (masked, snippets)."""
    snippets: list[str] = []

    def grab(match: re.Match[str]) -> str:
        idx = len(snippets)
        snippets.append(match.group(0))
        return f"§§CODE{idx}§§"

    masked = FENCED.sub(grab, text)
    masked = INLINE.sub(grab, masked)
    return masked, snippets


def unmask(text: str, snippets: list[str]) -> str:
    for idx, snippet in enumerate(snippets):
        text = text.replace(f"§§CODE{idx}§§", snippet)
    return text


def translate(text: str, source: str, target: str) -> str:
    if not text.strip():
        return text
    api_key = os.environ.get("ANTHROPIC_API_KEY")
    if not api_key:
        # Fail soft: empty stdout → ignis treats as pass-through.
        print("translate-en: ANTHROPIC_API_KEY unset; pass-through.", file=sys.stderr)
        return text
    body = {
        "model": MODEL,
        "max_tokens": 4096,
        "system": (
            f"Translate the user message from {source} to {target}. "
            "Preserve markdown structure. Do not translate the §§CODEn§§ "
            "sentinels. Output only the translation, no commentary."
        ),
        "messages": [{"role": "user", "content": text}],
    }
    req = urllib.request.Request(
        API_URL,
        data=json.dumps(body).encode("utf-8"),
        headers={
            "x-api-key": api_key,
            "anthropic-version": "2023-06-01",
            "content-type": "application/json",
        },
    )
    with urllib.request.urlopen(req, timeout=20) as resp:
        payload = json.loads(resp.read().decode("utf-8"))
    blocks = payload.get("content", [])
    for block in blocks:
        if block.get("type") == "text":
            return block.get("text", text)
    return text


def main() -> int:
    raw = sys.stdin.read()
    if not raw.strip():
        return 0
    try:
        envelope = json.loads(raw)
    except json.JSONDecodeError as exc:
        print(f"translate-en: bad envelope JSON: {exc}", file=sys.stderr)
        return 1
    event = envelope.get("hook_event_name")
    if event == "UserPromptSubmit":
        text = envelope.get("prompt", "")
        source = os.environ.get("IGNIS_TRANSLATE_FROM", "zh")
        target = os.environ.get("IGNIS_TRANSLATE_TO", "en")
        field = "updatedInput"
    elif event == "AssistantMessageRender":
        text = envelope.get("content", "")
        # Reverse the pair for display.
        source = os.environ.get("IGNIS_TRANSLATE_TO", "en")
        target = os.environ.get("IGNIS_TRANSLATE_FROM", "zh")
        field = "updatedOutput"
    else:
        # Unknown event — pass-through (exit 0 + empty stdout).
        return 0
    masked, snippets = mask(text)
    translated = translate(masked, source, target)
    restored = unmask(translated, snippets)
    json.dump(
        {
            "hookSpecificOutput": {
                "hookEventName": event,
                field: restored,
            }
        },
        sys.stdout,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
