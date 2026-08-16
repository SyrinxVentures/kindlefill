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

**Windows** is where **1.0.0** was validated, against both Kindles and both transports:

| | Kindle Oasis (10th gen) | Kindle Paperwhite Signature Edition |
|---|---|---|
| Reached over | WPD wrapping USB mass storage | native MTP |
| Enumerates as | `SWD\WPDBUSENUM\_??_USBSTOR#…` | `USB\VID_1949&PID_9981` |
| Also mounts as a drive | yes, `D:` | no |
| Throughput | 3.4 MB/s | 27–30 MB/s |
| `bench` free-space tracking | exact, zero overhead | exact, 4 KB per-file overhead |
| Filled to the 50–90 MB window | yes | to intermediate windows only |

Between them: `probe`, `bench`, `status`, `fill`, resume, Stop, `clean`, `purge`,
foreign-file protection, staged-update detection and deletion, the overwrite
confirmation, and recovery from a killed run. `bench` is the one that mattered on each —
free space tracked every write exactly and promptly, which is the assumption the whole
convergence design rests on and the reason this was blocked on hardware rather than on
review.

Two honest gaps, both narrowed since. The Paperwhite was filled into several
intermediate windows but never taken down to 50–90 MB on Windows, so on that platform
the smallest rungs of the size ladder are still untried — the macOS run below has since
exercised them on the same device. And **the mounted-volume transport was not exercised
on Windows at all** — the Oasis offered a `D:` volume, but a portable-device entry
outranks it and the tool took that instead, while the Paperwhite offers no volume to
take. That path was proven on macOS at 0.2.0, and `mass_storage.rs` has changed since;
with no Oasis on the cable for the 1.0.1 runs, the mounted-volume transport is
unvalidated at this version on **both** platforms.

**macOS** (Apple silicon) is the platform this is built and released on, and where both
of these devices were originally validated — the Paperwhite over MTP, the Oasis as a
Finder volume.

That validation was against **0.2.0**. **1.0.1 has since been run against the
Paperwhite Signature Edition on a Mac**, over native MTP, which is what this section
previously said had to happen before the macOS builds could stop carrying the
unverified-on-hardware status Windows used to.

Every surface the Windows fixes changed was exercised there. The `None`-addressed
storage root carried `probe`, `status`, `fill`, `clean`, `purge` and `bench` without
complaint. The identity gate recognised the Paperwhite and left Fill unheld. The
pre-flight estimate said nothing about rate on a fresh launch — "the time depends on
your Kindle and cable" — and only after the device had reported one did it quote
"roughly 8 min at ~28.6 MB/s, measured on this Kindle". A failed detect followed by a
replug announced one reconnect, not one every two seconds. The activity log held its
height as lines arrived. Stop answered immediately mid-object. And a resumed fill said
"Resuming — 11.00 GB of filler is already on the device (11 files), and is being kept.
The bar below measures only what is left to write."

`bench` was again the result that mattered, and it held: free space moved by precisely
the bytes written at 16, 128 and 512 MiB — 0, 0 and 4096 bytes of overhead — at
26.6–30.4 MB/s. Deletion accounting was exact in both directions. `clean` reported
24.78 GB reclaimed against 24.78 GB of free space actually returned, `purge` 512 MB
against 512 MB, and `clean` over an empty folder said `0 B` rather than inventing a
figure. The fill was taken to the real 50–90 MB default for the first time on this
device on either platform, landing at **81.18 MB** on the first pass with the ladder
stepping 1 GB → 256 MB → 16 MB, no overshoot below the window and no oscillation
around it.

Three things did not come out clean, and they are worth more than the passes.

**The test device was jailbroken** — Vera, with KOReader installed. Nothing in the
results looked jailbreak-shaped; the MTP responder is Amazon's own and the device
behaved as the stock Paperwhite did on Windows. But a stock Paperwhite on macOS is
inferred from that, not tested, and the difference is exactly the kind this section
exists to keep visible.

**The marker file was never directly observed.** `kindlefill_inflight.txt` behaved
correctly in every way that can be seen from outside — it was gone after a normal fill
and after Stop, and across two `kill -9` runs the stranded bytes were never counted as
foreign or as filler by either front end. But the CLI has no verb that lists a folder,
so the file itself was never laid eyes on. Its consequences are verified; its existence
is inferred.

