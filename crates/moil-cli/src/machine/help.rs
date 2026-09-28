//! What the owner's commands' `--help` says besides their doc comments:
//! examples, and the hook contract in full (spec/machine.md §2).

pub const STATUS: &str = "\
Examples:
  moil status
  moil status --json | jq .problems
  moil status --watch --json      # a line of JSON whenever something changes";

pub const PAUSE: &str = "\
Examples:
  moil pause
  moil pause && moil stop         # take no more jobs, and stop this one";

pub const RESUME: &str = "\
Example:
  moil resume";

pub const STOP: &str = "\
Examples:
  moil stop
  moil stop --json                # {\"stopping\": \"<job ID> attempt <n>\"}, or {}";

pub const PAIR: &str = "\
Examples:
  moil pair https://klisi.convex.works/moil
  moil pair klisi.convex.works    # a service the home knows, such as a clone from git
  moil pair https://klisi.convex.works/moil --name \"Ada's MacBook Pro\" --no-browser --json";

pub const UNPAIR: &str = "\
Example:
  moil unpair klisi.convex.works";

pub const SERVICES: &str = "\
Examples:
  moil services
  moil services --json | jq '.[] | select(.paired) | .id'";

pub const REVIEW: &str = "\
Examples:
  moil review klisi
  moil review klisi 42d30e3fb23030627d993330b1e2917a0ac2f4abef567fc6e82328d8acedeae6
  moil review klisi 42d30e3fb23030627d993330b1e2917a0ac2f4abef567fc6e82328d8acedeae6 --json";

pub const APPROVE: &str = "\
Examples:
  moil approve klisi 42d30e3fb23030627d993330b1e2917a0ac2f4abef567fc6e82328d8acedeae6
  moil approve klisi 42d30e3fb23030627d993330b1e2917a0ac2f4abef567fc6e82328d8acedeae6 --yes --json";

pub const REVOKE: &str = "\
Example:
  moil revoke klisi 42d30e3fb23030627d993330b1e2917a0ac2f4abef567fc6e82328d8acedeae6";

pub const LOGS: &str = "\
Examples:
  moil logs -n 50                 # the agent's last 50 lines
  moil logs -f                    # the agent's log, as it is written
  moil logs klisi transcribe -f   # klisi's hook for transcribe";

pub const HOME: &str = "\
Examples:
  moil home
  cd \"$(moil home --json | jq -r .home)\"";

pub const LOGIN_ITEM: &str = "\
Examples:
  moil login-item
  moil login-item on";

pub const QUIT: &str = "\
Example:
  moil quit";

pub const AGENT: &str = "\
Examples:
  moil agent
  MOIL_HOME=/srv/moil moil agent  # a home of its own, for a machine without the app";

/// What `moil hooks --help` explains: the whole hook contract, so that
/// whoever reads only that can write a hook that works.
pub const HOOKS_ABOUT: &str = r#"List the hooks, and whether moil can run each

Shows each hook's service and bundle, its path and log, and whether moil can run it, then the problems moil finds with hooks. With --json, each hook as `moil status --json` lists it, with its service.

A hook is an executable of yours that moil runs after an attempt of a bundle succeeds on this machine, with the attempt's outputs: to keep a transcript in your notes, say, or to tell you it's done. The file is the whole setup: there is nothing to register, and removing the file turns the hook off.

WHERE IT LIVES

  <home>/services/<service ID>/hooks/<bundle name>

such as ~/.moil/services/klisi.convex.works/hooks/transcribe, for the attempts of klisi's bundle named transcribe. `moil home` prints the home, and `moil services` the services' IDs. A hook is named exactly after its bundle; files named otherwise, and hidden files, are never run.

WHEN IT RUNS

moil runs a hook only after it has queued the attempt's outcome for the service, so a hook never delays or changes what the service sees, and nothing about hooks reaches the service. Hooks run one at a time, in the order their attempts ended. The machine goes on taking jobs while a hook runs, and hooks run while the machine is paused too. Attempts that fail or are stopped run no hook.

moil executes the file itself. It must be a file you can execute (chmod +x), or a symbolic link to one, that starts with a #! line, such as #!/bin/sh, or is a program: moil doesn't guess at a shell. If what is at the hook's path isn't such a file, moil doesn't run it, writes why in the hook's log, and reports it as a problem in `moil status` and here.

WHAT IT GETS

