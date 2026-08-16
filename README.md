# KindleFill

Fills a Kindle's storage down to a target free-space window (default 50–90 MB) so the
device can't download an OTA firmware update, and removes the filler again afterwards.

Targets both modern MTP Kindles (11th gen and newer — 2024 Paperwhite, Colorsoft,
Scribe, basic 2024) and older models that Finder mounts as a USB volume named
`Kindle`.

> **Not created or endorsed by Amazon.** KindleFill is an independent, free, open-source
> tool. *Kindle* and *Amazon* are trademarks of Amazon.com, Inc. or its affiliates, used
> here only to say which device this works with.

![The KindleFill window during a fill: a bar showing how much of the Kindle is filler
this tool wrote, the target range, and a live progress reading with throughput and time
remaining.](docs/screenshot.png)

## Install

From [Releases](https://github.com/SyrinxVentures/kindlefill/releases/latest):

- **macOS** — **KindleFill_*_aarch64.dmg**. Open it and drag KindleFill to Applications.
  Apple silicon only. Signed with a Developer ID and notarized by Apple, so it opens on a
  double-click with no warning to dismiss.
- **Windows** — **KindleFill_*_x64-setup.exe**. The installer is unsigned, so SmartScreen
  warns on first run: **More info** → **Run anyway**. On a machine without the WebView2
  runtime the installer fetches it, so that first install needs a network connection.

Both are built from the same commit and carry the same version — see
[Platforms](#platforms) for what has actually been tested on each.

Also turn on **Airplane Mode**. Filling the storage stops the download; Airplane Mode
stops the check.

## Using it

Plug in the Kindle and unlock it. The app shows capacity, free space, and how much of
the device is filler it wrote, then estimates the transfer before you commit to it — a
full fill on a 32 GB Kindle is roughly **17 minutes**.

**Stop responds in about a second**, not at the end of the current object. Stopping is
safe: the half-written object is deleted, everything already committed stays valid, and
pressing Fill again resumes from where it left off rather than starting over.

Three things it tells you before you start, because each one costs you seventeen minutes
if you find out afterwards:

- **The folder, by name.** Filler goes in `fill_disk` at the storage root. Undoing this
  by hand means knowing what to look for, so it says the name rather than a file count.
  Editable, and changing it re-checks the new folder.
- **Whether that folder already holds something that isn't ours.** Those items are
  listed by name and Fill is held until you either pick a different folder or tick
  **Overwrite**. A folder holding *only* filler this tool wrote is not a conflict —
  that's a resume, and it continues from what's there.
- **A downloaded firmware update.** Any `update_*.bin` at the storage root is listed by
  name, with a two-click delete if you want it gone. Stated rather than insisted on:
  it's your device, and it might be your file.

## What it deletes

By default, only files it can prove it wrote — names matching its own `fill_NNNN.bin`
pattern exactly — plus the filler folder itself when nothing else was left in it. A book
you dropped in that folder survives, and so does the folder holding it.

There is one file it deletes without being able to read its name, and it earns that by
leaving evidence first. A fill writes `kindlefill_inflight.txt` into the filler folder
and removes it on every ordinary exit, Stop included. Finding one on a later run means a
previous fill was killed mid-write — and on Windows the object it was writing carries a
name the driver chose, not ours. Only with that marker present is an unrecognised file
in that folder treated as debris and reclaimed, and it is named in the log when it goes.
Without the marker the same file is left alone, exactly as before. The marker is removed
only once the debris is confirmed gone, so a device that refuses the deletion keeps the
one piece of evidence that makes those bytes reclaimable later.

Two paths go further, and both need you to ask for them by name:

- **Overwrite** empties the filler folder, whatever is in it.
- **Delete staged update** removes named `update_*.bin` files from the storage root.

Neither is reachable by accident. Overwrite is bound to a digest of the exact file
listing you were shown, so a confirmation given for one folder cannot be spent on
another's contents, or on a folder that changed after you looked at it. Update deletion
passes three independent gates — root-level, matches the shape, and present in the list
you confirmed — so a name arriving from the UI that isn't a root-level update image
deletes nothing.

## Platforms

**macOS** (Apple silicon) and **Windows** each ship an app and a CLI. The macOS build is
signed with a Developer ID and notarized. The Windows installer is not signed.

Two Kindles have been on the cable, and they are the only hardware this has ever run on:

| | Kindle Oasis (10th gen) | Kindle Paperwhite Signature Edition |
|---|---|---|
| Reached over | USB mass storage | native MTP |
| Throughput | 3.4 MB/s | 27–30 MB/s |
| `bench` free-space tracking | exact | exact |

Windows was validated against both at 1.0.0. macOS was validated against the Paperwhite
at 1.0.1, and against the Oasis at 0.2.0. Between them: `probe`, `bench`, `status`,
`fill`, resume, Stop, `clean`, `purge`, foreign-file protection, staged-update detection
and deletion, the overwrite confirmation, and recovery from a killed run.

`bench` is the one that mattered on each — free space tracked every write exactly and
promptly, which is the assumption the whole convergence design rests on and the reason
this was blocked on hardware rather than on review.

Throughput is a property of the device rather than the platform. That Oasis runs at
about 3.4 MB/s over its micro-USB cable, so a full fill on it takes closer to half an
hour than to the seventeen minutes quoted above.

Some things still have no hardware behind them: the mounted-volume transport at 1.0.1 on
either platform, `ptpcamerad` taming, and Overwrite.
[docs/INTERNALS.md](docs/INTERNALS.md) carries the full validation record, including what
putting a device on the cable cost the tool and what each run did and didn't prove.

On Windows, `open_first` opens whichever portable device the OS enumerates first, and
every USB drive is one — so the app checks what the device says it is and holds Fill
behind an explicit opt-in when that isn't a Kindle. Filling a non-Kindle is therefore
possible and untested, in that order.

**Linux** compiles untested: volume detection looks under `/run/media/<user>/Kindle`
and `/media/<user>/Kindle` for mass-storage Kindles, and MTP goes through `mtp-rs`'s
libusb path, but no Linux machine has ever run this. A report either way is a
contribution.

Other models should work and none have been tried. If you run it against different
hardware, [an issue saying what happened](https://github.com/SyrinxVentures/kindlefill/issues)
— good or bad — is the most useful thing you could contribute.

**Sizes are binary.** 1 GB here means 1 GiB (1024³ bytes) throughout, so a capacity
shown here reads lower than the number on the box, which is quoted in SI GB.

## CLI

```bash
cargo run -p kindlefill-cli -- status    # free space and existing filler
cargo run -p kindlefill-cli -- fill      # fill to 50-90 MB free
cargo run -p kindlefill-cli -- fill --low 40MB --high 80MB
cargo run -p kindlefill-cli -- clean     # remove all filler
cargo run -p kindlefill-cli -- purge --dir some_folder   # empty a folder entirely
cargo run -p kindlefill-cli -- probe     # what do we see?
cargo run -p kindlefill-cli -- bench     # throughput + free-space sanity
```

`status`, `fill` and `clean` take `--dir` to work in a folder other than `fill_disk`.
`fill` renders a live progress bar with throughput and ETA, and falls back to plain
lines when output isn't a terminal.

`probe` touches nothing on the device, but it isn't inert on your Mac: it answers "can
this get `ptpcamerad` out of the way without sudo?" the only way that can be answered,
by sending the daemon a `SIGKILL` and reporting whether it worked. launchd restarts it
within a second.

## Build from source

```bash
cargo run -p kindlefill-app                     # run it
cd crates/kindlefill-app && cargo tauri build    # build a .app and .dmg
```

`cargo tauri build` needs `cargo install tauri-cli --version "^2"` and must run from the
app crate. The frontend is a single static HTML file with no framework and no build
step, so there's no dev server to start first.

```bash
cargo test --workspace    # no hardware required
```

## More

- [How it works, and how it was validated](docs/INTERNALS.md) — the convergence loop, the
  crate layout, and the record of what has actually been run against hardware
- [Contributing](CONTRIBUTING.md) — the most useful contribution is a report from a
  Kindle or an OS this has never run on
- [Security](SECURITY.md) — what counts as a vulnerability in a tool with no network
- [Releasing](docs/RELEASING.md) — signing and notarization

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option. This is the Rust ecosystem's default and matches `mtp-rs`, the crate
this is built on.

Unless you state otherwise, any contribution you intentionally submit for inclusion in
this work shall be dual-licensed as above, with no additional terms or conditions.
