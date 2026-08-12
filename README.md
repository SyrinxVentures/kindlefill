# KindleFill

Fills a Kindle's storage down to a target free-space window (default 50–90 MB) so the
device can't download an OTA firmware update, and removes the filler again afterwards.

Targets modern MTP Kindles (11th gen and newer — 2024 Paperwhite, Colorsoft, Scribe,
basic 2024), which no longer mount as USB mass storage.

> **Not created or endorsed by Amazon.** KindleFill is an independent, free, open-source
> tool. *Kindle* and *Amazon* are trademarks of Amazon.com, Inc. or its affiliates, used
> here only to say which device this works with.

![The KindleFill window during a fill: a bar showing how much of the Kindle is filler
this tool wrote, the target range, and a live progress reading with throughput and time
remaining.](docs/screenshot.png)

## Install

Download the latest **KindleFill_*_aarch64.dmg** from
[Releases](https://github.com/SyrinxVentures/kindlefill/releases/latest), open it, and drag
KindleFill to Applications. Apple silicon only; see [Platforms](#platforms) below.

Released builds are signed with a Developer ID and notarized by Apple, so they open on a
double-click with no warning to dismiss.

<details>
<summary>If macOS blocks it anyway</summary>

That means you have an unnotarized build — one you compiled yourself, or from before
notarization was set up. macOS quarantines anything downloaded, and since macOS Sequoia
the old right-click → Open escape no longer works. What does:

1. Double-click KindleFill. macOS says it can't be opened. Click **Done** — not *Move to
   Trash*.
2. Open **System Settings → Privacy & Security**, scroll to the bottom, and click
   **Open Anyway** next to the message about KindleFill.
3. Confirm with your password.

That button only appears for about an hour after a blocked launch; if it isn't there,
double-click the app again to bring it back. Or, from a terminal:

```bash
xattr -d com.apple.quarantine /Applications/KindleFill.app
```

A bundle you build locally is never quarantined in the first place.
</details>

## Platforms

macOS is the only one this has ever been compiled or run on, and the released DMG is
**Apple silicon only** — no Intel build, because there is no Intel Mac here to test one
on. Windows and Linux are not "supported but untested"; they are untried. The code
carries real affordances for both (`windows_subsystem`, a no-op `ptpcamerad` off macOS,
an `.ico` in the bundle set), which makes it look like someone checked, and nobody has.
The unknown isn't this app's code, it's whether `mtp-rs`'s WPD and libusb backends
behave the way the macOS one does. Reports either way are welcome.

**Sizes are binary.** 1 GB here means 1 GiB — 1024³ bytes — throughout: the CLI's
`--low 50MB`, the app's fields, and every figure printed. So the capacity shown for a
device will read lower than the number on its box, which is quoted in SI GB. Internally
it is consistent — `parse_size`, `human_bytes` and the frontend all use 1024 — so no
arithmetic depends on which convention you assume; only the labels are non-SI.

## Status

Validated against a **Kindle Paperwhite Signature Edition** (25.46 GB usable) on macOS.
Every assumption the design rested on held:

| Question | Answer |
|---|---|
| Does the device enumerate over MTP? | Yes — no driver, no helper app |
| Does `ptpcamerad` need working around? | **It wasn't running at all** — no admin prompt needed |
| Does reported free space track writes? | **Exactly.** 16/128/512 MiB writes each moved it by precisely that much |
| Per-file overhead | 0 bytes at 16 and 128 MiB; one 4 KB block at 512 MiB |
| Throughput | 25–30 MB/s, settling near 25.5 MB/s on large objects |

That last row puts a full fill on a 32 GB Kindle at roughly **17 minutes**.

The free-space result is the one that mattered. It was the single assumption that could
have invalidated the whole approach — a device caching `GetStorageInfo` would leave the
measure-write-remeasure loop unable to converge, and it would have passed every test in
this repo before failing on the cable. It doesn't cache. The stray 4 KB block is exactly
the kind of overhead the loop absorbs by measuring instead of tallying.

The GUI has since been run against the same device, driven end to end:

| Check | Result |
|---|---|
| Detects the device on launch | Yes — 25.46 GB capacity, 24.95 GB free, no filler |
| Pre-flight estimate | 24.88 GB to write, ~17 min |
| Progress during a 1 GiB object | 317 events, **largest gap 0.215 s** — the bar never freezes |
| Stop, pressed mid-object | Responded in **under 0.9 s**, including deleting the partial object |
| Fill again after Stop | Resumed — wrote `fill_0001.bin`, not `fill_0000.bin` |
| Remove filler & folder | Free space returned to its starting value; `fill_disk` removed |

Those runs used a window shifted ~2.5 GB below current free space rather than the
50–90 MB default. Same code path — the ladder still lays down 1 GiB objects, so Stop is
still tested mid-object — without parking the device at 70 MB free for the duration.

Three paths have *not* been exercised on hardware and rest on the test suite alone —
see [What has and hasn't been exercised on hardware](#what-has-and-hasnt-been-exercised-on-hardware).

## Why this isn't just "copy some big files over"

Three things make it more than a drag-and-drop:

**macOS doesn't refuse MTP — a daemon squats on it.** `ptpcamerad`, Apple's PTP camera
daemon, exclusively claims any MTP device the moment it's plugged in. It only speaks
PTP, so it can't talk to a Kindle; it just stops anything else from doing so. This tool
kills it repeatedly for the duration of a transfer and lets launchd restart it after,
rather than disabling it permanently and quietly breaking camera import.

It didn't turn out to be running on the machine this was validated on, so the handling
is defensive rather than load-bearing — but it costs nothing when idle, and the failure
it prevents is otherwise a baffling permission error with no obvious cause.

**Fill converges from either side.** Writing can only ever reduce free space, so a
device that ends up *under* the target — because you moved the window, or the Kindle
grew its own files — used to be stuck: the only remedy was deleting every filler
object and starting over, seventeen minutes to gain a few megabytes. Below the window,
`fill` now removes filler instead, then writes back down.

Deleting alone isn't enough, which is why it's one loop rather than a separate
"trim" command. Filler is written in coarse rungs, so on a real 25 GB device the
smallest object may be 64 MiB while the window is 20 MB wide — removing one takes you
from 87 MB free to 154 MB, straight past the target. So a removal deliberately
overshoots and the ladder converges back down. It can't oscillate: `next_step` never
proposes a write that lands below `low`, so once a removal reaches the window, writes
can't push back under it. `plan.rs` has that as a property test.

**Landing in the window is a control problem, not arithmetic.** A 40 MB window on a
25 GB device is a 0.16% target, and every file costs more than its own bytes in ways the host
can't predict. So the loop measures, writes, and re-measures — never tallies. See
`plan.rs`.

**Compression can't help.** The prebuilt filler ZIPs floating around compress well
because they're archives of zeros, but they're extracted *on the host* and the full
size still crosses the USB wire. Nothing can shortcut that: MTP has no compressed
transfer mode, and making the Kindle expand an archive needs code execution on the
device — exactly what you don't have before jailbreaking.

We do avoid the ZIP workflow's other costs. It expands the archive onto your SSD and
reads it all back to send — on a 32 GB Kindle that's ~25 GB written and ~25 GB read
locally, and you need the space free to begin with. This generates zeros in memory and
streams them straight into the upload, so the host never stores a byte.

## The app

```bash
cargo run -p kindlefill-app                    # run it from source
cd crates/kindlefill-app && cargo tauri build   # build a double-clickable .app and .dmg
```

`cargo tauri build` needs `cargo install tauri-cli --version "^2"`, and must run from
the app crate — that's where `tauri.conf.json` lives. It writes:

```
target/release/bundle/macos/KindleFill.app      drag to /Applications
target/release/bundle/dmg/KindleFill_0.1.0_aarch64.dmg
```

A local build is **ad-hoc signed** and opens by double-click, because macOS only
quarantines files that arrive from elsewhere. Releases are a different build: the
workflow signs them with a Developer ID and sends them to Apple's notary service, which
is what lets a *downloaded* copy open without the Privacy & Security detour.

Notarization needs five secrets on the repository — `APPLE_CERTIFICATE` (a
base64-encoded Developer ID Application `.p12`), `APPLE_CERTIFICATE_PASSWORD`,
`APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD` (an app-specific password, not
the account password) and `APPLE_TEAM_ID`. Without them the workflow still produces a
working DMG; it just isn't notarized, and the release notes say so rather than claiming
otherwise. The workflow asks `spctl` whether Gatekeeper actually accepts the bundle and
picks its release notes from the answer, so a failed notarization can't be published as
a successful one.

The `-p kindlefill-app` above is the crate; the binary it produces is `KindleFill`.
Unbundled, macOS takes the Dock and app-menu label from the executable name, so
without that the app would introduce itself as "kindlefill-app" during development
while the shipped bundle said KindleFill.

The frontend is a single static HTML file with no framework and no build step, so
`cargo run` is enough — there's no dev server to start first.

Two environment variables help when something goes wrong, both debug-only. A webview
swallows JS errors — a frontend fault reaches neither stdout nor the window, it just
leaves the UI inert — so `KINDLEFILL_DEVTOOLS=1` opens the inspector. And every figure
the UI shows arrives as an engine event, so `KINDLEFILL_TRACE=1` mirrors that stream to
stderr with timestamps, which is how the progress cadence and Stop latency above were
measured rather than eyeballed.

It shows the connected device, capacity, free space, and any filler already present;
lets you set the target window; estimates the transfer up front; and runs the fill with
a live bar, throughput, and ETA. The header keeps up during the fill — free space, the
folder, and the filler tally all move as objects land, because `detect` holds the
device open and so cannot run while a fill is using it; the figures come from the
engine's own event stream instead.

Throughput is **bytes moved divided by time elapsed** over a trailing five seconds,
not an average of instantaneous samples. The distinction is not cosmetic. MTP arrives
in bursts as the host buffer flushes, so a 100 ms sample carrying 10 MB reads as
100 MB/s; averaging *rates* weights that burst like any other sample and lands high.
On a transfer whose true throughput was 25.5 MB/s, the old exponential average read
22-86 MB/s and centred near 35 — the displayed speed was wrong, and the ETA it fed was
optimistic. The window reported 23.6-27.3, centred on 25.6. The ETA uses a longer
thirty-second horizon and is rounded coarser the further out it is, so the text
changes about once every four seconds instead of ten times a second.

**Stop** is wired to a real cancel token — checked inside the upload rather than only
between objects, so it responds in about a second rather than up to 40. Stopping is
safe: the half-written object is deleted, everything committed stays valid, and
pressing Fill again resumes.

Three things it tells you before you start, because each one costs you seventeen
minutes if you find out afterwards:

- **The folder by name.** Filler goes in `fill_disk` at the storage root, and the app
  says so rather than reporting a file count — undoing this by hand means knowing what
  to look for. The name is editable, and changing it re-checks the new folder.
- **Whether that folder already exists holding something that isn't ours.** The items
  are listed by name, and Fill is held until you either pick a different name or tick
  **Overwrite**, which empties the folder completely — including those files. That tick
  is the only way anything in there gets deleted; it resets on every re-check, so a
  confirmation given for one folder can't be spent on another.
- **A downloaded firmware update.** Any `update_*.bin` at the root is listed by name,
  with a two-click delete if you want it gone. Stated rather than insisted on: it's
  your device, and it might be your file.

A folder holding *only* filler this tool wrote is not a conflict — that's a resume, and
there is nothing there to lose. It says how much is already there and continues from it.

## CLI

```bash
cargo run -p kindlefill-cli -- probe     # what do we see? nothing on the device changes
cargo run -p kindlefill-cli -- bench     # throughput + free-space sanity; tidies up after itself
cargo run -p kindlefill-cli -- status    # free space and existing filler
cargo run -p kindlefill-cli -- fill      # fill to 50-90 MB free
cargo run -p kindlefill-cli -- fill --low 40MB --high 80MB
cargo run -p kindlefill-cli -- clean     # remove all filler
cargo run -p kindlefill-cli -- purge --dir some_folder   # empty a folder, whatever is in it
```

`status`, `fill` and `clean` take `--dir` to work in a folder other than `fill_disk`.

Two things delete files this tool didn't write, and they are the only two: `fill
--overwrite`, which empties the target folder before filling it, and `purge`, which
empties a named folder and does nothing else. `purge` has no default `--dir` — the verb
that deletes other people's files shouldn't be runnable from muscle memory — and it
leaves the (now empty) folder in place. It exists because `clean` deliberately can't
help here: `clean` removes only names it can prove it wrote, so it is no use for taking
back a folder, or for clearing up after a `bench` run that failed before it could tidy
up. `bench` names `purge` in that failure message.

`status` also reports anything in that folder this tool didn't write, and any staged
firmware update at the root. Deleting an update is the app's job; the CLI only says
it's there.

`probe` touches nothing on the device — but it isn't inert on your Mac. It answers
"can this tool get `ptpcamerad` out of the way without sudo?" the only way that
question can be answered, which is by trying: it sends the daemon a `SIGKILL` and
reports whether that worked. launchd restarts it within a second, and `fill` then keeps
it away for the length of the transfer rather than permanently. Worth knowing before
running it, not because it's risky but because "changes nothing" would be a lie.

`fill` renders a live progress bar with throughput and ETA:

```
  [############--------------------] 37.4% | 9.31 GB / 24.88 GB | 25.5 MB/s | 10m 26s left
```

It falls back to plain lines when output isn't a terminal, so piping to a log stays
readable. Progress updates come from *inside* each upload, not just between them —
otherwise the bar would freeze for a minute at a time during a 1 GiB write.

`fill` is resumable. It decides from measured free space, not from a tally of what it
wrote, so re-running after an interrupt tops up instead of starting over or colliding
on filenames.

`clean` only deletes files matching its own `fill_NNNN.bin` naming, and only removes
`fill_disk` if nothing else is in it — a book dropped in that folder survives.

"Matching its own naming" is exact, not approximate: a name counts as filler only if
formatting the number back reproduces it byte for byte, so `fill_notes.bin`,
`fill_12.bin` and `fill_00000007.bin` are all left alone. One function answers that
question and both the delete set and the resume sequence read it, because when the two
were decided separately they disagreed — and the half that was too permissive was the
half that deletes.

## Re-validating against hardware

`probe` and `bench` answered the two questions that gated the design, and stay useful
for checking a different Kindle model:

`bench` is also the fastest way to tell a wedged device from a broken build. An
interrupted transfer — an unplug mid-write, a process killed while it held the
device — can leave a Kindle answering reads while refusing every write: `status`
reports free space happily, and creating a folder times out. Unplug and replug (or
restart the Kindle) and run `bench`; if it completes, writes are healthy again.

1. **Does `pkill ptpcamerad` work without sudo?** `probe` answers this by trying. If it
   needs elevation, the GUI needs an admin prompt or a privileged helper — a
   significant complexity jump — instead of just working.
2. **Does reported free space actually move after a write?** `bench` prints free space
   before and after each upload. MTP caches `StorageInfo`, and this code calls
   `Storage::refresh()` every time to force a fresh `GetStorageInfo` — but if a Kindle
   still reports stale numbers, the convergence loop can't work as designed and needs
   rethinking. `bench` says so explicitly rather than hanging.

`bench` also measures throughput, which turns "how long will this take" into a real
number instead of a guess.

## Layout

```
crates/kindlefill-core/    plan.rs        pure convergence logic, no I/O
                          rate.rs        throughput smoothing, ETA, progress figures
                          zeros.rs       synthetic byte source for uploads
                          engine.rs      drives a real mtp_rs::Storage
                          ptpcamerad.rs  keeps Apple's camera daemon off the device
crates/kindlefill-cli/     probe / bench / status / fill / clean / purge
crates/kindlefill-app/     Tauri desktop UI (static HTML frontend, no build step)
```

`ptpcamerad.rs` lives in core rather than in the CLI because both front ends need it.
It started out private to the CLI, which meant the app could not tame the daemon at
all — the README said the tool did, and for the GUI that was untrue.

`plan.rs` is I/O-free so the hard part is testable without a Kindle. `engine.rs` is
covered end-to-end in `tests/virtual_device.rs` against `mtp-rs`'s virtual device,
which is backed by a real directory and reports free space from actual disk usage — so
the convergence loop is exercised for real, per-file overhead included.

That suite can't prove a Kindle refreshes free space promptly — a device that cached it
would pass every test here and hang on the cable. That's what `bench` is for, and on a
Paperwhite Signature Edition it came back exact.

```bash
cargo test      # no hardware required
```

CI runs `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings` and
`cargo test --workspace` on every push and pull request. Since none of it needs a
Kindle, it gates the part that matters rather than a subset that happens to run without
hardware.

**The frontend is not covered by CI.** The app's ~430 lines of JavaScript have a small
harness at `crates/kindlefill-app/ui-tests/controls.html`, which loads the real
`index.html` with the Tauri bridge stubbed and asserts what is clickable after scripted
interactions — including the case where re-ticking Overwrite mid-fill used to make Fill
clickable again. It runs in a browser, by hand:

```bash
cd crates/kindlefill-app && python3 -m http.server 8000
# then open http://localhost:8000/ui-tests/controls.html
```

Wiring a headless browser into CI is a bigger dependency than eleven assertions currently
justify. Until that changes, a green CI badge says nothing about the frontend, so it
shouldn't be read as if it does.

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option.

This is the Rust ecosystem's default and matches `mtp-rs`, the crate this is built on.
MIT is short and universally recognized; Apache-2.0 adds an explicit patent grant that
MIT lacks, which some downstream users need and which protects contributors too.
Offering both means nobody has to argue with their legal team about which one they got.

Unless you state otherwise, any contribution you intentionally submit for inclusion in
this work shall be dual-licensed as above, with no additional terms or conditions.

## Scope

Fill and unfill. It does not jailbreak anything and doesn't check whether your firmware
is jailbreakable. Turn on Airplane Mode.

By default the only things deleted are files it can prove it wrote, plus the filler
folder itself when nothing else was in it. Two paths go further, and both need you to
ask for them explicitly, by name, in the app:

- **Overwrite** empties the filler folder, whatever is in it.
- **Delete staged update** removes named `update_*.bin` files from the root.

Neither is reachable by accident. The update deletion in particular passes three
independent gates — root-level, matches the shape, and present in the list you
confirmed — so a name arriving from the UI that isn't a root-level update image deletes
nothing. `tests/virtual_device.rs` exercises both against a real directory, including
the cases that must survive.

## What has and hasn't been exercised on hardware

Fill, Stop, resume, and Remove filler & folder have all been run against a Paperwhite Signature
Edition (see Status). Three paths have not, and are covered only by the test suite:

- **`ptpcamerad` taming** — the daemon wasn't running on the test machine.
- **Deleting a staged firmware update** — no update was staged on the test device.
- **Overwrite** — the test folder never held foreign content.

Two more caveats, both about *when* those runs happened. They were made against an
earlier build, before the interface was reworked — the capacity bar, the terminal
progress state, the armed-overwrite button and the range guards have been exercised
against a stubbed backend in a browser and by the test suites, but not against a Kindle.
And the released bundle itself has not been launched: its contents are verified by
decompressing the embedded frontend and by 68 Rust tests, which is not the same as
double-clicking it.

If you run it against real hardware, an issue saying what happened — good or bad — is
the single most useful thing you could contribute.
