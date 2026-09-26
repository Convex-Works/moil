# /// script
# requires-python = ">=3.12"
# dependencies = ["idna==3.20"]
# ///
"""Grüße, 世界: CRLF line endings and non-ASCII text hash as raw bytes."""

import json
import sys

import idna

job = json.load(sys.stdin)
print(json.dumps({"v": 1, "type": "result", "meta": {"host": idna.encode("bücher.example").decode()}}), flush=True)
