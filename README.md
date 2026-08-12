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
[Releases](https://github.com/SyrinxVentures/kindlefill/releases/latest), open it, and
drag KindleFill to Applications. Apple silicon only — see [Platforms](#platforms).

Released builds are signed with a Developer ID and notarized by Apple, so they open on a
double-click with no warning to dismiss.

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

macOS only, Apple silicon only. That isn't a policy — it's the only configuration this
has ever been compiled or run on. Windows and Linux are untried rather than unsupported:
the code carries real affordances for both, and nobody has tested them. The unknown is
whether `mtp-rs`'s WPD and libusb backends behave like the macOS one.

Validated against a **Kindle Paperwhite Signature Edition**. Other models should work
and none have been tried. If you run it against different hardware,
[an issue saying what happened](https://github.com/SyrinxVentures/kindlefill/issues) —
good or bad — is the most useful thing you could contribute.

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
