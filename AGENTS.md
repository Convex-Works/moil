# moil

A protocol, a server SDK (Go), a job runtime and a tray app (Rust, macOS
first) that let people lend their machines to web services, one approved
job bundle at a time. README.md explains the idea. **spec/protocol.md
(the wire) and spec/machine.md (the home, hooks and command line) are the
contract**. Read them before changing anything that crosses a boundary.

## Layout

| Path | What |
|---|---|
| `spec/protocol.md` | The normative protocol spec |
| `spec/machine.md` | The machine side: the home's files, hooks, the command line and its JSON |
| `spec/vectors/` | Shared test vectors: messages, manifests, bundles and their hashes |
| `crates/moil-proto` | Rust wire types, checked against the vectors |
| `crates/moil-bundle` | Bundles: validation, hashing, lockfile listing, diffs, the on-disk store |
| `crates/moil-runtime` | Runs attempts: uv environments, assets, inputs, the script, outputs |
| `crates/moil-agent` | The machine side: home, secrets, pairing, approvals, control channels |
| `crates/moil-cli` | The `moil` binary: bundle authoring commands and a headless machine |
| `crates/moil-app` | The tray app (macOS) |
| `sdk/go` | The Go server SDK (`moil`) and a fake machine for service tests (`moiltest`) |
| `e2e/` | End-to-end tests: the Go SDK against the real `moil` binary and real uv |
| `examples/` | Example bundles and an example service |

## Rules that bite

- A change to anything on the wire (messages, HTTP bodies, the manifest,
  the bundle hash, the script contract) changes `spec/protocol.md`, the
  vectors, and both implementations in the same commit. The vector tests
  in `moil-proto`, `moil-bundle` and `sdk/go` must keep passing.
- A change to the home's files, the hook contract, or what a command
  prints changes `spec/machine.md` in the same commit.
- Machines never trust what a service claims: hashes are recomputed from
  the files, bundle names shown for approval come from the verified
  manifest, and every run re-checks the stored bundle.
- The script never sees the control channel, the machine token or URLs.
- Everything the app can do, the `moil` command can do too, including
  while the app runs: agents drive moil through the CLI. A change that
  adds something to the app adds its command in the same commit.
- Tests exercise behavior end to end where they can: real uv, real
  processes, real sockets and HTTP. A test that only restates the
  implementation isn't worth having. Keep them fast and deterministic: no
  fixed sleeps where a condition can be awaited.
- `make check` must pass before any commit.

## Style

- Match the surrounding code's comment density and idioms. Doc comments
  say what a thing is for and what it guarantees, not how it works.
- Errors shown to the machine owner are sentences they can act on.
- Rust: edition 2024, `cargo fmt`, clippy clean with `-D warnings`.
- Go: `gofmt`, `go vet`, standard library first. The SDK must stay
  importable by services on Go 1.24.
