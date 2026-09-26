# /// script
# requires-python = ">=3.12"
# dependencies = ["idna==3.20"]
# ///
"""A test bundle with a real dependency: encodes the name in its params."""

import json
import sys

import idna

job = json.load(sys.stdin)
encoded = idna.encode(job["params"]["name"]).decode()
print(json.dumps({"type": "result", "meta": {"encoded": encoded}}), flush=True)
