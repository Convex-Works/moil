# moil

Lend your machine to the services you use, one approved job at a time.

moil is a small protocol and a tray app. A web service (e.g. [klisi], a
self-hosted video meeting app) posts a job, such as "transcribe this meeting
with speaker labels". One of the machines its users have paired picks up the
job, runs a script the machine's owner has read and approved, and streams
progress and results back.

The name is plain English for "to toil".

> Status: design. Nothing here is implemented yet.

## Why

Self-hosted services often need occasional heavy compute (speech-to-text,
diarization, embeddings) that the server itself lacks, while their users
have idle laptops and desktops with capable GPUs. moil lets those users
offer that compute explicitly, per service and per script, without opening
ports or trusting the service with anything they haven't read.

## Principles

- **Explicit consent, pinned by hash.** A machine runs only job bundles
  whose exact hash its owner approved after reading the whole bundle. A new
  version means a new hash and a new approval.
- **Machines open every connection.** Machines only make outbound
  connections: no open ports, no NAT traversal.
- **Services decide eligibility; machines decide consent.** Which machines
  may receive a given job (e.g. only the meeting owner's) is the service's
  policy. moil only enforces what the machine's owner approved.
- **Large data moves out of band.** Inputs and outputs travel as
  short-lived pre-signed URLs, not over the control channel.

## Architecture

moil has three layers, all built against one protocol spec:

```
┌────────────────────┐        WebSocket        ┌──────────────────────┐
│  Service (klisi)   │◀── opened by machine ───│   moil app (Rust)    │
│  + server SDK (1)  │                         │   tray, supervisor   │
└────────────────────┘                         │  (3)                 │
         │                                     └──────────┬───────────┘
         │ pre-signed URLs                                │ stdin / stdout
         ▼                                                ▼
   object storage                               job bundle runtime (2)
                                                uv run --locked job.py
```

### 1. Server SDK

The library a service embeds to take part in the protocol. Go comes first,
since that's what klisi uses.

- **Pairing:** the user clicks "add machine" in the service and confirms a
  short code in the tray app (a device-code flow, like `gh auth login`).
  Each machine gets its own token.
- **Machine registry:** which machines are paired, online, and which bundle
  hashes they have approved.
- **Eligibility hook:** `Eligible(job, machine) bool`, implemented by the
  service with its own authorization rules.
- **Jobs:** `Submit(job)` returns a stream of events. The SDK handles offers,
  bids, assignment, time limits (leases) and retries.

### 2. Job bundle runtime

A job bundle is three files:

| File | Contents |
|---|---|
| `job.py` | The script. Dependencies are declared inline (PEP 723). |
| `uv.lock` | Every package at an exact version, with the sha256 of each wheel for every platform (`uv lock --script`). |
| `manifest.json` | Name, the job types it handles, model weights by URL and sha256, and the resources it needs. |

The **bundle hash** is the sha256 over the three files combined in a
canonical way. The app computes it itself from the downloaded files and
never trusts a hash the service claims. uv also provides Python, so the app
only ships the `uv` binary. It runs the script with `uv run --locked`, which
installs exactly what the lockfile pins, with hashes checked.

Making the script work on every platform (CUDA, Apple MPS, CPU) is the
bundle author's job.

**Script ↔ runtime contract.** The job's JSON arrives on stdin. The script
writes one JSON event per line to stdout:

```jsonl
{"v":1,"type":"progress","fraction":0.42,"message":"diarizing"}
{"v":1,"type":"log","level":"info","message":"loaded model"}
{"v":1,"type":"data","payload":{}}
{"v":1,"type":"result","files":["transcript.json"],"meta":{}}
{"v":1,"type":"error","message":"CUDA OOM","retryable":true}
```

- `progress`, `log`, `result` and `error` are typed: the app and the SDK
  use them for the tray progress bar, lease renewal and retry decisions.
- `data` carries arbitrary service-specific JSON, passed through untouched.
- Files listed in `result` are written to the working directory and
  uploaded by the runtime.
- A job succeeds only if the script sends a `result` event. Exit code 0
  without one counts as a failure.

The script never sees the WebSocket, the machine token or the service.

### 3. Native app

A Rust tray app (`tray-icon` + `tao`, `tokio`, `tokio-tungstenite`,
`keyring`, `auto-launch`).

- Keeps one WebSocket per paired service. The open connection also tells
  the service the machine is online.
- **Approval screen:** shows the service, the full `job.py`, the full
  `uv.lock` (every package, with links), `manifest.json`, the bundle hash,
  and a diff against the version approved before.
- Supervises job processes: timeouts, cancellation, killing the whole
  process tree, and a working directory per job.
- Stores tokens in the OS keychain and can start at login.

## Protocol

One WebSocket per machine per service, opened by the machine:

```
machine ──▶ service   hello   {machine_id, approved_hashes, hw: {gpu, ram}, idle}
service ──▶ machines  offer   {job_id, bundle_hash, requirements}
machine ──▶ service   bid     {job_id, fitness}             only if approved + idle
service ──▶ machine   assign  {job_id, inputs, lease_ttl}   one machine
machine ──▶ service   event   {job_id, ...}                 script events; each renews the lease
machine ──▶ service   done    {job_id, exit_code}
```

- The service picks among bids (e.g. prefer the owner's machine, a GPU, or
  an idle machine) rather than taking whichever claims first.
- If the socket drops or the lease runs out, the job is offered again. Job
  IDs make repeated deliveries safe.
- `inputs` hold pre-signed URLs for downloads and uploads.

The full spec, with message schemas, pairing flow, bundle canonicalization
and versioning, is the next thing to write.

## Trust model

moil runs arbitrary code. Approving a bundle hash means "I trust exactly
this code, and the exact dependencies it pins", in the same way installing
an app means trusting its author.

- A compromised service can't swap the code: the app verifies the hash
  locally before every run.
- A compromised service *can* send malicious inputs to an approved script,
  so scripts should treat inputs as untrusted.
- Whoever runs a machine can see the job's inputs. Services must only offer
  sensitive data (e.g. meeting audio) to machines their policy allows,
  typically the data owner's own.
- v1 has no sandbox. The approval screen says so plainly.

## Example: klisi meeting transcription

1. A meeting ends and LiveKit Egress writes the recording to S3.
2. klisi submits a `transcribe+diarize` job. Its eligibility hook allows
   only machines the room owner has paired.
3. The owner's desktop bids and is assigned the job. It downloads the audio
   through a pre-signed URL and runs a bundle pairing a speech-to-text model
   with Nemotron 3 Diarization, merging who spoke when with what was said.
4. The bundle streams progress to klisi and uploads `transcript.json`.

## Later

- Trusting a publisher's signing key instead of approving each hash.
- Sandboxing: Landlock/bubblewrap on Linux, `sandbox-exec` on macOS, Job
  Objects on Windows.
- Server SDKs in other languages.
- Bundles in languages other than Python.

[klisi]: ../klisi

## License

MIT, see [LICENSE](LICENSE).
