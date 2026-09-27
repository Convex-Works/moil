# /// script
# requires-python = ">=3.12"
# dependencies = []
# ///
"""A test bundle that does whatever its params say, one step at a time.

params: {"steps": [step, ...]}. Each step is an object with one of:

  {"emit": {...}}                print the object as one JSON line
  {"print": "text"}              print a raw line
  {"raw": [byte, ...]}           write raw bytes to stdout
  {"stderr": "text", "times": n} write a line to stderr, n times
  {"write": "name", "text": "."} write out/<name>
  {"fill": "name", "bytes": n}   write n bytes to out/<name>
  {"symlink": "path", "to": "."} make a symbolic link at <work dir>/<path>,
                                 replacing an empty directory there
  {"fifo": "path"}               make a FIFO at <work dir>/<path>
  {"input": "name"}              send an input's contents as data
  {"asset": "name"}              send an asset's contents as data
  {"context": true}              send the job context as data
  {"environ": true}              send the environment as data
  {"prefix": true}               send the environment's sys.prefix as data
  {"spawn": true}                start a grandchild that ignores SIGTERM,
                                 then send its pid, the script's and uv's
  {"on_sigterm": "message",      on SIGTERM, log the message, wait
   "linger": seconds}            `linger` seconds (default 0) and exit 0
  {"until_orphaned": true}       print until nobody reads stdout any more
  {"ignore_sigterm": true}       ignore SIGTERM
  {"sleep": seconds}
  {"exit": code}
  {"kill_self": signal}
"""

import json
import os
import signal
import subprocess
import sys
import time


def emit(event):
    print(json.dumps(event), flush=True)


def data(payload):
    emit({"type": "data", "payload": payload})


def run(step, job):
    if "emit" in step:
        emit(step["emit"])
    elif "print" in step:
        print(step["print"], flush=True)
    elif "raw" in step:
        sys.stdout.buffer.write(bytes(step["raw"]))
        sys.stdout.buffer.flush()
    elif "stderr" in step:
        for _ in range(step.get("times", 1)):
            print(step["stderr"], file=sys.stderr, flush=True)
    elif "write" in step:
        with open(os.path.join(job["output_dir"], step["write"]), "w") as f:
            f.write(step["text"])
    elif "fill" in step:
        with open(os.path.join(job["output_dir"], step["fill"]), "wb") as f:
            f.write(b"\0" * step["bytes"])
    elif "symlink" in step:
        path = os.path.join(job["work_dir"], step["symlink"])
        if os.path.isdir(path):
            os.rmdir(path)
        os.symlink(step["to"], path)
    elif "fifo" in step:
        os.mkfifo(os.path.join(job["work_dir"], step["fifo"]))
    elif "input" in step:
        with open(job["inputs"][step["input"]], "rb") as f:
            data({"input": f.read().decode()})
    elif "asset" in step:
        with open(job["assets"][step["asset"]], "rb") as f:
            data({"asset": f.read().decode()})
    elif "context" in step:
        data({"context": job})
    elif "environ" in step:
        data({"environ": dict(os.environ)})
    elif "prefix" in step:
        data({"prefix": sys.prefix})
    elif "spawn" in step:
        stubborn = (
            "import signal, time\n"
            "signal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
            "print('ready', flush=True)\n"
            "time.sleep(600)\n"
        )
        child = subprocess.Popen([sys.executable, "-c", stubborn], stdout=subprocess.PIPE)
        child.stdout.readline()
        data({"pid": child.pid, "script": os.getpid(), "uv": os.getppid()})
    elif "on_sigterm" in step:
        message, linger = step["on_sigterm"], step.get("linger", 0)
        line = (json.dumps({"type": "log", "message": message}) + "\n").encode()

        def handle(signum, frame):
            # Not print(): the signal may arrive while the main code is
            # inside print(), and Python's stdout isn't reentrant.
            os.write(sys.stdout.fileno(), line)
            time.sleep(linger)
            sys.exit(0)

        signal.signal(signal.SIGTERM, handle)
    elif "ignore_sigterm" in step:
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
    elif "until_orphaned" in step:
        try:
            while True:
                print(".", flush=True)
                time.sleep(0.02)
        except BrokenPipeError:
            os._exit(0)
    elif "sleep" in step:
        time.sleep(step["sleep"])
    elif "exit" in step:
        sys.exit(step["exit"])
    elif "kill_self" in step:
        os.kill(os.getpid(), step["kill_self"])
    else:
        raise ValueError(f"unknown step {step!r}")


job = json.loads(sys.stdin.read())
for step in (job["params"] or {}).get("steps", []):
    run(step, job)
