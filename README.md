# KindleFill

Fills a Kindle's storage down to a target free-space window (default 50–90 MB) so the
device can't download an OTA firmware update, and removes the filler again afterwards.

Targets modern MTP Kindles (11th gen and newer — 2024 Paperwhite, Colorsoft, Scribe,
basic 2024), which no longer mount as USB mass storage. macOS is the target platform;
the stack is portable but untested elsewhere.

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

**The GUI has not been run.** Its backend is type-checked and the workspace builds, but
it was developed on Linux where a Tauri window can't be opened — treat first launch as
unverified.

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
cargo run -p kindlefill-app          # run it
cargo tauri build                    # build a .app / .dmg (needs `cargo install tauri-cli`)
```

The frontend is a single static HTML file with no framework and no build step, so
`cargo run` is enough — there's no dev server to start first.

It shows the connected device, capacity, free space, and any filler already present;
lets you set the target window; estimates the transfer up front; and runs the fill with
a live bar, throughput, and ETA. **Stop** is wired to a real cancel token — checked
inside the upload rather than only between objects, so it responds in about a second
rather than up to 40. Stopping is safe: the half-written object is deleted, everything
committed stays valid, and pressing Fill again resumes.

## CLI

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

## Re-validating against hardware

`probe` and `bench` answered the two questions that gated the design, and stay useful
for checking a different Kindle model:

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
crates/kindlefill-app/     Tauri desktop UI (static HTML frontend, no build step)
```

`plan.rs` is I/O-free so the hard part is testable without a Kindle. `engine.rs` is
covered end-to-end in `tests/virtual_device.rs` against `mtp-rs`'s virtual device,
which is backed by a real directory and reports free space from actual disk usage — so
the convergence loop is exercised for real, per-file overhead included.

That suite can't prove a Kindle refreshes free space promptly — a device that cached it
would pass every test here and hang on the cable. That's what `bench` is for, and on a
Paperwhite Signature Edition it came back exact.

```bash
cargo test      # 36 tests, no hardware required
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
