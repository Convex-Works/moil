# moil

Lend your machine to the services you use, one approved job at a time.

A web service, such as [tide], posts a job: "transcribe this meeting with
speaker labels". A machine that its owner paired with the service runs a
script the owner approved. The machine streams progress and results back.

moil is a protocol, a Go server SDK and a tray app. It is for self-hosted
services that need occasional heavy compute, and for the people who use them.

> Status: alpha. Machines run on macOS only. The protocol is at version 1.

moil has no sandbox. An approved script runs with your permissions, so read
each bundle before you approve it. See the [trust model](docs/trust-model.md).

## Try it

You need Rust 1.90 or later, Go 1.24 or later, and
[uv](https://docs.astral.sh/uv/).

1. Start the example service. It publishes the `echo` bundle.

   ```sh
   cd examples/service && go run . -bundles ../bundles/echo
   ```

2. In a second terminal, start the app.

   ```sh
   cargo run -p moil-app
   ```

3. Pair the app with `http://127.0.0.1:8787/moil`.
4. Confirm the code on the page that opens.
5. Review and approve `echo` from the tray menu.
6. Submit a job from http://127.0.0.1:8787.

To see the app without a service, run `cargo run -p moil-app -- --demo`.

To build `Moil.app`, run `packaging/macos/build.sh`.

## Use the command line

The `moil` command does everything the app does, while the app runs too.
Scripts and agents can use it. `Moil.app` links it to `~/.local/bin/moil`.
From source, run `cargo build -p moil-cli` and use `target/debug/moil`.

```sh
moil pair http://127.0.0.1:8787/moil
moil review 127.0.0.1-8787                # list the service's bundles
moil approve 127.0.0.1-8787 <hash>        # read it in full, then approve
moil status                               # what moil is doing; --json for scripts
moil agent                                # without the app: take jobs until Ctrl-C
```

Run `moil --help` for every command.

## Use hooks

Everything moil runs or acts on is in one folder, `~/.moil`. You, your editor
or an LLM can read it, and you can keep it in git.

```
~/.moil/
  settings.json
  services/acme.example.com/
    service.json                 the service, and the bundles you approved for it
    bundles/transcribe/<hash>/   each approved bundle's files, read-only, checked before every run
    hooks/transcribe.py          yours: runs after each transcribe job that succeeds here
    logs/transcribe.log          what that hook printed
```

A hook makes a job's results yours. After a job succeeds on your machine, moil
runs your executable `services/<service>/hooks/<bundle>`. The hook gets the
job's context on stdin and its outputs in a folder. The service never learns
that a hook exists.

The [example hook](examples/hooks/meeting-notes.py) turns a transcript into
meeting notes in an Obsidian vault. Run `moil hooks --help` for the whole
contract.

## Write a bundle

A bundle is a script, its lockfile and a manifest.

```sh
moil init mybundle        # job.py, manifest.json and a lockfile to start from
moil check mybundle       # valid, lockfile current, what it installs and downloads
moil run mybundle --input audio.ogg=meeting.ogg --output transcript.json
```

`moil run` runs the bundle exactly as a machine does. A local HTTP server
moves the inputs and outputs.

Start from [examples/bundles/echo](examples/bundles/echo), which uses every
part of the script contract.

The bundle author makes the script work on each platform (CUDA, Apple MPS, CPU).

## Add moil to a service

```go
srv, _ := moil.NewServer(moil.Config{Name: "acme", VerificationURL: "https://acme.example.com/machines/pair", Store: store})
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

The [Go SDK guide](sdk/go/README.md) covers how to onboard your users, the
pairing page, bundles, jobs and tests.

## Documentation

- [How moil works](docs/how-it-works.md): the design, pairing, bundles, approval and jobs.
- [Trust model](docs/trust-model.md): what approving a bundle means, and what moil protects against.
- [Machine spec](spec/machine.md): the home's files, settings, hooks and the command line.
- [Protocol spec](spec/protocol.md): what machines and services say to each other.
- [Go SDK guide](sdk/go/README.md): add moil to a service.
- [Tray app](crates/moil-app/README.md): run, build and test the app.
- [AGENTS.md](AGENTS.md): the repository layout, and the rules for changing moil.

## License

MIT, see [LICENSE](LICENSE).

[tide]: https://github.com/Convex-Works/tide
