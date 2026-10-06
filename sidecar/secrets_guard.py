"""Refuse to write anything that looks like a credential.

Every file the sidecar writes goes through `write_lines`, which checks the
whole payload first. The Rust side has the same rule in `src/artifacts.rs`.
A match raises and nothing is written; the error names the kind of secret,
never its value.
"""

import os
import re

SECRET_ENV_VARS = [
    "TYPESAFE_API_KEY",
    "LAYA_API_KEY",
    "IMPOSSIBL_API_KEY",
    "REDDIT_CLIENT_ID",
    "REDDIT_CLIENT_SECRET",
]

PATTERNS = {
    "openai-style key": re.compile(r"\bsk-[A-Za-z0-9_-]{16,}"),
    "stripe-style key": re.compile(r"\b[sr]k_(live|test)_[A-Za-z0-9]{16,}"),
    "github token": re.compile(r"\bgh[pousr]_[A-Za-z0-9]{30,}"),
    "slack token": re.compile(r"\bxox[abprs]-[A-Za-z0-9-]{10,}"),
    "aws access key id": re.compile(r"\bAKIA[0-9A-Z]{16}\b"),
    "bearer token": re.compile(r"(?i)\bbearer\s+[A-Za-z0-9._~+/-]{20,}"),
    "private key block": re.compile(r"-----BEGIN [A-Z ]*PRIVATE KEY-----"),
}


def find_secret(text):
    """Return the kind of secret found in `text`, or None."""
    for name in SECRET_ENV_VARS:
        value = os.environ.get(name, "")
        if len(value) >= 8 and value in text:
            return f"value of ${name}"
    for kind, pattern in PATTERNS.items():
        if pattern.search(text):
            return kind
    return None


def write_lines(path, lines, mode="w"):
    """Write lines (strings without newlines) after checking for secrets."""
    payload = "".join(line + "\n" for line in lines)
    kind = find_secret(payload)
    if kind:
        raise RuntimeError(f"refusing to write {path}: it contains a {kind}")
    with open(path, mode, newline="\n", encoding="utf-8") as f:
        f.write(payload)
