# moil-app

The tray app: how a machine's owner pairs with services, reads and approves
job bundles, sees what runs, and pauses or stops work. It runs the agent on
the home (`~/.moil`, spec/machine.md) and serves the control socket, so that
the `moil` command does everything the app does while it runs. It runs on
macOS. The platform-specific parts are small and live behind `cfg` in
`platform.rs`, `login.rs` and `local_bin.rs`.

## Run it

```sh
cargo run -p moil-app -- --demo    # a simulated agent; touches nothing on this machine
cargo run -p moil-app              # the real agent, on $MOIL_HOME or ~/.moil
```

The demo has two services: tide, connected, offering `transcribe 0.3.0` to
replace an approved 0.2.0 (so the review opens with a diff), and lab, which
keeps failing to connect. Jobs for tide come and go through every phase,
and one in three runs out of GPU memory, so the menu has a failure to show.
tide has a hook; lab has one that isn't executable, which the menu shows as
a problem until lab is unpaired. Pairing with any address succeeds after a
few seconds with the code `WDJB-MJHT`; hosts starting with `deny` are
refused, and hosts ending in `.invalid` aren't moil services. The demo
serves no control socket, so commands don't reach it.

The app logs to stderr and to `logs/moil.log` in the home (the demo's home
is `$TMPDIR/moil-demo`), which starts over past 4 MB, keeping the previous
one as `moil.log.1`. "Show Log in Finder" in the menu finds it; `RUST_LOG`
sets the level.

Outside the app bundle the real agent needs uv on `PATH` or in `MOIL_UV`,
Open at Login is unavailable, and the app leaves `~/.local/bin` alone.

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
the bundle with `SMAppService`. When it starts, the app links
`~/.local/bin/moil` to its `Contents/MacOS/moil`, if nothing is there or a
link into a `Moil.app` is; it never replaces anything else, and logs why it
left the path alone. Try a link with
`open 'moil://pair?url=https://tide.example.com/moil'`: it opens the pair
window with the address filled in, and nothing happens until the owner
presses Continue.

## How it fits together

The decisions are plain code with tests; the event-loop glue only carries
events to them and carries out what they ask for.

| Module | What |
|---|---|
| `backend` | `Backend`, what the UI needs from the agent; its implementation for `Hosted`, and real-mode startup |
| `demo` | `Demo`, the simulated agent behind `--demo`, and the demo bundles in `demo/` |
| `view` | `TrayView::new`: a `Snapshot` and the home's report become the menu model, the icon's state and the title |
| `tray` | makes a `TrayView` real with tray-icon and muda, relabelling in place when only text changed |
| `icon` | draws the tray icon as a template image |
| `format` | numbers and times as the owner reads them |
| `review`, `pair`, `unpair` | one window each: its state machine and its page |
| `page` | the page shell (policy, style, script) and helpers for untrusted text |
| `ipc`, `links` | strict parsing of page messages, `moil://` links and navigation |
| `snapshots` | the agent's snapshots and the home's reports, taken by the event loop when it gets to them, however fast they come |
| `login` | Open at Login, for the menu and `moil login-item` |
| `local_bin` | the rule for linking `~/.local/bin/moil` |
| `gallery` | writes every page and the tray menu to files, for a look at them |
| `app` | the tao event loop, wry windows and Tokio tasks; `Controls`, which carries `moil quit` and `moil login-item` into the event loop |

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
cargo test -p moil-app --test commands -- --ignored        # the real app on a temporary home, driven by commands
```

To look at every page, the tray menu and the tray icons, write them to files
and snapshot them offscreen with WebKit:

```sh
MOIL_APP_GALLERY=/tmp/gallery cargo test -p moil-app gallery -- --ignored
swift crates/moil-app/tools/snapshot.swift /tmp/gallery/review-update.html /tmp/review.png 900 dark
swift crates/moil-app/tools/snapshot.swift /tmp/gallery/review-update.html /tmp/packages.png 900 light '#packages' 900
```
