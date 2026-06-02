"""pytest for the reference translator hook.

Not part of cargo test — run with `pytest examples/hooks/translate-en/`.
Mocks `urllib.request.urlopen` so the test never touches the real API.
"""

from __future__ import annotations

import importlib
import importlib.util
import io
import json
import os
import sys
from pathlib import Path
from unittest.mock import patch

HERE = Path(__file__).parent
SPEC = importlib.util.spec_from_file_location("translate_run", HERE / "run.py")
assert SPEC and SPEC.loader
RUN = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUN)


class _Resp(io.BytesIO):
    def __enter__(self) -> "_Resp":  # type: ignore[override]
        return self

    def __exit__(self, *_: object) -> None:
        self.close()


def _fake_response(text: str) -> _Resp:
    payload = json.dumps({"content": [{"type": "text", "text": text}]}).encode("utf-8")
    return _Resp(payload)


def test_user_prompt_submit_routes_and_translates(monkeypatch):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "fake")
    monkeypatch.setattr(sys, "stdin", io.StringIO(json.dumps({
        "hook_event_name": "UserPromptSubmit",
        "session_id": "s1",
        "cwd": "/tmp",
        "prompt": "你好",
    })))
    out = io.StringIO()
    monkeypatch.setattr(sys, "stdout", out)
    with patch("urllib.request.urlopen", return_value=_fake_response("hello")):
        rc = RUN.main()
    assert rc == 0
    body = json.loads(out.getvalue())
    assert body["hookSpecificOutput"]["hookEventName"] == "UserPromptSubmit"
    assert body["hookSpecificOutput"]["updatedInput"] == "hello"


def test_assistant_message_render_routes_to_updated_output(monkeypatch):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "fake")
    monkeypatch.setattr(sys, "stdin", io.StringIO(json.dumps({
        "hook_event_name": "AssistantMessageRender",
        "session_id": "s1",
        "cwd": "/tmp",
        "content": "hello world",
    })))
    out = io.StringIO()
    monkeypatch.setattr(sys, "stdout", out)
    with patch("urllib.request.urlopen", return_value=_fake_response("你好世界")):
        rc = RUN.main()
    assert rc == 0
    body = json.loads(out.getvalue())
    assert body["hookSpecificOutput"]["updatedOutput"] == "你好世界"


def test_sentinel_masking_protects_code_blocks():
    text = "Run `cargo build` then\n```python\nprint('x')\n```\nand done."
    masked, snippets = RUN.mask(text)
    assert "cargo build" not in masked
    assert "print('x')" not in masked
    assert masked.count("§§CODE") == 2
    restored = RUN.unmask(masked, snippets)
    assert restored == text


def test_unknown_event_is_passthrough(monkeypatch):
    monkeypatch.setattr(sys, "stdin", io.StringIO(json.dumps({
        "hook_event_name": "FuturePostToolUse",
        "session_id": "s",
        "cwd": "/tmp",
    })))
    out = io.StringIO()
    monkeypatch.setattr(sys, "stdout", out)
    rc = RUN.main()
    assert rc == 0
    assert out.getvalue() == ""  # empty stdout = pass-through


def test_missing_api_key_is_passthrough(monkeypatch):
    monkeypatch.delenv("ANTHROPIC_API_KEY", raising=False)
    # Pass a small text; mask/translate falls back to the original.
    out = RUN.translate("hi", "en", "zh")
    assert out == "hi"
