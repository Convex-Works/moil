# How moil works

moil lets the people who use a service lend it their machines, one approved
job at a time. The name is plain English for "to toil".

[spec/protocol.md](../spec/protocol.md) defines the wire, and
[spec/machine.md](../spec/machine.md) defines the machine side. This page
explains the idea.

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
  short-lived URLs, such as pre-signed S3 URLs, not over the control
  channel.

## Architecture

```
┌────────────────────┐        WebSocket        ┌──────────────────────┐
│  Service (tide)    │◀── opened by machine ───│   moil app (Rust)    │
│  + Go SDK          │                         │   tray, agent        │
└────────────────────┘                         └──────────┬───────────┘
         │                                                │ stdin / stdout
         │ pre-signed URLs                                ▼
         ▼                                     job runtime: uv run --locked
   object storage  ◀──── inputs and outputs ────  job.py
```

**Pairing.** In the app, the owner enters the service's moil address. The
app shows a short code and opens the service's confirmation page, where the
owner, already signed in, checks the code and confirms. That is the device
flow `gh auth login` uses, and the app never sees the owner's password. The
machine's token goes into the keychain.

**Bundles.** A job bundle is three files:

| File | Contents |
|---|---|
| `job.py` | The script, with its dependencies declared inline (PEP 723). |
| `job.py.lock` | Every package at an exact version, with the hash of every artifact (`uv lock --script job.py`). |
| `manifest.json` | Name, version, description, model weights to download (URL and SHA-256) and the resources it needs. |

The bundle hash is the SHA-256 of their `sha256sum` listing, so anyone can
check it by hand:

```sh
shasum -a 256 job.py job.py.lock manifest.json | shasum -a 256
```

**Approval.** When a service offers a new bundle, the app's review window
shows the three files in full, every package the lockfile installs, what
the manifest downloads, and what changed since the version the owner
approved before. Approving stores that exact copy; jobs only ever run from
it, re-checked before each run.

**Jobs.** The service offers a job to the eligible machines that approved
its bundle, picks one of those that bid, and assigns it. The machine's
runtime prepares the environment with `uv sync --locked`, downloads and
verifies the assets and inputs, runs `uv run --locked --script job.py` in
its own process group, and uploads the outputs. The script reads the job
from stdin and writes one JSON event per line to stdout:

```jsonl
{"v":1,"type":"progress","fraction":0.42,"message":"diarizing"}
{"v":1,"type":"data","payload":{"speaker":1,"text":"Shall we start?"}}
{"v":1,"type":"result","files":["transcript.json"],"meta":{"speakers":4}}
```

It never sees the WebSocket, the machine's token or any URL. Leases,
sequence numbers and acknowledgements let an attempt survive a dropped
connection, and a machine that disappears loses its job to another.

## The home

A service folder kept in git moves your approvals and hooks to your other
machines. `.gitignore` keeps out what belongs to one machine alone. The
token is never in a service's folder: it stays in the keychain (in
`.machine/` off macOS), kept with the one URL it may be sent to.
