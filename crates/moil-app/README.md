# moil-app

The tray app: how a machine's owner pairs with services, reads and approves
job bundles, sees what runs, and pauses or stops work. macOS first; the
platform-specific parts are small and live behind `cfg` in `platform.rs` and
`login.rs`.

## Run it

```sh
cargo run -p moil-app -- --demo    # a simulated agent; touches nothing on this machine
cargo run -p moil-app              # the real agent, on $MOIL_HOME or ~/.moil
```

The demo has two services: klisi, connected, offering `transcribe 0.3.0` to
replace an approved 0.2.0 (so the review opens with a diff), and lab, which
keeps failing to connect. Jobs for klisi come and go through every phase,
and one in three runs out of GPU memory, so the menu has a failure to show.
Pairing with any address succeeds after a few seconds with the code
`WDJB-MJHT`; hosts starting with `deny` are refused, and hosts ending in
`.invalid` aren't moil services.

The app logs to stderr and to `~/Library/Logs/moil/moil.log` (the demo to
`$TMPDIR/moil-demo/moil.log`), which starts over past 4 MB, keeping the
previous one as `moil.log.1`. "Show Log in Finder" in the menu finds it;
`RUST_LOG` sets the level.

Outside the app bundle the real agent needs uv on `PATH` or in `MOIL_UV`,
and Open at Login is unavailable.

## Build Moil.app

```sh
packaging/macos/build.sh
open target/macos/Moil.app
```

The script builds `moil-app` and `moil` in release mode, downloads the uv
release pinned in the script for this Mac's architecture and checks its
SHA-256, draws the icon (`packaging/macos/icon.swift`), and assembles and
ad-hoc signs `target/macos/Moil.app`:

```text
Moil.app/Contents/
  Info.plist        works.convex.moil, LSUIElement, the moil:// URL scheme
  MacOS/moil-app    the app
  MacOS/moil        the command line
  MacOS/uv          found by the runtime next to the executable
  Resources/Moil.icns
```

It needs Rust, the Xcode command line tools and, the first time, network
access. Copy the app to `/Applications` to keep it; Open at Login registers
the bundle with `SMAppService`. Try a link with
`open 'moil://pair?url=https://klisi.example.com/moil'`: it opens the pair
window with the address filled in, and nothing happens until the owner
presses Continue.

## How it fits together

The decisions are plain code with tests; the event-loop glue only carries
events to them and carries out what they ask for.

| Module | What |
|---|---|
| `backend` | `Backend`, what the UI needs from the agent; its implementation for `AgentHandle`, and real-mode startup |
| `log` | the log on stderr and in a file that starts over past a size |
| `demo` | `Demo`, the simulated agent behind `--demo`, and the demo bundles in `demo/` |
| `view` | `TrayView::new`: a `Snapshot` becomes the menu model, the icon's state and the title |
| `tray` | makes a `TrayView` real with tray-icon and muda, relabelling in place when only text changed |
| `icon` | draws the tray icon as a template image |
| `review`, `pair`, `unpair` | one window each: its state machine and its page |
| `page` | the page shell (policy, style, script) and helpers for untrusted text |
| `ipc`, `links` | strict parsing of page messages, `moil://` links and navigation |
| `snapshots` | the agent's snapshots, taken by the event loop when it gets to them, however fast they come |
| `app` | the tao event loop, wry windows and Tokio tasks |

Pages show text a service controls, so every page is escaped by maud, runs
under a Content Security Policy that loads nothing and runs only its own
nonce'd script and style, can't navigate away (web links open in the
browser), and can only send the few messages its window understands.
Characters that are invisible or reorder text are shown as markers, the way
`moil review` shows them (`␍`, `\u{202e}`).
Approving sends the full bundle hash, and the review window refuses it unless
it names the bundle shown.

## Tests

```sh
cargo test -p moil-app
cargo test -p moil-app --test demo_starts -- --ignored     # launches the demo; shows a tray icon
```

To look at every page and the tray icons, write them to files and snapshot
them offscreen with WebKit:

```sh
MOIL_APP_GALLERY=/tmp/gallery cargo test -p moil-app gallery -- --ignored
swift crates/moil-app/tools/snapshot.swift /tmp/gallery/review-update.html /tmp/review.png 900 dark
swift crates/moil-app/tools/snapshot.swift /tmp/gallery/review-update.html /tmp/packages.png 900 light '#packages' 900
```
