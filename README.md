# KindleFiller

Fills a Kindle's storage down to a target free-space window (default 50–90 MB) so the
device can't download an OTA firmware update, and removes the filler again afterwards.

Targets modern MTP Kindles (11th gen and newer — 2024 Paperwhite, Colorsoft, Scribe,
basic 2024), which no longer mount as USB mass storage. macOS is the target platform;
the stack is portable but untested elsewhere.

## Status

`kindlefill-core` is written and tested. **Nothing has been run against a real Kindle
yet.** The `probe` and `bench` commands exist to close that gap — see
[Validating against hardware](#validating-against-hardware).

The Tauri GUI is not built yet, deliberately: two of its design decisions depend on
what `bench` reports.

## Why this isn't just "copy some big files over"

Three things make it more than a drag-and-drop:

**macOS doesn't refuse MTP — a daemon squats on it.** `ptpcamerad`, Apple's PTP camera
daemon, exclusively claims any MTP device the moment it's plugged in. It only speaks
PTP, so it can't talk to a Kindle; it just stops anything else from doing so. This tool
kills it repeatedly for the duration of a transfer and lets launchd restart it after,
rather than disabling it permanently and quietly breaking camera import.

**Landing in the window is a control problem, not arithmetic.** 50–90 MB on a 16 GB
device is a 0.25% target, and every file costs more than its own bytes in ways the host
can't predict. So the loop measures, writes, and re-measures — never tallies. See
`plan.rs`.

**Compression can't help.** The prebuilt filler ZIPs floating around compress well
because they're archives of zeros, but they're extracted *on the host* and the full
size still crosses the USB wire. Nothing can shortcut that: MTP has no compressed
transfer mode, and making the Kindle expand an archive needs code execution on the
device — exactly what you don't have before jailbreaking.

We do avoid the ZIP workflow's other costs. It writes ~13 GB to your SSD and reads it
back, and needs that much free space locally. This generates zeros in memory and
streams them straight into the upload.

## Usage

```bash
cargo run -p kindlefill-cli -- probe     # what do we see? changes nothing
cargo run -p kindlefill-cli -- bench     # throughput + free-space sanity; self-cleaning
cargo run -p kindlefill-cli -- status    # free space and existing filler
cargo run -p kindlefill-cli -- fill      # fill to 50-90 MB free
cargo run -p kindlefill-cli -- fill --low 40MB --high 80MB
cargo run -p kindlefill-cli -- clean     # remove all filler
```

`fill` renders a live progress bar with throughput and ETA:

```
  [############--------------------] 37.4% | 4.88 GB / 13.04 GB | 18.2 MB/s | 7m 42s left
```

It falls back to plain lines when output isn't a terminal, so piping to a log stays
readable. Progress updates come from *inside* each upload, not just between them —
otherwise the bar would freeze for a minute at a time during a 1 GiB write.

`fill` is resumable. It decides from measured free space, not from a tally of what it
wrote, so re-running after an interrupt tops up instead of starting over or colliding
on filenames.

`clean` only deletes files matching its own `fill_NNNN.bin` naming, and only removes
`fill_disk` if nothing else is in it — a book dropped in that folder survives.

## Validating against hardware

Run `probe` and `bench` with the Kindle attached. Two results decide open questions:

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
crates/kindlefill-core/    plan.rs    pure convergence logic, no I/O
                          rate.rs    throughput smoothing, ETA, progress figures
                          zeros.rs   synthetic byte source for uploads
                          engine.rs  drives a real mtp_rs::Storage
crates/kindlefill-cli/     probe / bench / status / fill / clean
```

`plan.rs` is I/O-free so the hard part is testable without a Kindle. `engine.rs` is
covered end-to-end in `tests/virtual_device.rs` against `mtp-rs`'s virtual device,
which is backed by a real directory and reports free space from actual disk usage — so
the convergence loop is exercised for real, per-file overhead included.

That suite can't prove a Kindle refreshes free space promptly. A device that cached it
would pass every test here and hang on the cable. Hence `bench`.

```bash
cargo test      # 34 tests, no hardware required
```

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

Fill and unfill. It does not jailbreak anything, doesn't check whether your firmware is
jailbreakable, and doesn't delete staged `update*.bin` files — filling around an
already-downloaded update accomplishes nothing, so check for one yourself before
starting, and turn on Airplane Mode.