**And the CLI and the app explain the same device state differently, with only the app
correct.** A `kill -9` mid-fill leaves this Paperwhite answering reads while refusing
every write. The app says so precisely — that the session can wedge after an interrupted
transfer, that the device will look fine while nothing can be written, and to replug and
then restart the device — and replugging did clear it. The CLI, on the same condition,
prints the `ptpcamerad` advice and tells you to quit Android File Transfer and OpenMTP.
On the run that produced this, `ptpcamerad` was not running, `probe` said so two lines
above the error, neither of those apps was installed, and the process actually holding
the device was **Calibre**, which auto-launches on connect and is named nowhere. That
sent the validation down a blind alley for five attempts. It is a wrong explanation
rather than a broken operation, so it did not gate this release; it is queued as a fix.

Putting a device on the cable cost the tool eight bugs a green CI had no way to catch,
all in code the virtual-device tests never reach. Root listings addressed the storage
root by a sentinel handle that PTP tolerates and WPD rejects, which failed every device
operation on Windows. The app and CLI binaries shared one case-insensitive path in
`target/` and silently overwrote each other. The pre-flight estimate quoted a throughput
measured on other hardware, advertising four minutes for a transfer that took twenty-six.
A failed detect left the presence poll re-announcing a reconnect every two seconds
forever. The activity log resized itself as lines arrived, moving the panel above it
under the reader's eyes. And a resumed fill was indistinguishable on screen from one
that had thrown the previous run's work away.

The last two are worth their own paragraph, because they concern what the tool *claims*
rather than what it does.

A write on the WPD path does not carry the name we ask for until it commits — the object
is created under a driver-assigned temporary name (`NEWF4A3.tmp` and friends) and renamed
at the end. Kill the process before then and the leftover is a file this tool named
nothing and could recognise by nothing, so `clean` reported "nothing to remove" over
94 MB of its own debris. Fixed by leaving evidence rather than guessing at names: a fill
writes `kindlefill_inflight.txt` into the filler folder and removes it on any ordinary
exit, Stop included. A marker still there on a later run means a previous one died
mid-write, and only then is an unrecognised file in that folder treated as ours.

And a deletion that reports success was not evidence the object was gone.
`IPortableDeviceContent::Delete` returns `S_OK` while refusing individual objects,
reporting per-object outcomes in a results collection that `mtp-rs` 0.30's WPD backend
never reads — so every refusal counted as a success, and `clean` and `purge` added those
bytes to "reclaimed". A Kindle refuses exactly these orphaned temp objects until the USB
session is reset, so `purge` announced 286 MB freed while free space moved by none of it.
Deletions are now confirmed by re-listing the folder before anything is announced, and a
refusal says so and tells you to replug. That one is not Windows-specific in principle:
the same silence would have applied on macOS the moment a device refused a delete.

Neither of those two is reachable on native MTP in the form Windows hit them, though
the macOS runs sharpened what that means. A `kill -9` mid-fill does strand the bytes on
a Paperwhite: free space dropped by the ~400 MB in flight and stayed down across a
later, successful MTP session, coming back only when the device was restarted. What it
does not produce is an *object* — the partial never appears in the folder listing, so
there is no debris carrying a name we could fail to recognise, which is the inverse of
the WPD case where it becomes a visible file under a driver-assigned one. So the marker
has nothing to grip here and nothing to get wrong: across those runs the stranded bytes
were counted as neither foreign nor filler, `clean` under-reported rather than
over-reported them, and every byte it did claim came back. The marker and the delete
confirmation cost this device nothing and save the other one from losing space it can
never account for — which is the case for keeping both rather than gating them behind a
transport check.

Throughput is a property of the device, not of Windows: that Oasis runs at about
3.4 MB/s over its micro-USB cable, and writing to its mounted volume directly measures
the same, so a full fill on it takes closer to half an hour than to the seventeen
minutes quoted above.

Because `open_first` opens whichever portable device Windows enumerates first, and
every USB drive is one, the app checks what the device says it is and holds Fill behind
an explicit opt-in when that isn't a Kindle. Filling a non-Kindle is therefore possible
and untested, in that order.

The Windows installer is unsigned, so SmartScreen will warn on first run, and installing
on a machine without the WebView2 runtime needs a network connection (the installer
downloads it).

**Linux** compiles untested: volume detection looks under `/run/media/<user>/Kindle`
and `/media/<user>/Kindle` for mass-storage Kindles, and MTP goes through `mtp-rs`'s
libusb path, but no Linux machine has ever run this. A report either way is a
contribution.

The two devices above are the only hardware this has ever run on: a **Kindle Paperwhite
Signature Edition** and a **Kindle Oasis (10th generation)**, each tried on both
platforms and on whichever transport it offers there — MTP and a Finder volume on macOS,
native MTP and the WPD mass-storage shim on Windows. Other models should work and none
have been tried. If you run it against different hardware,
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
