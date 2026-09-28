# The moil machine

This document is normative for moil's machine side: the tray app, the
`moil` command and the agent they run. [protocol.md](protocol.md) defines
what machines and services say to each other. This document defines what a
machine keeps on disk, the code it runs for its owner, and its command
line. None of it crosses the wire, so no service can observe any of it.

The key words MUST, SHOULD and MAY are used as in RFC 2119.

Three rules shape it:

- **The home is plain files.** What moil runs and acts on lives in one
  folder that the owner, a code editor or an LLM can read, and that the
  owner can keep in git.
- **The owner's code is kept apart from the services' code.** Bundles are
  the services' code: approved by hash, run by moil, and one day
  sandboxed. Hooks are the owner's: nobody approves them, and they run as
  the owner.
- **The command line does everything the app does**, including while the
  app runs, so that scripts and agents can drive moil.

- [1. The home](#1-the-home)
- [2. Hooks](#2-hooks)
- [3. The command line](#3-the-command-line)
- [4. The control socket](#4-the-control-socket)
- [5. The app](#5-the-app)
- [6. Coming from moil 0.1](#6-coming-from-moil-01)

## 1. The home

The home is `~/.moil`, or the directory that `$MOIL_HOME` or the `--home`
option names. moil creates it readable only by the owner.

```
~/.moil/
  README.md                          what this folder is; moil keeps it current
  .gitignore                         written by moil when missing
  settings.json                      this machine's preferences (§1.1)
  logs/moil.log                      the agent's log
  services/
    klisi.convex.works/              one folder per service, named by its ID (§1.2)
      service.json                   the service, and the bundles approved for it
      pairing.json                   this machine's pairing with the service
      .gitignore                     written by moil when missing
      bundles/transcribe/<hash>/     each approved bundle's three files
      hooks/transcribe               the owner's hook for that bundle (§2)
      logs/transcribe.log            what that hook printed
  .machine/                          moil's own
```

`.machine/` holds what only moil reads: working directories, bundle
environments, cached assets, hooks waiting to run, the tokens when there is
no keychain, the lock that one agent at a time holds, and the control
socket (§4). Its layout may change between versions, and nothing else
should depend on it.

Everything else is meant to be read. moil writes JSON pretty-printed with a
trailing newline, and replaces a file atomically, so a reader never sees
half of one. Times are RFC 3339 strings in UTC, such as
`2026-09-27T22:41:07Z`. JSON files are strict, like manifests (protocol
§5.3): a file with unknown fields or duplicate keys is invalid, so that a
typo is reported rather than ignored.

The owner, an editor or git MAY change `settings.json`, each
`service.json`, anything under `hooks/`, and the `.gitignore` files once
moil has written them. The rest is moil's: `pairing.json`, `bundles/`,
`logs/` and `README.md`, which moil rewrites when it opens the home, if
its text differs from what this version of moil writes.

### 1.1 Settings

```json
{
  "paused": false,
  "machine_name": "Ada's MacBook Pro",
  "allow_insecure_http": false,
  "keep_work_dirs": false
}
```

| Field | Default | Meaning |
|---|---|---|
| `paused` | `false` | Take no new jobs. A running job carries on. |
| `machine_name` | the computer's name | What services call this machine (protocol §4). They see a new name when the machine next connects. |
| `allow_insecure_http` | `false` | Accept `http` services, inputs and outputs on any host (protocol §3). For development only. |
| `keep_work_dirs` | `false` | Keep attempts' working directories, for debugging bundles. |

Every field is optional. `paused` takes effect at once;
`allow_insecure_http` and `keep_work_dirs` when moil next starts. moil
refuses to start with an invalid `settings.json`, rather than guess: a
guess could take jobs the owner meant to pause.

### 1.2 Services

A service's ID names its folder. moil derives it from the host and port of
the service's base URL when the machine first pairs with it
(`klisi.example.com`, `localhost-8787`, then `-2`, `-3` and so on if the
name is taken). An ID is 1 to 128 lowercase ASCII letters, digits, dots
and hyphens, starting and ending with a letter or digit.

`service.json` says which service the folder is for and what the owner
approved for it:

```json
{
  "base_url": "https://klisi.convex.works/moil",
  "name": "klisi",
  "approvals": [
    {
      "hash": "42d30e3fb23030627d993330b1e2917a0ac2f4abef567fc6e82328d8acedeae6",
      "name": "transcribe",
      "version": "0.3.0",
      "approved_at": "2026-09-27T22:20:11Z"
    }
  ]
}
```

| Field | Meaning |
|---|---|
| `base_url` | Where the service's moil endpoints are (protocol §1). |
| `name` | The name the service gave itself when pairing (protocol §4). |
| `approvals` | The bundles the owner approved for this service (protocol §5.5), oldest first. |

`pairing.json` belongs to this machine alone:

```json
{
  "base_url": "https://klisi.convex.works/moil",
  "machine_id": "m_7Hq2Xk9Lw3",
  "paired_at": "2026-09-27T22:10:00Z"
}
```

`machine_id` is the service's ID for this machine. The token that goes
with it is kept in the keychain on macOS, and in `.machine/` elsewhere,
together with the base URL it was issued for. It is never kept in the
service's folder.

A service is **paired** on this machine when its folder has a
`pairing.json` whose `base_url` is the one in `service.json`, and moil
holds a token issued for that URL. A folder without them, such as one
cloned from git, is a service the machine knows but isn't paired with.
Pairing it (§3.2) keeps its approvals and hooks.

moil MUST send a token only to the base URL it was issued for, as the
keychain (or `.machine/`) records it, whatever the service's files say:
`pairing.json` and `service.json` are plain files, and a `git pull` can
change both. If `service.json` names another URL, the service isn't
paired, and moil reports it as a problem (§3.3). To move a service to
another URL, pair with the new one.

An approval counts only while `bundles/<name>/<hash>/` holds the bundle's
three files, the files hash to `hash` (protocol §5.2), and the manifest's
name and version are the approval's. moil ignores an approval that
doesn't, and reports it as a problem (§3.3). moil writes bundle files
read-only, and runs a bundle only from a copy it has verified against the
approval, checked again before every run (protocol §5.5). Editing an
approved bundle's files therefore can't change what runs: its jobs are
refused until the files match again.

The `.gitignore` in a service's folder keeps `pairing.json` and `logs/`
out of git, so the folder can be a repository of its own. The one at the
root of the home keeps out `.machine/`, `settings.json`, `pairing.json`
and `logs/`, so the whole home can be one instead.

Whoever can change a service's folder can approve bundles for it and add
hooks. A folder kept in git extends that power to whoever can push to its
repository.

### 1.3 Changes made from outside

A running agent MUST notice changes that other programs (an editor, git)
make to `settings.json` or under `services/`, within five seconds, and
apply them as if the owner had made them through moil:

- An approval added or removed is announced to its service (protocol
  §6.2). An approval that stops counting (§1.2), say because its bundle's
  files were edited, is removed as far as the service can tell. A removed
  approval stops a running attempt of its bundle, which fails as
  `interrupted`. A new approval's bundle is prepared.
- A service that becomes paired is connected, and one that is no longer
  paired is disconnected. When its `pairing.json`, its `service.json` or
  its whole folder is gone, moil also deletes its token, and a
  `pairing.json` left without its `service.json`; when its
  `service.json` names another URL, moil keeps the token, so that putting
  the URL back pairs it again. The service still lists the machine until
  the owner removes it there. `moil unpair` tells the service.
- A change to `paused` takes effect.

When a file becomes unreadable or invalid, moil reports it as a problem
(§3.3) and keeps what it had read from that file before.

## 2. Hooks

A hook is an executable the owner puts at
`services/<service>/hooks/<bundle name>`. After an attempt of that bundle
for that service succeeds on this machine, moil runs the hook with the
attempt's outputs, if the hook's file is there when the attempt ends. The file is the whole setup: there is nothing to
register, and removing the file turns the hook off.

### 2.1 When a hook runs

moil runs a hook only after it has queued the attempt's `done` for the
service (protocol §7), so a hook never delays or changes what the service
sees, and nothing about hooks reaches the service. Hooks run one at a
time, in the order their attempts ended. The machine goes on taking jobs
while a hook runs, and hooks run while the machine is paused too.

moil executes the file directly, so it needs a `#!` line or must be a
program; moil doesn't guess at a shell for a file that is neither. If
something is at the hook's path but it isn't such a file, or a symbolic
link to one, that the owner can execute, moil doesn't run it, writes why
in the hook's log (§2.3), and reports it as a problem (§3.3).

### 2.2 What a hook gets

On stdin, the hook gets the **hook context**: one line of JSON, then
end-of-file.

```json
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
  "output_dir": "/Users/ada/.moil/.machine/hooks/…/out",
  "outputs": {
    "transcript.txt": "/Users/ada/.moil/.machine/hooks/…/out/transcript.txt",
    "transcript.vtt": "/Users/ada/.moil/.machine/hooks/…/out/transcript.vtt"
  }
}
```

| Field | Meaning |
|---|---|
| `v` | The hook contract's version: `1`. |
| `service` | The service's ID, name and base URL. |
| `bundle` | The approved bundle that ran. |
| `job_id`, `attempt` | As the service sent them (protocol §7.2). |
| `title` | The job's title, if the service gave one. |
| `params` | The job's params, as the script got them (protocol §8.2). |
| `meta` | The script's `result.meta`, if it sent one. |
| `ended_at` | When the attempt ended. |
| `output_dir` | The absolute path of a directory that holds the outputs and nothing else. |
| `outputs` | Each output the attempt uploaded, by name, with its absolute path. |

The hook runs:

- **In an empty directory**, which is also its `TMPDIR`, and which moil
  removes afterwards. The outputs are in `output_dir`, which the hook may
  change as it likes; moil deletes it once the hook exits, so a hook that
  wants to keep an output copies or moves it elsewhere. The outputs' names
  come from the service, so moil doesn't run the hook among them, where a
  file such as `uv.toml` could change what the hook runs.
- **With only these environment variables:** `HOME`, `USER`, `LANG`
  (moil's own, else `C.UTF-8`), `MOIL_HOME`, `TMPDIR`, and `PATH`, which
  is `<home>/.machine/bin:/usr/bin:/bin:/usr/sbin:/sbin`.
  `.machine/bin` holds the `moil` and `uv` this machine runs, so a hook
  starting `#!/usr/bin/env -S uv run --script` can declare its Python
  dependencies inline, as a bundle does. A hook names anything else it
  needs itself, a program by its absolute path or a `PATH` of its own, so
  that the file alone says what it depends on.
- **In a process group of its own, for at most an hour.** After that moil
  stops it the way it stops a script (protocol §8.6).

### 2.3 What a hook leaves

What a hook prints to stdout and stderr is appended to
`services/<service>/logs/<bundle name>.log`, between two lines moil writes:

```
== 2026-09-27T22:41:08Z recording-d07aa27961e839c14bb0a099bd971df0 attempt 1
…
== exited 0 after 42s
```

The closing line is one of:

- `== exited <status> after <duration>`
- `== killed by signal <number> after <duration>`
- `== exited after <duration>, but moil couldn't tell how`
- `== stopped after an hour`
- `== stopped: moil quit`
- `== stopped: moil stopped unexpectedly`, written when moil next starts,
  before the hook runs again
- `== not run: <reason>`

Before a hook runs, if its log has passed 1 MiB, moil renames it to
`<bundle name>.log.1`, replacing any older one, and starts a new log.
moil never writes to a log through a symbolic link, which a commit could
put at its path to point at another of the owner's files: what that hook
prints is lost, and moil says why in `logs/moil.log`.

moil records a hook's exit status and doesn't retry a hook that failed.
But a hook that didn't get to exit, because moil quit or crashed, runs
again from the start, with the same outputs, when moil next starts, unless
its file is gone by then. So a hook runs at least once for each attempt
that succeeded, and it should cope with running twice.

### 2.4 Trust

A hook is the owner's own code. moil doesn't ask anyone to approve it and
doesn't sandbox it, and it can do whatever the owner can.

Some of what a hook gets comes from the service: `title`, `params`,
`meta`, `job_id`, and the outputs' names and contents, which the approved
bundle made from the service's inputs. A hook SHOULD treat them as
untrusted. For example, it shouldn't build a file path from `title`
without checking it, or hand the outputs to a program that acts on
instructions it finds in them, such as an LLM that can run tools. The
context carries no token, and no URL other than the service's base URL.

## 3. The command line

### 3.1 Rules

- **Parity.** Everything the app does is also a command (§3.2). A change
  that adds something to the app adds its command at the same time.
- **The same whether or not an agent runs.** When an agent runs on the
  home, in the app or as `moil agent`, a command asks it through the
  control socket (§4): the agent pairs, fetches bundles, approves and
  holds the tokens, and the change takes effect at once. When no agent
  runs, the command works on the home itself. Only `stop`, `quit`,
  `login-item` and `status --watch` need a running agent. Without one they
  say so and exit with status 3.
- **Output.** Results go to stdout, as `Label: value` lines or aligned
  columns. Hints, prompts and progress go to stderr. Everything a service
  or a bundle controls is shown with hidden characters revealed (protocol
  §5.5).
- **JSON.** Every command in §3.2 except `logs`, `agent` and the bundle
  authoring commands takes `--json` (`moil run --json` keeps printing the
  job's events, as it does today). With `--json`,
  stdout carries only JSON: one value, or
  one value per line for a command that keeps printing (`status --watch`,
  `pair`). The JSON form of `Label: value` lines is an object whose keys
  are the labels in lowercase, with underscores for spaces: `Approved:
  <hash>` becomes `{"approved": "<hash>"}`. §3.3 gives the other shapes.
  Optional fields are omitted when absent, as on the wire (protocol §2).
  Every character that text output would reveal is written as a JSON
  escape.
- **Exit status.** `0` when the command did what it was asked. `1` when it
  failed, with a sentence on stderr saying why and what to do. `2` when the
  command line was wrong. `3` when it needs a running agent and none runs.
- **No prompts without a terminal.** The only command that asks, `approve`,
  refuses to approve when stdin isn't a terminal, unless `--yes` is given,
  and says so. `approve --json` without `--yes` is a wrong command line: it
  exits with status 2, with a message that names `--yes`.
- **Services** are named by their ID, host, base URL or name.
- **Help.** `moil <command> --help` explains the command, with an example.
  `moil hooks --help` gives the hook contract (§2) in full.

### 3.2 Commands

| Command | What it does | In the app |
|---|---|---|
| `moil status [--watch]` | Whether an agent runs and in what; whether the machine is paused; the running job and its progress; the last job; each service's connection, and the bundles waiting for review; the hooks; and any problem with the home. `--watch` prints it again whenever it changes. | The tray menu |
| `moil pause`, `moil resume` | Stop or resume taking new jobs. A running job carries on. | Pause, Resume |
| `moil stop` | Stop the running job. Its service may give it to another machine. | Stop Current Job |
| `moil pair SERVICE [--name NAME] [--no-browser]` | Pair with the service at a URL, with a service the home knows but isn't paired with (§1.2), or again with a paired one. Prints the code and the confirmation page's URL, opens the page, and waits for the owner to confirm there. `--name` sets `machine_name` (§1.1), for every service. Without an agent, the command holds the home until the pairing ends, so no agent can start meanwhile. | Pair with a Service…, Pair Again |
| `moil unpair SERVICE` | Tell the service to forget this machine, and delete this machine's pairing and token. The folder, with its approvals and hooks, stays; delete it to forget those too. | Unpair |
| `moil services` | The services the home knows, paired or not, with their approvals. | The services in the menu |
| `moil review SERVICE [HASH]` | The bundles a service offers and whether each is approved. With a hash, that bundle in full (protocol §5.5). | Review |
| `moil approve SERVICE HASH [--yes]` | Show the bundle in full, then approve it. | Approve |
| `moil revoke SERVICE HASH` | Withdraw an approval, stopping a running attempt of that bundle, and delete the bundle's files. It also removes an approval that doesn't count (§1.2). | Revoke Approval |
| `moil hooks` | Each hook: its service and bundle, its path and log, and whether moil can run it. | The hooks in the menu |
| `moil logs [SERVICE BUNDLE] [-f] [-n LINES]` | The agent's log or, given a service and bundle, that hook's log: all of it, or its last `LINES`. `-f` follows it, starting from the last 10 lines unless `-n` says otherwise. | Show Log |
| `moil home` | The home's path. | Open moil Folder |
| `moil login-item [on\|off]` | Whether the app opens at login. Needs the app to be running; under `moil agent` it fails. | Open at Login |
| `moil quit` | Stop the agent. The running job is interrupted and reported, and the app quits, or `moil agent` exits. | Quit moil |
| `moil agent` | Run the agent in this terminal until interrupted. | — |
| `moil init`, `check`, `hash`, `run` | Bundle authoring, as before. | — |

`moil home` prints `Home: <path>`. The other commands' `Label: value`
lines are as they are today: `Code:`, `URL:` and `Paired:` for `pair`,
`Approved:`, `Revoked:`, `Unpaired:`, and `Paused:` (`true` or `false`)
for `pause` and `resume`, which answer once `settings.json` says so.
`stop` prints `Stopping:` with the job's ID and attempt, or nothing if no
job runs; `stop --json` then prints `{}`, as `quit --json` does. `login-item` prints `Open at login:`
(`true` or `false`).

### 3.3 JSON shapes

`moil status --json`:

```json
{
  "home": "/Users/ada/.moil",
  "agent": {"host": "app", "version": "0.2.0"},
  "paused": false,
  "job": {
    "service": "klisi.convex.works",
    "job_id": "recording-d07aa27961e839c14bb0a099bd971df0",
    "attempt": 1,
    "title": "Weekly sync",
    "bundle": "transcribe",
    "phase": "running",
    "fraction": 0.42,
    "message": "diarizing",
    "started_at": "2026-09-27T22:32:43Z",
    "stopping": false
  },
  "last_job": {
    "service": "klisi.convex.works",
    "job_id": "recording-5b1e0c2f9a7d4e6b8c3a1f0e2d4c6b8a",
    "attempt": 2,
    "bundle": "transcribe",
    "outcome": "failed",
    "error": {"code": "timeout", "message": "the job ran past its time limit"},
    "ended_at": "2026-09-27T21:05:12Z"
  },
  "services": [],
  "problems": [
    {
      "path": "services/klisi.convex.works/hooks/transcribe",
      "message": "moil can't run this hook: it isn't executable. Run chmod +x on it."
    }
  ]
}
```

- `agent` is present while an agent runs. Its `host` is `app` or `agent`
  (`moil agent`), and its `version` is the running moil's.
- `job` is the running attempt, if any. `phase` is as in protocol §8.3,
  and `fraction` and `message` are the script's latest progress.
- `last_job` is the attempt that ran here last since the agent started.
  It has a `title` when the job had one. `outcome` is `succeeded`,
  `failed`, `cancelled` or `interrupted`, and `error` comes with `failed`.
- `services` lists every service the home knows, in the shape below.
- `problems` lists what moil found wrong with the home, each with the path
  concerned (relative to the home) and a sentence the owner can act on.

A service, in `status` and as each element of `moil services --json`:

```json
{
  "id": "klisi.convex.works",
  "name": "klisi",
  "base_url": "https://klisi.convex.works/moil",
  "paired": true,
  "connection": {"state": "offline", "error": "connection refused", "retry_at": "2026-09-27T22:33:10Z"},
  "approvals": [
    {"hash": "42d30e3f…", "name": "transcribe", "version": "0.3.0", "approved_at": "2026-09-27T22:20:11Z"}
  ],
  "offered": [
    {"hash": "9c1d77a0…", "name": "transcribe", "version": "0.4.0", "description": "…", "approved": false}
  ],
  "hooks": [
    {
      "bundle": "transcribe",
      "path": "/Users/ada/.moil/services/klisi.convex.works/hooks/transcribe",
      "log": "/Users/ada/.moil/services/klisi.convex.works/logs/transcribe.log",
      "runnable": true
    }
  ]
}
```

- `paired` is true when the folder's `pairing.json` names its URL
  (§1.2), with or without an agent. Only an agent looks for the token,
  since reading the keychain may ask the owner first; it tells a missing
  one by the connection.
- `connection` and `offered` are present while an agent runs and the
  service is paired. `connection.state` is `connecting`, `connected`,
  `offline` (with `error` and `retry_at`), `unpaired` (pair again: the
  service removed this machine, or moil has no token for it) or
  `replaced` (another connection with this machine's token took over).
- `approvals` lists only approvals that count (§1.2).
- `offered` is what the service offers, as it describes it. A bundle with
  `approved` set to `false` is waiting for the owner's review.

Each element of `moil hooks --json` is a hook, as in `hooks` above, with
`service` added.

`moil review SERVICE --json` is a list of the service's bundles:
`{"hash", "name", "version", "description", "approved", "offered"}`, where
`offered` is `false` for an approved bundle the service no longer offers.

`moil review SERVICE HASH --json` is one bundle in full:

```json
{
  "service": {"id": "klisi.convex.works", "name": "klisi", "base_url": "https://klisi.convex.works/moil"},
  "hash": "9c1d77a0…",
  "approved": false,
  "files": {"job.py": "…", "job.py.lock": "…", "manifest.json": "…"},
  "packages": [
    {"name": "numpy", "version": "2.3.1", "source": "https://pypi.org/simple", "builds_from_source": false}
  ],
  "previous": {
    "hash": "42d30e3f…",
    "version": "0.3.0",
    "changes": {"job.py": "--- job.py\n+++ job.py\n@@ …"}
  }
}
```

`previous` is the approved version this bundle would replace, if there is
one, and `changes` holds a unified diff for each file that changed.

## 4. The control socket

The process that runs the agent, the app or `moil agent`, listens on a
Unix socket once it holds the home's lock, and stops listening when it
lets go. Commands use it to reach the agent (§3.1).

- Only the owner's user can connect. The socket is in `.machine/`, which is
  private, and the agent also checks the user ID of each connecting
  process.
- The messages are moil's own and aren't specified here, since both ends
  are always the same moil. A command that reaches an agent of a different
  version fails, and says to use `<home>/.machine/bin/moil`, which is the
  agent's own.
- Whatever the agent does for a command, it does as if the owner had done
  it in the app.

## 5. The app

- The app runs the agent on the home and serves the control socket.
- When it starts, it links `~/.local/bin/moil` to its own `moil`, so that
  the command is on the `PATH` of most shells. It does so only when nothing
  is at that path, or a link into a `Moil.app` is. It never replaces
  anything else.
- Its menu has **Open moil Folder**, which opens the home, and lists each
  paired service's hooks and any problems (§3.3).
- It logs to `logs/moil.log` in the home.

## 6. Coming from moil 0.1

moil 0.1 kept its state in `~/Library/Application Support/moil`, in
another layout. Later versions don't read it. After upgrading, pair again
and approve the bundles again, and remove the old machine on each
service's side. The old folder can then be deleted.
