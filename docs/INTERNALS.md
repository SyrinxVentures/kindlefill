# How KindleFill works, and how it was validated

Background for contributors and the curious. The [README](../README.md) covers
installing and using it; this is the reasoning underneath, the crate layout, and
the record of what has actually been run against hardware.

## Why this isn't just "copy some big files over"

Three things make it more than a drag-and-drop:

**macOS doesn't refuse MTP — a daemon squats on it.** `ptpcamerad`, Apple's PTP camera
daemon, exclusively claims any MTP device the moment it's plugged in. It only speaks
PTP, so it can't talk to a Kindle; it just stops anything else from doing so. This tool
kills it repeatedly for the duration of a transfer and lets launchd restart it after,
rather than disabling it permanently and quietly breaking camera import.

Older Kindles use USB mass storage instead. Finder mounts those at `/Volumes/Kindle`;
the app recognizes that volume only when it also contains the expected `documents`
directory, then uses normal filesystem operations with the same filler-name and
overwrite-confirmation rules as the MTP path. This path has been run against an Oasis
(10th generation) — see [Status](#status).

`ptpcamerad` didn't turn out to be running on the machine this was validated on, so that
handling is defensive rather than load-bearing — but it costs nothing when idle, and the failure
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

## Status

Validated on macOS against one device per transport: a **Kindle Paperwhite Signature
Edition** (25.46 GB usable) over MTP, and a **Kindle Oasis (10th generation)** as a
mass-storage volume.

The MTP measurements below come from the Paperwhite. Every assumption the design rested
on held:

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

The GUI has since been run against the same Paperwhite, driven end to end:

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

**The mass-storage path has since been exercised too**, with the Mac app against a
**Kindle Oasis (10th generation)** — the older USB-volume transport rather than MTP.
Detection, fill, Stop, resume, and Remove filler & folder all behaved as they do over
MTP. There are no `bench` numbers for it: `bench` opens an MTP device, so it doesn't run
on a mass-storage Kindle at all. The question it exists to answer doesn't arise there
either — free space comes from `statvfs` on a mounted filesystem, not from a device
answering a cacheable `GetStorageInfo`.

**1.0.1 was re-validated on macOS against the same Paperwhite**, over native MTP, after
the eight fixes that Windows hardware forced into shared engine code. That run is the
one that put those fixes in front of a Mac for the first time:

| Check | Result |
|---|---|
| `None`-addressed storage root | Carried `probe`, `status`, `fill`, `clean`, `purge`, `bench` |
| Free space tracks writes (`bench`) | **Exactly** — 0, 0 and 4096 bytes of overhead at 16/128/512 MiB |
| Throughput | 26.6–30.4 MB/s |
| `clean` reclaim vs measured free-space delta | 24.78 GB reported, 24.78 GB returned |
| `purge` reclaim vs measured free-space delta | 512 MB reported, 512 MB returned |
| `clean` over an empty folder | `reclaimed 0 B` — no invented figure |
| Fill to the real 50–90 MB default | Landed at **81.18 MB**, ladder 1 GB → 256 MB → 16 MB, no overshoot or oscillation |
| Identity gate | Paperwhite recognised; Fill not held behind the non-Kindle opt-in |
| Pre-flight estimate before any measured rate | Quoted none; after one, "~28.6 MB/s, measured on this Kindle" |
| Presence poll after a failed detect | One reconnect announced, not one every two seconds |
| Activity log as lines arrived | Fixed height; panel above did not move |
| Stop, mid-object | Reported as "basically immediate" by the operator — impression, not instrumented; "What's written is intact — Fill again to resume" |
| Resumed fill | Named what it continued from and scoped the bar to what was left |

Three caveats attach to that run and should not be lost. **The device was jailbroken**
(Vera, KOReader installed), so a stock Paperwhite on macOS is inferred rather than
tested. **`kindlefill_inflight.txt` was never directly observed** — its consequences held
across normal exit, Stop and two `kill -9` runs, but the CLI has no verb that lists a
folder, so the file's existence is inferred from behaviour. And **the CLI's `explain()`
misdiagnoses a wedged device on macOS**: after a `kill -9` the Paperwhite answers reads
while refusing writes, and where the app names that condition and its remedy correctly,
the CLI prints the `ptpcamerad` advice and names Android File Transfer and OpenMTP —
while the actual holder was Calibre, which auto-launches on connect and appears in no
branch.

A `kill -9` also strands the in-flight bytes on this device: free space dropped by the
~400 MB in flight and stayed down across a later successful MTP session, returning only
after a device restart. No listable object is produced, so there is no debris to name —
the inverse of the WPD case.

Three paths have *not* been exercised on hardware and rest on the test suite alone —
see [What has and hasn't been exercised on hardware](#what-has-and-hasnt-been-exercised-on-hardware).

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
                          mass_storage.rs  the same fill over an OS-mounted volume
                          wpd.rs         "is a portable device present?" on Windows
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

It also can't see anything the *backend* does differently, and on Windows that turned
out to be most of what mattered. The virtual device speaks PTP-over-USB; a Kindle on
Windows is reached through WPD, and the first time one was put on the cable it produced
five bugs a green CI had no way to catch. Three were disagreements between the two
backends about the same call — addressing the storage root by sentinel handle works on
PTP and fails on WPD; a written object carries the name you asked for on PTP and a
driver-chosen temporary name on WPD until it commits; and a delete that the device
refuses reports as a success on WPD because the per-object result codes go unread.

The pattern is worth naming, because it will recur: a test suite that exercises one
transport proves the *engine*, not the tool. Anything expressed as "the library will do
X" is unproven until a device has been asked. Where an assumption of that shape survives
in this codebase, it now carries a comment saying which device, on which transport, was
asked.

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

## What has and hasn't been exercised on hardware

Fill, Stop, resume, and Remove filler & folder have all been run against a Paperwhite Signature
Edition over MTP and an Oasis (10th generation) as a mass-storage volume (see Status), so
both transports have carried a real fill and a real cleanup. Three paths have not, and are
covered only by the test suite:

- **`ptpcamerad` taming** — the daemon wasn't running on the test machine. It is an MTP
  concern by construction: the mass-storage path opens no MTP session, so the Oasis run
  couldn't exercise it either.
- **Deleting a staged firmware update** — no update was staged on either device.
- **Overwrite** — neither test folder ever held foreign content.

That caveat used to extend to *when* those runs happened — the original Paperwhite runs
predated the interface rework, so the reworked UI had never been driven over MTP against
a Kindle. The 1.0.1 macOS run closes that: the capacity bar, the terminal progress
state, the resume messaging and the failure states were all driven over MTP against the
Paperwhite. **The armed-overwrite button and the range guards still have not been** —
the test folder never held foreign content, and the windows used were the 50–90 MB
default and a wide intermediate one, neither of which trips a range guard.

The Oasis run closes the other half of that gap: it used the released 0.2.0 `.dmg`, so
the shipped bundle has been double-clicked and the reworked interface has driven a real
fill — on the mass-storage path.

If you run it against real hardware, an issue saying what happened — good or bad — is
the single most useful thing you could contribute.