On stdin, the hook context: one line of JSON, then end-of-file. For example (spread over lines here):

  {
    "v": 1,
    "service": {
      "id": "klisi.convex.works",
      "name": "klisi",
      "base_url": "https://klisi.convex.works/moil"
    },
    "bundle": {
      "name": "transcribe",
      "version": "0.3.0",
      "hash": "42d30e3fb23030627d993330b1e2917a0ac2f4abef567fc6e82328d8acedeae6"
    },
    "job_id": "recording-d07aa27961e839c14bb0a099bd971df0",
    "attempt": 1,
    "title": "Weekly sync",
    "params": {"language": "en"},
    "meta": {"speakers": 3},
    "ended_at": "2026-09-27T22:41:07Z",
    "output_dir": "/Users/ada/.moil/.machine/hooks/7/out",
    "outputs": {
      "transcript.txt": "/Users/ada/.moil/.machine/hooks/7/out/transcript.txt",
      "transcript.vtt": "/Users/ada/.moil/.machine/hooks/7/out/transcript.vtt"
    }
  }

  v               The hook contract's version: 1.
  service         The service's ID, name and base URL.
  bundle          The approved bundle that ran: its name, version and hash.
  job_id, attempt The job's ID, and which attempt at it this was.
  title           The job's title, if the service gave one.
  params          The job's params, as the bundle's script got them.
  meta            The script's result.meta, if it sent one.
  ended_at        When the attempt ended, in UTC.
  output_dir      The absolute path of a directory holding the outputs and
                  nothing else.
  outputs         Each output the attempt uploaded, by name, with its
                  absolute path.

The hook runs:

- In an empty directory, which is also its TMPDIR, and which moil removes afterwards. The outputs are in output_dir, which the hook may change as it likes; moil deletes it once the hook exits, so a hook that wants to keep an output copies or moves it elsewhere. The outputs' names come from the service, so moil doesn't run the hook among them, where a file such as uv.toml could change what the hook runs.
- With only these environment variables: HOME, USER, LANG, MOIL_HOME, TMPDIR, and PATH, which is <home>/.machine/bin:/usr/bin:/bin:/usr/sbin:/sbin. <home>/.machine/bin holds the moil and uv this machine runs, so a hook starting `#!/usr/bin/env -S uv run --script` can declare its Python dependencies inline, as a bundle does. A hook names anything else it needs itself, a program by its absolute path or a PATH of its own, so that the file alone says what it depends on.
- In a process group of its own, for at most an hour. After that moil stops it as it stops a job's script: SIGTERM to the group, then SIGKILL 10 seconds later.

WHAT IT LEAVES

What the hook prints on stdout and stderr is appended to its log, <home>/services/<service ID>/logs/<bundle name>.log, between two lines moil writes:

  == 2026-09-27T22:41:08Z recording-d07aa27961e839c14bb0a099bd971df0 attempt 1
  …
  == exited 0 after 42s

The closing line is one of:

  == exited <status> after <duration>
  == killed by signal <number> after <duration>
  == stopped after an hour
  == stopped: moil quit
  == stopped: moil stopped unexpectedly      (written when moil next starts)
  == not run: <reason>

Once a log passes 1 MiB, moil renames it to <bundle name>.log.1, replacing any older one, and starts a new log. `moil logs <service> <bundle> -f` follows it.

moil records the hook's exit status, and doesn't retry a hook that failed. But a hook that didn't get to exit, because moil quit or crashed, runs again from the start, with the same outputs, when moil next starts, unless its file is gone by then. So a hook runs at least once for each attempt that succeeded, and it should cope with running twice: write to a path named by the job, say, rather than appending.

TRUST

A hook is your own code. moil doesn't ask anyone to approve it and doesn't sandbox it: it can do whatever you can. Whoever can change a service's folder, or push to the repository it is cloned from, can add hooks.

Some of what a hook gets comes from the service: title, params, meta and job_id, and the contents of the outputs, which the approved bundle made from the service's inputs. Treat them as untrusted. Don't build a file path from title without checking it, for example, and don't hand the outputs to a program that acts on instructions it finds in them, such as an LLM that can run tools. The context carries no token, and no URL other than the service's base URL.

AN EXAMPLE

~/.moil/services/klisi.convex.works/hooks/transcribe, made executable with chmod +x, keeps each transcript in ~/Transcripts:

  #!/usr/bin/env -S uv run --script
  # /// script
  # requires-python = ">=3.12"
  # dependencies = []
  # ///
  import json, pathlib, shutil, sys

  context = json.load(sys.stdin)
  keep = pathlib.Path.home() / "Transcripts"
  keep.mkdir(exist_ok=True)
  # Job IDs are letters, digits, dots, dashes and underscores (not the
  # title, which could be anything), so the name is safe; and a second run
  # writes the same file again.
  shutil.copy(context["outputs"]["transcript.txt"], keep / f"{context['job_id']}.txt")
  print("kept", context["job_id"])"#;

pub const HOOKS: &str = "\
Examples:
  moil hooks
  moil hooks --json | jq '.[] | select(.runnable | not) | .path'";
