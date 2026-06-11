#!/usr/bin/env python3
"""Regression test for secret redaction in bundle_traces + generate_report.

Not wired into cargo CI (these are Python adapter scripts); run by hand:

    python3 scripts/test_redact.py    # exits non-zero on any failure

Guards the value-agnostic patterns added after a crack-7z-hash trial dumped
~/.ignis/config.toml into its agent log, leaking a bare-UUID provider token
that the prefix-shaped patterns (sk-/ghp_/BSA/...) did not catch.
"""

import importlib.util
import sys
from pathlib import Path

HERE = Path(__file__).parent


def _load(name: str, filename: str):
    spec = importlib.util.spec_from_file_location(name, HERE / filename)
    mod = importlib.util.module_from_spec(spec)
    sys.modules[name] = mod  # @dataclass in generate_report needs the module registered
    spec.loader.exec_module(mod)
    return mod


bt = _load("bundle_traces", "bundle_traces.py")
gr = _load("generate_report", "generate_report.py")

# A bare-UUID token with no recognizable prefix - invisible to sk-/ghp_/BSA/... .
TOK = "00000000-1111-2222-3333-444444444444"
# A legitimate identifier of the SAME shape that must NOT be redacted.
SESSION = "session-1781000000-00000000abcd1234"
# Built at runtime so no secret-shaped LITERAL lands in source (which would trip
# the /ship secret scan, real value or not). They still match the regexes once
# assembled.
SK_KEY = "sk-" + "a1b2c3d4" * 4
BSA_KEY = "BSA" + "z9y8x7w6" * 4

must_redact = {
    "config api_key dump": '[providers.ark-coding]\napi_key = "%s"\nmodels = []' % TOK,
    "bearer header": "Authorization: Bearer %s" % TOK,
    "prefixed key (sk-)": "DEEPSEEK_API_KEY=%s" % SK_KEY,
    "brave key (BSA)": "key=%s" % BSA_KEY,
}
secret_of = {
    "config api_key dump": TOK,
    "bearer header": TOK,
    "prefixed key (sk-)": SK_KEY,
    "brave key (BSA)": BSA_KEY,
}

fails: list[str] = []

for label, text in must_redact.items():
    secret = secret_of[label]
    s = gr._redact_secrets(text)
    b = bt._redact_bytes(text.encode()).decode("utf-8", "replace")
    if secret in s:
        fails.append("generate_report did not redact %r: %r" % (label, s))
    if secret in b:
        fails.append("bundle_traces did not redact %r: %r" % (label, b))

# Negative: a bare session id (not in an api_key/Bearer context) must survive -
# the value-agnostic patterns must not nuke legitimate UUID-shaped identifiers.
kept = gr._redact_secrets("Session: %s" % SESSION)
if SESSION not in kept:
    fails.append("over-redacted a legitimate session id: %r" % kept)

if fails:
    print("FAIL")
    for f in fails:
        print("  -", f)
    sys.exit(1)
print("ok: all redaction cases pass")
