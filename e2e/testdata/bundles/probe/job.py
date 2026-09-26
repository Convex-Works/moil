# /// script
# requires-python = ">=3.12"
# dependencies = []
# ///
"""probe: the end-to-end tests' bundle. It does what its params say.

params: {"attempts": [[step, ...], ...], "record": dir}. Attempt n takes the
steps of the n-th list; the last list repeats for later attempts. Each step
is an object with one of these keys:

  {"progress": 0.5, "message": "…"}   a progress event
  {"data": <any JSON>}                 a data event carrying it
  {"count": [first, last]}             data events {"n": first} … {"n": last}
  {"junk": "text"}                     a line on stdout that isn't JSON
  {"stderr": "text"}                   a line on stderr
  {"copy": "input", "to": "output"}    copy an input file to an output
  {"reveal": true}                     send what the script can see: its
                                       environment and the job context
  {"spawn": true}                      start a grandchild that ignores
                                       SIGTERM, then send the pids of uv,
                                       the script and the grandchild
  {"wait": "path"}                     wait until the file exists
  {"touch": "path"}                    create the file
  {"sleep": seconds}
  {"fail": "message", "retryable": b}  an error event
  {"exit": code}                       exit now
  {"result": {"files": […], "meta": …}}

The script records its process group in "record", so that the tests can
make sure nothing it started outlives the agent that ran it. Nothing it
starts runs for more than a minute, even if the tests never let it finish.
"""

import json
import os
import subprocess
import sys
import time

GIVE_UP_AFTER = 60  # seconds


def emit(type, **fields):
    print(json.dumps({"v": 1, "type": type, **fields}), flush=True)


def step(action, job):
    if "progress" in action:
        emit("progress", fraction=action["progress"], message=action["message"])
    elif "data" in action:
        emit("data", payload=action["data"])
    elif "count" in action:
        first, last = action["count"]
        for n in range(first, last + 1):
            emit("data", payload={"n": n})
    elif "junk" in action:
        print(action["junk"], flush=True)
    elif "stderr" in action:
        print(action["stderr"], file=sys.stderr, flush=True)
    elif "copy" in action:
        with open(job["inputs"][action["copy"]], "rb") as f:
            data = f.read()
        with open(os.path.join(job["output_dir"], action["to"]), "wb") as f:
            f.write(data)
    elif "reveal" in action:
        emit("data", payload={"environ": dict(os.environ), "context": job})
    elif "spawn" in action:
        stubborn = (
            "import signal, time\n"
            "signal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
            "print('ready', flush=True)\n"
            f"time.sleep({GIVE_UP_AFTER})\n"
        )
        child = subprocess.Popen([sys.executable, "-c", stubborn], stdout=subprocess.PIPE)
        child.stdout.readline()
        pids = {"uv": os.getppid(), "script": os.getpid(), "grandchild": child.pid}
        emit("data", payload={"pids": pids})
    elif "wait" in action:
        deadline = time.monotonic() + GIVE_UP_AFTER
        while not os.path.exists(action["wait"]):
            if time.monotonic() > deadline:
                sys.exit(f"gave up waiting for {action['wait']}")
            time.sleep(0.02)
    elif "touch" in action:
        open(action["touch"], "w").close()
    elif "sleep" in action:
        time.sleep(action["sleep"])
    elif "fail" in action:
        emit("error", message=action["fail"], retryable=action.get("retryable", False))
    elif "exit" in action:
        sys.exit(action["exit"])
    elif "result" in action:
        emit("result", **action["result"])
    else:
        raise ValueError(f"unknown step {action!r}")


def main(job):
    params = job["params"]
    if params.get("record"):
        path = os.path.join(params["record"], f"{job['job_id']}.{job['attempt']}")
        with open(path, "w") as f:
            f.write(str(os.getpgid(0)))
    attempts = params["attempts"]
    for action in attempts[min(job["attempt"], len(attempts)) - 1]:
        step(action, job)


if __name__ == "__main__":
    main(json.load(sys.stdin))
