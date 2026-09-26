# /// script
# requires-python = ">=3.12"
# dependencies = []
# ///
"""Smallest useful bundle: echoes its params back as the result."""

import json
import sys

job = json.load(sys.stdin)
print(json.dumps({"v": 1, "type": "result", "meta": job["params"]}), flush=True)
