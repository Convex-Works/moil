# /// script
# requires-python = ">=3.12"
# dependencies = []
# ///
"""echo: the example moil job bundle.

Reads a text file, sends each line back as it goes, and writes the text,
shouted if asked, to an output file. It uses every part of the script
contract (moil spec, section 8), so it's a good place to start a bundle
of your own. Try it:

    moil run examples/bundles/echo --input text.txt=README.md \\
        --output echo.txt --params '{"shout": true}'

Params, all optional:

    text        what to echo when the job has no text.txt input
    shout       uppercase every line
    delay_ms    pause after each line, to watch progress or cancel
    fail        fail with this message instead of echoing
    retryable   whether that failure is worth another attempt

A bundle is three files: this script, job.py.lock (from
`uv lock --script job.py`, pinning every dependency by hash) and
manifest.json. Machine owners approve the exact bundle by hash, so any
change, even to a comment, needs a new approval. Model weights and other
large files go in the manifest's "assets": the runtime downloads and
verifies them, and the script finds them in job["assets"].
"""

import json
import sys
import time


def emit(type, **fields):
    """Sends one event to the runtime: a JSON object on its own line.

    Anything else the script prints becomes a log line, so a stray print()
    can't break a job. stderr is kept for error reports but never sent.
    """
    print(json.dumps({"v": 1, "type": type, **fields}), flush=True)


def main(job):
    # The job context arrives on stdin. Besides params it says where the
    # inputs were downloaded, which outputs the service accepts, and where
    # to write them. The script never sees URLs or credentials.
    params = job["params"] or {}

    # An error event fails the attempt with the script's own message, and
    # says whether another attempt (maybe on another machine) could work.
    if "fail" in params:
        emit("error", message=params["fail"], retryable=params.get("retryable", False))
        return 1

    if "text.txt" in job["inputs"]:
        with open(job["inputs"]["text.txt"], encoding="utf-8") as f:
            text = f.read()
    else:
        text = params.get("text", "hello from moil\n")
    lines = text.splitlines()
    emit("log", level="info", message=f"echoing {len(lines)} lines")

    echoed = []
    for number, line in enumerate(lines, start=1):
        if params.get("shout"):
            line = line.upper()
        echoed.append(line)
        # data events carry whatever the service wants, passed on untouched.
        emit("data", payload={"line": number, "text": line})
        # progress drives the progress bars on the machine and the service.
        emit("progress", fraction=number / len(lines), message=f"line {number} of {len(lines)}")
        time.sleep(params.get("delay_ms", 0) / 1000)

    # Outputs go in output_dir under names the service declared; naming an
    # undeclared one in the result fails the attempt. So echo.txt is only
    # written when the service asked for it.
    files = []
    if "echo.txt" in job["outputs"]:
        with open(f"{job['output_dir']}/echo.txt", "w", encoding="utf-8") as f:
            f.write("".join(line + "\n" for line in echoed))
        files.append("echo.txt")

    # The result ends a successful job: the runtime uploads the listed files
    # and hands meta to the service. Exiting without a result is a failure.
    emit("result", files=files, meta={"lines": len(lines), "shouted": bool(params.get("shout"))})
    return 0


if __name__ == "__main__":
    sys.exit(main(json.load(sys.stdin)))
