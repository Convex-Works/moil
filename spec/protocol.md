# moil protocol, version 1

This document is normative. The Go server SDK, the Rust machine agent and
the job runtime are all built against it, and the files in
[`vectors/`](vectors) pin the parts that must match byte for byte.

The key words MUST, SHOULD and MAY are used as in RFC 2119.

- [1. Roles and terms](#1-roles-and-terms)
- [2. Conventions](#2-conventions)
- [3. HTTP endpoints](#3-http-endpoints)
- [4. Pairing](#4-pairing)
- [5. Job bundles](#5-job-bundles)
- [6. The control channel](#6-the-control-channel)
- [7. Job lifecycle](#7-job-lifecycle)
- [8. The script contract](#8-the-script-contract)
- [9. Errors](#9-errors)
- [10. Limits](#10-limits)
- [11. Versioning](#11-versioning)

## 1. Roles and terms

- **Service:** a web application (e.g. klisi) that embeds the server SDK.
  It has a **base URL**, the URL where the SDK's HTTP handler is mounted,
  e.g. `https://klisi.example.com/moil`.
- **Machine:** a computer running the moil app (or the headless `moil
  agent`). It is paired with zero or more services.
- **Owner:** the service's own identifier for the user who paired a
  machine. moil never interprets it; the service uses it in its
  eligibility policy.
- **Bundle:** the code a machine runs, identified by its **bundle hash**.
- **Job:** a unit of work a service submits. A job runs as one or more
  **attempts**; each attempt runs on exactly one machine.

## 2. Conventions

- All JSON is UTF-8. Durations are integer milliseconds in fields ending in
  `_ms`; sizes are integer bytes in fields ending in `_bytes`.
- Optional fields are omitted when absent, never sent as `null`. Fields
  that carry arbitrary JSON (`params`, `payload`, `meta`) MAY be `null`.
- Those fields carry JSON values, not bytes: every hop may re-encode them,
  changing key order, whitespace and how numbers are written. Keep them
  within I-JSON (RFC 7493): numbers must fit an IEEE 754 double, so send
  larger integers, such as 64-bit IDs, as strings.
- Receivers MUST ignore unknown fields in HTTP bodies and control
  messages, so that minor additions don't break older peers. Manifests
  (§5.3) are the exception: they are strict.
- Hashes are SHA-256, written as 64 lowercase hex characters.
- Lengths given in characters count Unicode code points.

Identifiers:

| Name | Format |
|---|---|
| machine ID | chosen by the service, 1–64 characters of `[A-Za-z0-9_-]` |
| job ID | chosen by the service, `^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$` |
| attempt | integer ≥ 1, increasing per job |
| file name (inputs, outputs) | `^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$` |
| asset name | `/`-separated segments, each `^[A-Za-z0-9][A-Za-z0-9._-]*$`, at most 256 characters in total |
| bundle name | `^[a-z0-9][a-z0-9-]{0,63}$` |

## 3. HTTP endpoints

All paths are relative to the service's base URL.

| Method and path | Auth | Purpose |
|---|---|---|
| `GET /v1/info` | none | Identify the service (§3.1) |
| `POST /v1/pair` | none | Start pairing (§4) |
| `POST /v1/pair/token` | none | Poll for the machine token (§4) |
| `GET /v1/bundles` | machine | List the bundles the service may send jobs for (§5.4) |
| `GET /v1/bundles/{hash}` | machine | Fetch a bundle's files (§5.4) |
| `GET /v1/connect` | machine | Open the control channel (§6) |
| `DELETE /v1/machine` | machine | Unpair: the service forgets the calling machine (§4) |

"machine" auth is `Authorization: Bearer <token>` with the token issued
during pairing. A missing, unknown or revoked token gets `401`.

Error responses carry a JSON body `{"error": "<code>", "message": "..."}`
(`message` is optional and for humans).

Machines MUST use `https` base URLs, except for loopback hosts
(`localhost`, `127.0.0.0/8`, `::1`), which MAY use `http`. Input and output
URLs (§7.4) MUST use `https` too; they MAY use `http` to a loopback host
only when the service's own base URL is loopback, as in local development,
so that a remote service can't aim requests at services that listen only
on the owner's machine. Redirects are held to the same rule at every hop.
A machine owner MAY switch these checks off for development; the app then
says so wherever it shows the service. Services SHOULD refuse, when a job
is submitted, URLs that machines would refuse.

### 3.1 Info

```json
GET /v1/info
→ 200 {"protocol": 1, "name": "klisi"}
```

The app calls this when the user enters a base URL, to confirm it points
at a moil service before starting to pair.

## 4. Pairing

Pairing follows the OAuth 2.0 device authorization grant (RFC 8628): the
machine displays a short code and the user confirms it in the service's web
UI, where they are already signed in. This ties the machine to a service
account without the app ever handling the user's credentials.

```
app                              service                          user (browser)
 │ POST /v1/pair ───────────────▶ │                                   │
 │ ◀──── device_code, user_code   │                                   │
 │ shows user_code, opens ───────────────────── verification_uri ───▶ │
 │                                │ ◀──── signed-in user confirms ─── │
 │ POST /v1/pair/token ─────────▶ │                                   │
 │ ◀──── machine_id, token        │                                   │
```

**Start:**

```json
POST /v1/pair
{"name": "Ada's MacBook Pro", "os": "macos", "arch": "aarch64", "app_version": "0.1.0"}

→ 200
{
  "device_code": "0V8i3Yl5tE9c7mNcA4Qe2wBv",
  "user_code": "WDJB-MJHT",
  "verification_uri": "https://klisi.example.com/machines/pair",
  "verification_uri_complete": "https://klisi.example.com/machines/pair?code=WDJB-MJHT",
  "expires_in": 600,
  "interval": 2,
  "service": {"name": "klisi"}
}
```

- `name` is what the user calls the machine (at most 64 characters). `os`
  and `arch` use Rust's `std::env::consts` values (`macos`, `linux`,
  `windows`; `aarch64`, `x86_64`).
- `service.name` is how the app names the service: 1–64 characters, with
  no control characters. Machines show it with hidden characters revealed
  (§5.5), since the service chooses it.
- `user_code` is 8 characters from `BCDFGHJKLMNPQRSTVWXZ` with a hyphen in
  the middle. The service compares codes case-insensitively and ignores the
  hyphen.
- `expires_in` and `interval` are seconds, as in RFC 8628.

The app shows `user_code` and opens `verification_uri_complete`. The
service's page shows the pending machine's name, OS and code, and asks the
signed-in user to confirm. On confirmation the service records the machine
with that user as its owner.

**Poll:**

```json
POST /v1/pair/token
{"device_code": "0V8i3Yl5tE9c7mNcA4Qe2wBv"}

→ 200 {"machine_id": "m_7Hq2Xk9Lw3", "token": "moil_…", "service": {"name": "klisi"}}
→ 400 {"error": "authorization_pending"}
→ 400 {"error": "slow_down"}
→ 400 {"error": "access_denied"}
→ 400 {"error": "expired_token"}
```

The app polls every `interval` seconds and adds 5 seconds to the interval
on `slow_down`. `access_denied` means the user rejected the pairing;
`expired_token` means the code expired, was already used, or was never
issued. A device code yields a token exactly once. Services SHOULD tolerate
some jitter before answering `slow_down` (the Go SDK accepts polls three
quarters of an interval apart).

The service MUST store only a hash of the token. Removing a machine on the
service revokes its token and closes its control channel with code `4401`.

**Unpair:** a machine that forgets a service first calls
`DELETE /v1/machine` with its token (`204` on success), so the service
removes it too. This is best effort: the machine forgets the service even
if the call fails.

## 5. Job bundles

### 5.1 Files

A version 1 bundle is exactly these three files, all UTF-8 text:

| File | Contents |
|---|---|
| `job.py` | The script, with PEP 723 inline metadata declaring `requires-python` and `dependencies`. |
| `job.py.lock` | The lockfile `uv lock --script job.py` writes: every package at an exact version, with the hash of every artifact. |
| `manifest.json` | Metadata and model assets (§5.3). |

The lockfile must pin everything it can, because the owner approves what it
names:

- Every sdist and wheel it lists MUST carry a SHA-256 `hash`.
- Every package MUST come from an `https` package index (`registry`), an
  `https` URL (`url`), or a git repository over `https` pinned to a full
  40-character commit (`git`). Path, directory and editable sources are
  invalid: they name files on the author's machine.
- A package the lockfile gives no wheel for is built from source on the
  machine. Its build requirements are resolved when it's built rather than
  pinned by the lockfile, so the approval screen marks it. Every other
  package is installed only from a wheel (§8.1).

### 5.2 Bundle hash

The bundle hash is the SHA-256 of the text produced by listing, for each
file in bytewise order of name, the file's own SHA-256 in hex, two spaces,
the file name and a newline:

```
<sha256(job.py)>  job.py\n
<sha256(job.py.lock)>  job.py.lock\n
<sha256(manifest.json)>  manifest.json\n
```

This is exactly the output of `sha256sum`, so anyone can check a hash with
standard tools:

```sh
cd bundle && shasum -a 256 job.py job.py.lock manifest.json | shasum -a 256
```

Every party computes the hash itself from the files it holds. A machine
MUST NOT rely on a hash a service claims without recomputing it.

### 5.3 Manifest

```json
{
  "moil": 1,
  "name": "transcribe",
  "version": "0.3.0",
  "description": "Transcribe a meeting recording with speaker labels.",
  "assets": {
    "whisper/model.bin": {
      "url": "https://huggingface.co/…/model.bin",
      "sha256": "…",
      "size_bytes": 75538270
    }
  },
  "resources": {"memory_bytes": 8589934592, "gpu": "optional"}
}
```

| Field | Required | Meaning |
|---|---|---|
| `moil` | yes | Manifest format version. MUST be `1`. |
| `name` | yes | Bundle name (§2). Versions of a bundle share a name. |
| `version` | yes | Free-form version label, 1–32 characters. |
| `description` | no | At most 500 characters. |
| `assets` | no | Files the runtime downloads, verifies and hands to the script (§8.4), keyed by asset name. No name may be a directory of another (`a` and `a/b`). `url` follows the grammar below. `size_bytes` is optional. |
| `resources` | no | Informational in v1, shown to the machine owner. `memory_bytes` is an integer; `gpu` is `none`, `optional` or `required`. |

`version` and `description` contain no control characters (a
`description` may contain line feeds) and no bidirectional formatting
characters (U+061C, U+200E, U+200F, U+202A–U+202E, U+2066–U+2069).

An asset `url` is `https://` host [`:` port] [path] [`?` query]. The host
is a DNS name of ASCII letters, digits, hyphens and dots, or an IPv4
address; the port is 1–65535; the path and query use only RFC 3986
unreserved characters, percent-encodings, sub-delimiters, `:`, `@`, `/`
and `?`. So there is no user info, fragment, whitespace or backslash. The
grammar is deliberately narrower than what URL parsers accept, so that
every implementation agrees on which manifests are valid.

Manifests are strict: a manifest with unknown fields or duplicate keys, at
any level, is invalid. So is one that isn't valid UTF-8, including `\u`
escapes of unpaired surrogates. The approval screen shows the manifest in full, but a
field the app doesn't understand could carry meaning the owner never sees explained,
and a duplicate key could show the owner one value while a parser uses
another.

A bundle is **valid** when it has exactly the three files, each is UTF-8,
the manifest parses under the rules above, and `job.py.lock` parses as
TOML. Machines MUST refuse to approve or run an invalid bundle.

### 5.4 Listing and fetching bundles

```json
GET /v1/bundles
→ 200 {"bundles": [{"hash": "…", "name": "transcribe", "version": "0.3.0", "description": "…"}]}
```

The list is the same one `welcome` and `bundles` carry (§6.3), for clients
that don't hold a control channel open, such as the command line.

```json
GET /v1/bundles/{hash}
→ 200 {"hash": "…", "files": {"job.py": "…", "job.py.lock": "…", "manifest.json": "…"}}
→ 404 {"error": "not_found"}
```

File contents are JSON strings holding the files' exact bytes. The machine
recomputes the hash and MUST reject the bundle if it differs from the
requested hash.

### 5.5 Approval

The machine owner approves a bundle for one service at a time: approval is
a (service, bundle hash) pair. Before approving, the app shows the
service, the three files in full, the bundle hash, the command from §5.2,
and a diff against the most recently approved bundle with the same name
from the same service. Everything the service or the bundle controls is
shown with hidden characters revealed: control characters other than tab
and line feed (including carriage returns and terminal escapes),
bidirectional formatting characters, and invisible or default-ignorable
code points, any of which could make code read differently from how it
runs.

On approval the machine stores the verified files locally and runs jobs
only from that stored copy, re-checking the hash before each run. A
service therefore can't change an approved bundle after the fact.

## 6. The control channel

The machine opens one WebSocket per service:

```
GET /v1/connect
Authorization: Bearer <token>
```

Every message is a JSON object in a single text frame, with a `type`
field. A peer that receives a message it can't parse, or a known message
with invalid fields, MAY close the channel with code `4400`. The machine
sends `hello` as soon as the channel opens; a service MAY close a channel
that hasn't sent one within its ping interval.

### 6.1 Liveness

The service sends a WebSocket ping at least every 20 seconds. A machine
that receives nothing (pings included) for 45 seconds MUST treat the
channel as dead, and a machine whose writes stall for as long does the
same. The service treats a missed pong as a dead channel.

After a lost channel the machine reconnects with exponential backoff
(1 second doubling to 60 seconds, with jitter), except as noted in §6.4.

### 6.2 Machine → service

**hello** is the machine's first message.

```json
{
  "type": "hello",
  "protocol": 1,
  "app_version": "0.1.0",
  "machine": {
    "name": "Ada's MacBook Pro",
    "os": "macos",
    "arch": "aarch64",
    "cpus": 12,
    "memory_bytes": 38654705664,
    "gpus": [{"name": "Apple M3 Pro"}]
  },
  "state": "idle",
  "approved": ["<bundle hash>", "…"],
  "attempts": [{"job_id": "j_42", "attempt": 1}]
}
```

- `state` is `idle` (can take a job), `busy` (running a job, for this
  service or another) or `paused` (the owner paused the machine). A
  machine paused while running a job reports `busy` until the job ends,
  then `paused`.
- `approved` lists every bundle hash the owner approved for this service.
- `attempts` lists attempts for this service that the machine still holds:
  running, or finished with an unacknowledged `done` (§7.5).
- In `gpus`, each entry has a `name` and optionally `memory_bytes`.

**state** reports a change of state.

```json
{"type": "state", "state": "busy"}
```

**approved** replaces the set of approved hashes after the owner approves
or revokes a bundle.

```json
{"type": "approved", "hashes": ["<bundle hash>"]}
```

**bid** and **decline** answer an offer.

```json
{"type": "bid", "job_id": "j_42"}
{"type": "decline", "job_id": "j_42", "reason": "busy"}
```

`reason` is `busy`, `paused` or `not_approved`.

**event** forwards one job event (§8.3) with a sequence number.

```json
{"type": "event", "job_id": "j_42", "attempt": 1, "seq": 3,
 "event": {"v": 1, "type": "progress", "fraction": 0.42, "message": "diarizing"}}
```

**renew** keeps an attempt's lease alive (§7.3).

```json
{"type": "renew", "job_id": "j_42", "attempt": 1}
```

**done** ends an attempt (§7.5).

```json
{"type": "done", "job_id": "j_42", "attempt": 1, "seq": 9, "outcome": "succeeded",
 "result": {"meta": {"speakers": 3},
            "files": {"transcript.json": {"size_bytes": 18211, "sha256": "…"}}}}

{"type": "done", "job_id": "j_42", "attempt": 1, "seq": 9, "outcome": "failed",
 "error": {"code": "script_error", "message": "CUDA out of memory", "retryable": true,
           "exit_code": 1, "stderr_tail": "…"}}

{"type": "done", "job_id": "j_42", "attempt": 1, "seq": 9, "outcome": "cancelled"}
```

`outcome` is `succeeded` (carries `result`), `failed` (carries `error`,
§9) or `cancelled` (the service asked, §7.6).

### 6.3 Service → machine

**welcome** answers `hello`.

```json
{
  "type": "welcome",
  "protocol": 1,
  "machine_id": "m_7Hq2Xk9Lw3",
  "service": {"name": "klisi"},
  "bundles": [{"hash": "…", "name": "transcribe", "version": "0.3.0", "description": "…"}],
  "resume": [{"job_id": "j_42", "attempt": 1, "seq": 7}]
}
```

- `bundles` lists every bundle the service may send jobs for, so the app
  can ask the owner to review new ones. The app MUST NOT trust these names
  and descriptions for approval; it shows what it fetches and verifies.
- `resume` answers `hello.attempts` (§7.5).

**bundles** replaces the list of bundles when it changes.

```json
{"type": "bundles", "bundles": [{"hash": "…", "name": "transcribe", "version": "0.3.0"}]}
```

**offer** asks whether the machine will take a job.

```json
{"type": "offer", "job_id": "j_42", "bundle_hash": "…", "title": "Weekly sync", "expires_ms": 2000}
```

`title` is an optional human-readable label (at most 200 characters) the
app may display. The machine answers with `bid` or `decline` before
`expires_ms` elapses; later answers are ignored.

**assign** gives an attempt to one machine.

```json
{
  "type": "assign",
  "job_id": "j_42",
  "attempt": 1,
  "bundle_hash": "…",
  "title": "Weekly sync",
  "params": {"language": "en"},
  "inputs": {"audio.ogg": {"url": "https://s3…", "headers": {}}},
  "outputs": {"transcript.json": {"url": "https://s3…", "method": "PUT", "headers": {"Content-Type": "application/json"}}},
  "lease_ms": 30000,
  "timeout_ms": 3600000
}
```

- `params` is arbitrary JSON handed to the script. It is always present
  and MAY be `null`.
- `inputs` maps file names to URLs the runtime downloads with `GET`.
- `outputs` maps the file names the script may produce to URLs the runtime
  uploads them to, with `method` `PUT` (the default) or `POST`.
- `headers` are optional in both.
- `timeout_ms` is optional; without it the attempt has no time limit.
  When present it is at least 1. It covers the whole attempt: preparation,
  transfers and the script.

**ack** acknowledges a `done`.

```json
{"type": "ack", "job_id": "j_42", "attempt": 1}
```

**cancel** stops an attempt.

```json
{"type": "cancel", "job_id": "j_42", "attempt": 1}
```

### 6.4 Close codes

| Code | Meaning | Machine reaction |
|---|---|---|
| `4400` | Protocol error | Reconnect with backoff |
| `4401` | Machine removed or token revoked | Stop reconnecting; mark the service as unpaired |
| `4409` | Another connection with the same token replaced this one | Stop reconnecting; show the conflict |
| `1001` | The service is shutting down | Reconnect with backoff |

An upgrade request answered with `401` means the same as `4401`. Any other
close, or a connection lost without one, means reconnect with backoff.

## 7. Job lifecycle

### 7.1 Offers

A service offers a job to machines that are connected, report `idle`, have
approved the job's bundle hash, and pass the service's own eligibility
policy. It then picks one of the bidders by its own preference (for
example the job owner's machine, or one with a GPU) and assigns the
attempt to it. The machine that bid first doesn't automatically win.

Offering doesn't reserve a machine. A service SHOULD NOT offer a machine a
second job while an offer to it is open, and SHOULD NOT offer a job again
to a machine that declined it until that machine reports a change
(`state` or `approved`).

A machine bids only if it is idle, has approved the bundle for this
service, and holds a verified copy of it. Otherwise it declines, giving the
first reason that applies: `busy`, `paused`, `not_approved`.

### 7.2 Assignment

On `assign` the machine re-checks that it is idle and that the bundle is
approved, then starts the attempt and reports `state: busy` to every
service it is connected to. If the checks fail it immediately sends a
`done` with outcome `failed` and code `busy` (also when the machine is
paused) or `not_approved`. Those two
codes mean the attempt never started, so services SHOULD NOT count it
toward their retry limit.

If the machine can read an `assign`'s `job_id` and `attempt` but not the
rest of it (for example `params` beyond the limits of §10), it sends a
`done` with outcome `failed` and code `rejected`, which is not retryable.

A machine runs at most one attempt at a time, across all services.

### 7.3 Leases

Each attempt has a lease of `lease_ms`. Every job-scoped message the
service receives for the attempt (`event`, `renew`, `done`) renews it. A
connected machine sends `renew` whenever `lease_ms / 3` passes without
another message for the attempt, for as long as the attempt runs.

Services SHOULD use a lease of at least 90 seconds (the Go SDK's default
is 120), so that a machine can notice a silently dead channel (§6.1) and
reconnect before the lease runs out.

A lease keeps running while the machine is disconnected. If it runs out,
the service fails the attempt with code `lease_expired`, which is
retryable. If the machine is still connected, the service SHOULD also send
it `cancel`, so a hung attempt doesn't keep the machine busy.

### 7.4 Inputs, outputs and assets

The runtime downloads inputs before starting the script and uploads
outputs after it finishes (§8). Uploads send the file's bytes as the body,
with `Content-Length` and the given headers; any `2xx` counts as success.
Uploads don't follow redirects: an HTTP client following a `303` would
resend the request as a `GET` without the file, so a `3xx` is a failure.
Downloads follow redirects (§3). A machine MAY cap the size of downloads
whose size it isn't told, to protect its disk.
The machine sends no credentials of its own. Pre-signed URLs carry their
own authorization.

### 7.5 Sequence numbers and resumption

`event` and `done` messages for an attempt carry `seq`, starting at 1 and
increasing by 1. The service processes each `seq` at most once, ignoring
any `seq` it has already seen.

The machine keeps every `event` and `done` of an attempt until the service
acknowledges its `done` with `ack`. The service acks every `done` it
receives, including one for an attempt that is no longer current, so the
machine can always let go of it. After reconnecting it lists those
attempts in `hello.attempts`. In `welcome.resume` the service lists, for
each attempt it still considers assigned to this machine, the last `seq` it
processed (0 if none). The machine then:

- replays its kept messages with a greater `seq`, in order, and carries on;
- abandons every attempt listed in `hello.attempts` but absent from
  `resume`: it stops the process if still running and drops the messages.

If `hello.attempts` omits an attempt the service considers assigned to
that machine (for example after the app restarted), the service fails the
attempt with code `lost`, which is retryable. Removing a machine fails its
attempt the same way.

A machine MAY bound what it keeps. If it had to drop events, it replays
what it has and `seq` has a gap. It MUST keep the `done` of an attempt that
started. It MAY drop the `done` of an attempt it refused (§7.2); if that
`done` is lost, the service fails the attempt as `lost` when the machine
reconnects, or as `lease_expired` when its lease runs out.

### 7.6 Cancellation and interruption

`cancel` makes the machine stop the attempt (§8.6) and send `done` with
outcome `cancelled`. A machine MUST NOT report `cancelled` for an attempt
the service didn't cancel. If the owner stops a job on the machine, or quits the
app while one runs, the machine sends `done` with outcome `failed` and code
`interrupted`, which is retryable, so the service can hand the job to
another machine.

### 7.7 Retries

A service MAY retry a failed attempt whose error is retryable, as a new
attempt with the next attempt number, on any machine. Attempt numbers let
both sides tell attempts apart; a message for an attempt that isn't
current is ignored.

## 8. The script contract

### 8.1 Preparation

Before running an attempt, and ideally right after approval, the runtime
prepares the bundle:

1. It creates the environment with
   `uv sync --locked --script <bundle dir>/job.py`. `--locked` makes uv
   refuse a lockfile that doesn't match the script's inline metadata, so
   the dependencies the owner read in `job.py` are the ones installed. For
   every package the lockfile has a wheel for, it adds
   `--no-build-package <name>`, so uv never builds one from source instead,
   for example on a platform the wheels don't cover.
2. It downloads each asset that isn't cached yet and checks its SHA-256.
   Assets are cached by hash and shared between bundles.

The runtime chooses where each bundle's environment lives and removes it
once the bundle is no longer stored. It runs uv with its own configuration
only (`UV_NO_CONFIG=1`) and
with uv-managed Python (`UV_MANAGED_PYTHON=1`), so the machine's own uv or
Python setup doesn't change what runs. It also sets `UV_SYSTEM_CERTS=1`, so
package downloads trust the same certificate authorities as the machine.
Downloads, of assets here and of inputs later, follow redirects across
hosts, applying the URL policy of §3 to every hop.

### 8.2 Invocation

The runtime runs

```
uv run --locked --script <bundle dir>/job.py
```

in a fresh working directory per attempt, laid out as:

```
<work dir>/
  in/       inputs, downloaded under their names
  out/      where the script writes outputs
  assets/   links to the attempt's assets, under their names
  tmp/      TMPDIR for the script
```

The process gets a minimal environment: `PATH`, `HOME`, `LANG`, `TMPDIR`,
`PYTHONUNBUFFERED=1`, and the variables uv needs. It runs in its own process
group.

The script reads a single JSON document from stdin, the **job context**,
which the runtime writes as one line followed by end-of-file:

```json
{
  "v": 1,
  "job_id": "j_42",
  "attempt": 1,
  "params": {"language": "en"},
  "work_dir": "/…/work",
  "inputs": {"audio.ogg": "/…/work/in/audio.ogg"},
  "output_dir": "/…/work/out",
  "outputs": ["transcript.json"],
  "assets_dir": "/…/work/assets",
  "assets": {"whisper/model.bin": "/…/work/assets/whisper/model.bin"}
}
```

All paths are absolute. The script never sees the control channel, the
machine token or any URL.

### 8.3 Events

The script writes events to stdout as JSON objects, one per line:

```jsonl
{"v":1,"type":"progress","fraction":0.42,"message":"diarizing"}
{"v":1,"type":"log","level":"info","message":"loaded model"}
{"v":1,"type":"data","payload":{"segment":{"start":1.2,"end":3.4,"text":"hi"}}}
{"v":1,"type":"result","files":["transcript.json"],"meta":{"speakers":3}}
{"v":1,"type":"error","message":"CUDA out of memory","retryable":true}
```

| Type | Fields | Forwarded to the service |
|---|---|---|
| `progress` | `fraction` (0 to 1, optional), `message` (optional); at least one | yes |
| `log` | `level` (`debug`, `info`, `warn`, `error`; default `info`), `message` | yes |
| `data` | `payload` (any JSON) | yes, as the same JSON value (§2) |
| `result` | `files` (output names, default `[]`), `meta` (any JSON, optional) | folded into `done` |
| `error` | `message`, `retryable` (default `false`) | folded into `done` |

During the `preparing`, `downloading` and `uploading` phases the runtime
MAY send `progress` events about its own transfers; the phase tells them
apart from the script's. It adds one event type of its own, which scripts
can't send:

| Type | Fields |
|---|---|
| `phase` | `phase`: `preparing`, `downloading`, `running` or `uploading` |

`v` is optional and defaults to 1. The runtime is lenient with stdout, so a
stray `print` or a chatty library can't break a job:

- Blank lines are skipped.
- A line that isn't a JSON object becomes a `log` event at level `info`,
  with the line (truncated to 4 KiB) as `message`.
- An event without a `type`, with an unknown `type`, with a `v` other than
  1, or with invalid fields becomes a `log` event at level `warn`
  describing the problem.
- A second `result` or `error`, or a `result` after an `error`, is ignored
  with a warning: the first one counts.
- A `data` event whose payload is over the size or depth limits (§10) is
  dropped with a warning. A `result` whose `meta` is over them is refused,
  so the attempt fails with `no_result` rather than succeeding without its
  meta.
- `message` fields are truncated to 64 KiB.

stderr is not forwarded. The runtime keeps its tail for error reports
(§9); uv's own output lands there too.

### 8.4 Assets

Each asset in the manifest is linked into `<work dir>/assets/<name>` from
the cache, creating directories for `/`-separated names. The script finds
them through `assets` or `assets_dir`. A model that expects a directory can
be pointed at a subdirectory, e.g. `assets_dir + "/whisper"`.

### 8.5 Outcome

When the process exits, the runtime decides the outcome, taking the first
rule that applies:

1. The service cancelled the attempt: `cancelled`.
2. The owner stopped it or the app quit: `failed`, `interrupted`.
3. It ran past `timeout_ms`: `failed`, `timeout`.
4. The script sent `error`: `failed`, `script_error`, with the script's
   `message` and `retryable`.
5. It exited with a nonzero status or was killed by a signal: `failed`,
   `exit`. (uv reports a script killed by signal N as exit status 128+N.)
6. It sent no `result`: `failed`, `no_result`.
7. `result.files` names a file that isn't a declared output, or isn't a
   regular file in `out/` (a symbolic link doesn't count): `failed`,
   `output_missing`.
8. Uploading an output failed: `failed`, `output_upload`.
9. Otherwise: `succeeded`, with `meta` and each uploaded file's size and
   SHA-256.

The working directory is deleted after the outcome is reported, unless
the owner chose to keep it for debugging.

### 8.6 Stopping a process

To stop a script the runtime sends `SIGTERM` to its process group, waits
up to 10 seconds, then sends `SIGKILL` to the group. uv forwards the
`SIGTERM` it receives to the script, so a script can get it twice: signal
handlers must tolerate that, and shouldn't print, since writing to stdout
inside a handler can fail in Python. It records the
process group of every running attempt so that it can clean up processes
left over from a crash when it next starts.

## 9. Errors

A failed `done` carries `error`:

| Field | Meaning |
|---|---|
| `code` | One of the codes below. |
| `message` | For humans. |
| `retryable` | Whether another attempt could succeed. |
| `exit_code` | The process's exit status, if it exited normally. |
| `stderr_tail` | The last lines of stderr, at most 8 KiB, if a process ran. |

| Code | Sent by | Meaning | Retryable |
|---|---|---|---|
| `script_error` | machine | The script sent `error` | as the script said |
| `exit` | machine | Nonzero exit, or killed by a signal | no |
| `no_result` | machine | Exited without a `result` | no |
| `output_missing` | machine | `result.files` named an undeclared or absent file | no |
| `timeout` | machine | Ran past `timeout_ms` | no |
| `environment` | machine | `uv sync` failed | yes, unless the machine can tell it's permanent, such as a lockfile that doesn't match the script |
| `asset_download` | machine | An asset couldn't be downloaded | yes; no when it's larger than the machine accepts |
| `asset_mismatch` | machine | An asset's hash didn't match the manifest | no |
| `input_download` | machine | An input couldn't be downloaded | yes; no when the URL policy (§3) refused it, or it's larger than the machine accepts |
| `output_upload` | machine | An output couldn't be uploaded | yes; no when the URL policy (§3) refused it |
| `rejected` | machine | The assignment couldn't be read (§7.2); never started | no |
| `interrupted` | machine | Stopped by the owner, or the app quit | yes |
| `busy` | machine | Assigned while not idle; never started | yes |
| `not_approved` | machine | Bundle not approved or not stored; never started | yes |
| `internal` | machine | A bug in the runtime | yes |
| `lease_expired` | service | The lease ran out | yes |
| `lost` | service | The machine reconnected without the attempt | yes |

## 10. Limits

| What | Limit |
|---|---|
| Control message | 1 MiB |
| Script stdout line | 1 MiB; longer lines are dropped with a warning |
| `data.payload`, `result.meta` | 256 KiB each when serialized |
| Event `message` fields | 64 KiB |
| Arbitrary JSON (`params`, `payload`, `meta`) | nested at most 64 levels deep, counting containers (a scalar is 0, `[[1]]` is 2); numbers within an IEEE 754 double (§2) |
| `service.name`; `hello`'s machine `name`, `os`, `arch`, `app_version` | 64 characters each |
| `hello.machine.gpus` | 64 entries |
| `hello.approved`, `approved.hashes` | 1024 hashes; a machine with more approvals for one service announces the most recent |
| Bundle, all three files | 8 MiB |
| `stderr_tail` | 8 KiB |
| Pending pairings per service | an SDK-chosen bound, to resist abuse |

## 11. Versioning

The protocol version is the `v1` in every path and the `protocol` field of
`hello` and `welcome`. Additions that older peers can safely ignore (new
optional fields, new event types that fall back to warnings) don't change
it. Anything else gets a new version, served under a new path prefix so
that a service can support two versions side by side.

Receivers SHOULD accept unknown values of enumerations that are only
displayed (`phase`, log `level`) and show them as they are; an unknown
value of any other enumeration is a protocol error.

The manifest format (`moil` field) and the script contract (`v` field)
are versioned separately, for the same reason.
