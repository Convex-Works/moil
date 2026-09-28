# moil

Lend your machine to the services you use, one approved job at a time.

moil is a small protocol, a server SDK and a tray app. A web service (e.g.
[klisi], a self-hosted video meeting app) posts a job, such as "transcribe
this meeting with speaker labels". One of the machines its users have
paired picks up the job, runs a script the machine's owner has read and
approved, and streams progress and results back.

The name is plain English for "to toil".

> Status: alpha. Everything below works end to end on macOS; Linux and
> Windows machines come later. [spec/protocol.md](spec/protocol.md) is
> version 1 of the protocol.

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

## How it works

```
┌────────────────────┐        WebSocket        ┌──────────────────────┐
│  Service (klisi)   │◀── opened by machine ───│   moil app (Rust)    │
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

## Try it

You need Rust, Go 1.24 or later, and [uv](https://docs.astral.sh/uv/).

Start the example service, which publishes the `echo` bundle:

```sh
cd examples/service && go run . -bundles ../bundles/echo
```

Then run the app and pair it with `http://127.0.0.1:8787/moil`:

```sh
cargo run -p moil-app             # or: packaging/macos/build.sh && open target/macos/Moil.app
```

Confirm the code on the page that opens, review and approve `echo` from
the tray menu, and submit a job from http://127.0.0.1:8787. The same works
from a terminal, while the app runs or without it:

```sh
moil pair http://127.0.0.1:8787/moil
moil review 127.0.0.1-8787                # list the service's bundles
moil approve 127.0.0.1-8787 <hash>        # read it in full, then approve
moil status                               # what moil is doing; --json for scripts
moil agent                                # without the app: take jobs until Ctrl-C
```

`cargo run -p moil-app -- --demo` shows the app with a simulated agent.

## Your machine, as files

Everything moil runs or acts on is in one folder, `~/.moil`, which you,
your editor or an LLM can read, and which you can keep in git
([spec/machine.md](spec/machine.md)):

```
~/.moil/
  settings.json
  services/klisi.example.com/
    service.json                 the service, and the bundles you approved for it
    bundles/transcribe/<hash>/   each approved bundle's files, read-only, checked before every run
    hooks/transcribe             yours: runs after each transcribe job that succeeds here
    logs/transcribe.log          what that hook printed
```

**Hooks** are how a job's results become yours. After a job of a bundle
succeeds on your machine, moil runs your executable
`services/<service>/hooks/<bundle>` with the job's context on stdin and
its outputs in a folder. The service never learns a hook exists. The
[example hook](examples/hooks/meeting-notes.py) turns a klisi transcript
into meeting notes in an Obsidian vault; `moil hooks --help` has the whole
contract.

**The command line does everything the app does**, while the app runs
too, so scripts and agents can drive moil: `moil status`, `pause`,
`stop`, `pair`, `review`, `approve`, `hooks`, `logs` and the rest, each
with `--json`. A service folder kept in git moves your approvals and
hooks to your other machines; `.gitignore` keeps out what belongs to one
machine alone, and the token never leaves the keychain.

## Write a bundle

```sh
moil init mybundle        # job.py, manifest.json and a lockfile to start from
moil check mybundle       # valid, lockfile current, what it installs and downloads
moil run mybundle --input audio.ogg=meeting.ogg --output transcript.json
```

`moil run` runs the bundle exactly as a machine would, with inputs and
outputs moving over a local HTTP server. [examples/bundles/echo](examples/bundles/echo)
uses every part of the script contract and is the place to start;
[examples/bundles/transcribe](examples/bundles/transcribe) transcribes a
meeting with speaker labels, using Nemotron 3 Diarization and Parakeet.

Making a script work on every platform (CUDA, Apple MPS, CPU) is the
bundle author's job.

## Add moil to a service

```go
srv, _ := moil.NewServer(moil.Config{Name: "klisi", VerificationURL: "https://klisi.example.com/machines/pair", Store: store})
mux.Handle("/moil/", http.StripPrefix("/moil", srv.Handler()))

bundle, _ := moil.LoadBundle(os.DirFS("bundles/transcribe"))
srv.AddBundle(bundle)

run, _ := srv.Submit(ctx, moil.Job{
	Bundle:   bundle,
	Inputs:   map[string]moil.Download{"audio.ogg": {URL: presignedGet}},
	Outputs:  map[string]moil.Upload{"transcript.json": {URL: presignedPut}},
	Eligible: moil.OwnedBy(room.OwnerID),
})
for ev := range run.Events(ctx) { /* progress, live data */ }
result, err := run.Wait(ctx)
```

[sdk/go/README.md](sdk/go/README.md) covers onboarding your users (where
to get the app, `moil.AppURL`, and the three steps to show them), the
pairing page, publishing bundles, submitting jobs and testing a service
with `moiltest`, a fake machine.

## Trust model

moil runs arbitrary code. Approving a bundle hash means "I trust exactly
this code, and the exact dependencies it pins", in the same way installing
an app means trusting its author.

- A compromised service can't swap the code: the machine verifies the hash
  locally before every run, and shows only what it verified.
- Nor the dependencies: the lockfile must pin every artifact by hash and
  come from https sources, and packages that have wheels install only
  from those wheels. A package that must be built from source is marked in
  the review, because its build tools aren't pinned.
- The review marks characters that could hide code from a reader:
  carriage returns, escape sequences, bidirectional overrides and
  invisible characters.
- A compromised service *can* send malicious inputs to an approved script,
  so scripts should treat inputs as untrusted. It can't aim a job's
  downloads and uploads at services that listen only on the owner's
  machine: plain `http` to loopback is only for services on loopback.
- Whoever runs a machine can see the job's inputs. Services must only offer
  sensitive data (e.g. meeting audio) to machines their policy allows,
  typically the data owner's own.
- There is no sandbox yet. The review window says so plainly.
- Hooks are your own code: nobody approves them, and they run as you.
  What they're given comes partly from the service (a job's title and
  params, its outputs' names and contents), so a hook should treat it as
  untrusted. Whoever can push to a service folder you keep in git can
  approve bundles and add hooks on your machines.

## Repository

| Path | What |
|---|---|
| [`spec/`](spec) | The protocol spec and the test vectors both implementations check against |
| [`sdk/go`](sdk/go) | The Go server SDK and `moiltest` |
| [`crates/moil-app`](crates/moil-app) | The tray app (macOS) |
| `crates/moil-cli` | The `moil` command line |
| `crates/moil-agent` | The machine side: pairing, approvals, control channels |
| `crates/moil-runtime` | Runs attempts: uv environments, assets, the script, outputs |
| `crates/moil-bundle`, `crates/moil-proto` | Bundles and wire types |
| [`e2e/`](e2e) | End-to-end tests: the Go SDK against the real `moil` binary and uv |
| [`examples/`](examples) | Example bundles, a demo service, and example hooks |

`make check` runs everything: formatting, lints, the Rust and Go tests and
the end-to-end suite.

## Later

- The app on Linux and Windows; a signed and notarized macOS build.
- Trusting a publisher's signing key instead of approving each hash.
- Sandboxing: `sandbox-exec` on macOS, Landlock or bubblewrap on Linux, Job
  Objects on Windows.
- Jobs that survive a service restart; server SDKs in other languages.
- Bundles in languages other than Python.

[klisi]: ../klisi

## License

MIT, see [LICENSE](LICENSE).
