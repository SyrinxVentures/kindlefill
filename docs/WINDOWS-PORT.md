# What a Windows version would take

An analysis, not a plan of record. Written against `5ce1aed`. (The mass-storage work landed
and was twice amended while this was being written, so re-check the three compile errors
below before acting on them — line numbers there have already moved once.)

The short version: the MTP engine is already portable and `mtp-rs` ships a complete,
native Windows backend, so the code delta is small and mostly mechanical. What the port
actually costs is **validation** — a Windows machine with a Kindle on the cable — plus
**Authenticode signing**, which has no cheap equivalent to the Apple notarization the
release workflow already does. Three compile errors block the build today, and all three
arrived with the USB mass-storage commit.

## It does not compile for Windows right now

Verified, not assumed — `cargo check --target x86_64-pc-windows-msvc --all-targets
-p kindlefill-core`:

```
error[E0425]: cannot find type `statvfs` in crate `libc`      mass_storage.rs:66
error[E0425]: cannot find function `statvfs` in crate `libc`  mass_storage.rs:68
error[E0425]: cannot find value `W_OK` in crate `libc`        mass_storage.rs:85
```

Three errors, and they fail both `lib` and `lib test`. With those three stubbed behind a
`cfg`, **`kindlefill-core` and `kindlefill-cli` typecheck clean for
`x86_64-pc-windows-msvc` with `--all-targets`** — libs, bins, the unit-test modules, and
the 1048-line `virtual_device.rs` integration test, from an empty target directory, with
no errors and no warnings. That is the same target set `cargo clippy --all-targets` gates
in CI, so it is the meaningful check rather than the default one. `mtp-rs` and its WPD
backend, the `windows` crate, and `nusb` all compiled for the Windows target without
complaint.

`libc` is currently an unconditional dependency of `kindlefill-core`. It should move to
`[target.'cfg(unix)'.dependencies]` as part of the same fix — right now it compiles on
Windows only because nothing under `cfg(windows)` happens to reference the unix-only
symbols.

The README's Platforms section says "Windows and Linux are untried rather than
unsupported: the code carries real affordances for both." As of `5ce1aed` that is no
longer true for Windows — it is a hard build break — and it is half-true for Linux, which
compiles but where `MountedKindle::find` looks in `/Volumes` and therefore never finds
anything.

The app crate could not be cross-checked from macOS: `tauri-winres` panics with
`NotAttempted("llvm-rc")`, because embedding a Windows resource needs a resource compiler
this host doesn't have. That is a cross-compilation gap, not a Windows-host problem — on
a `windows-latest` runner with MSVC, `rc.exe` is present. It does mean the app crate's
Windows typecheck is **unverified**; it is 694 lines over core and uses no platform API
except through `ptpcamerad` and `MountedKindle`, so the expectation is that it follows
core, but that is an expectation rather than a result.

## What already works, and why

`mtp-rs` 0.30's WPD backend is not a stub. It is a full `MtpBackend` implementation over
the Windows Portable Devices COM API, with one dedicated thread per open device owning all
the apartment-affine COM pointers, and `Backend::Auto` — what `MtpDevice::open_first()`
uses — **prefers WPD on Windows** and only falls back to raw USB if no WPD device is
present. No driver install, no Zadig.

Every operation this app performs has a real implementation there:

| What the engine needs | WPD backend |
|---|---|
| `storages` / `storage_info` | `IPortableDeviceProperties::GetValues`, re-read on every call — so `Storage::refresh()` does issue a fresh query |
| `create_folder`, `delete`, `list` | implemented |
| Streaming upload of a declared size | implemented, chunk-by-chunk over a bounded channel — peak memory is a few chunks, not the file |
| Progress callback | fires per forwarded chunk |
| `ControlFlow::Break` mid-upload | releases the stream **without** `Commit`, then probes the parent by name and returns the partial's handle |

That last row is the one that matters most, because Stop-deletes-the-partial is the
safety property the whole UI rests on, and it is the WPD path's own code — not something
the engine has to emulate. `engine.rs` and the frontend need no changes for any of this:
the app only ever touches the backend-neutral `mtp::` façade (`MtpDevice`, `Storage`,
`NewObjectInfo`, `CancelToken`), which is exactly the API that auto-selects WPD.

`validate_dir_name` already rejects `\ / : * ? " < > |` and control characters, so it is
Windows-safe as written. (Trailing-dot names still pass, but these are WPD object names
rather than Win32 paths, so it's a footnote.)

One caution for whoever reads the library next: `mtp-rs`'s `src/mtp/backend/mod.rs`
docstring still says a WPD backend "is planned" while `wpd/` is fully implemented. Trust
the code over that comment.

## What breaks at runtime, and needs actual work

Five things. None are deep, but none are free either, and the first two are the kind that
ship broken because the test suite can't see them.

**1. `device_present()` is likely a false negative on Windows.** It calls
`MtpDevice::list_devices()` (`app/src/main.rs:617`), which goes through `nusb` only —
`mtp-rs` has no WPD-based enumeration at all, just WPD opening. On Windows, `nusb`'s
`list_devices` does enumerate driver-agnostically and does populate interface descriptors
from the hub, so a WPD-bound Kindle is *listed*. The problem is the match filter:
`mtp-rs`'s own comment names the Kindle as the example of a device that is vendor-class
(0xFF) with non-standard subclass/protocol and only recognizable by its **endpoint
layout** — and that check lives behind `dev.open()`, which on Windows requires a
WinUSB-bound interface. A Kindle bound to the WPD driver cannot be opened, so the fallback
fails and the list comes back empty.

The consequence is not a silent one. `open_first()` still works (it tries WPD first and
returns before touching `nusb`), so `detect` succeeds and paints the device — and then the
frontend's 2-second presence poll (`ui/index.html:955`) sees `false`, disagrees with
`device`, and paints **"Kindle unplugged"** over a working connection. Day-one visible.
The fix is to route presence through something WPD-aware on Windows, or through
`list_devices_with_known` with Amazon's VID/PID pairs, which short-circuits to a match
without opening anything.

Worth noting in passing: because Kindles need that open-the-device fallback, the presence
poll on **macOS** is also opening the USB device every two seconds, which the comment
above `device_present` ("this only enumerates USB, so it never opens the Kindle") does not
quite describe. It opens no MTP session and starts no tamer, so the intent holds.

**2. A free-space property WPD doesn't report becomes a confident lie.** The backend reads
`WPD_STORAGE_FREE_SPACE_IN_BYTES` with `.unwrap_or(0)`, and `WPD_STORAGE_CAPACITY` the
same way. If a Kindle doesn't expose them, `free = 0` flows into `fill_with_cancel`, trips
the below-the-window guard, and the user is told *"only 0 bytes free, already below the
50 MB target; nothing to fill"* — a precise, actionable, wrong message, when the truth is
"the device didn't tell us." The UI is slightly better defended: `paintCapacity` bails on
`!d.total`, so the bar goes unpainted rather than NaN. Worth a boundary guard that treats
`capacity == 0` as *not reported* rather than as a measurement.

**3. `explain()` has no Windows arm** (`app/src/main.rs:183`). Its three branches offer
macOS `ptpcamerad` guidance and Linux `udev` guidance, both wrong on Windows. WPD is also
multi-client, so `is_exclusive_access` probably never fires the way it does on macOS,
while the failure modes that *do* occur there — a locked device, a wedged WPD session,
Explorer mid-indexing — have no branch. This needs a third arm, not a reworded one.

**4. The mass-storage path is macOS-shaped, not just unix-shaped.** Beyond the three
`libc` calls, `find()` looks for `/Volumes/Kindle` and requires a `documents` directory
as its second signature. Windows needs drive-letter or volume-label enumeration and
`GetDiskFreeSpaceExW`.

The good news is that `5ce1aed` just made this much easier than it would have been.
`engine::run_fill` is now the single owner of the convergence loop, and the transports
supply only I/O through an `engine::FillStorage` trait — `MtpFill` wrapping `Storage`,
`MassFill` wrapping the mounted volume. The naming, token, staged-update and
`validate_dir_name` decisions were already shared. So the Windows work lands squarely in
`MountedKindle`: platform-swap `find`, `space` and `is_writable`, and everything
downstream is already common. **Not** a third fill loop, and no longer even a second one.

**5. Packaging is macOS-only by configuration.** `bundle.targets` is hardcoded
`["app", "dmg"]`, which produces nothing on Windows; it needs `nsis` and/or `msi`. The
icon set is already complete — `icon.ico` and the full `Square*Logo.png` family are
committed. Beyond that there's a WebView2 decision to make (`bundle.windows.
webviewInstallMode`: bootstrapper keeps the installer small but needs network at install
time; embedding adds ~150 MB), and `x86_64` alongside the existing `aarch64`-only build.

## Release and CI

`release.yml` is 100% Apple — Developer ID import, notary submission, `spctl` gate, DMG
Finder-layout injection. A Windows job shares none of it, and `ci.yml` runs
`runs-on: macos-latest` alone. Adding a `windows-latest` matrix entry is cheap.

**Authenticode is plausibly the largest single line item, and it is procurement, not
engineering.** An unsigned Windows binary gets a SmartScreen "unrecognized app" wall that
is worse than the macOS Privacy & Security detour, and reputation accrues per-certificate
with download volume — so unlike notarization, signing correctly on day one still means
early users see a warning for a while.

Two specifics to confirm rather than budget from this document, both being things that
move and neither verified here: current guidance is that OV code-signing certificates
require hardware-backed keys (a token or cloud HSM), which would make the CI story an
HSM or Azure Trusted Signing integration rather than a `.pfx` in a GitHub secret; and
embedding the WebView2 runtime rather than using the bootstrapper is usually quoted at
roughly a hundred-plus MB of installer size. Check both against current Microsoft
documentation before committing to an approach — the first one decides the CI
architecture. Either way it is identity paperwork with a lead time measured in days to
weeks, and it should start in parallel with the code work rather than after it.

## The validation gap, which is the real cost

`cargo test --workspace` on a `windows-latest` runner would go green while exercising
**zero lines of the WPD backend**. The virtual-device suite runs over a `Transport`, which
means it goes through the PTP-over-USB backend; WPD is a sibling of that backend, not a
transport beneath it. So a green Windows CI badge would say the code compiles and the
planner is correct, and nothing whatsoever about whether the app talks to a Kindle. This
repo already documents the same trap for the frontend — "a green CI badge says nothing
about the frontend, so it shouldn't be read as if it does" — and the Windows equivalent
deserves saying in the same voice.

`probe` and `bench` are the instruments, exactly as they were for macOS, and both still
work on Windows: each reaches the device through `open()` → `MtpDevice::open_first()`,
which is the WPD path. Two things about `probe` would mislead a Windows tester, though,
and both are cheap to fix while you're in there. Its `== devices ==` section calls
`MtpDevice::list_devices()`, so per finding #1 it will likely print "none found"
immediately before `== storages ==` successfully opens the same device. And its
`== ptpcamerad ==` section prints "not running (nothing to work around)" — technically
true, structurally vacuous, since off macOS `probe_privileges` can only return
`NotRunning`. A Windows tester reading that would think a check had passed.

Five questions they'd have to answer on a Windows box with a Kindle attached:

| Question | Why it gates | Consequence if wrong |
|---|---|---|
| Does WPD enumerate the Kindle, and does `open_first` pick it? | Everything | No port |
| Does `WPD_STORAGE_FREE_SPACE_IN_BYTES` exist **and move promptly after each write**? | The convergence loop measures rather than tallies | The loop cannot converge; needs rethinking, not fixing. This is the macOS `GetStorageInfo` question again, and it is the one that would pass every test in the repo before failing on the cable |
| Throughput over the extra COM/driver hop | The ~17-minute estimate and every ETA | Cosmetic but user-visible |
| Stop latency, and does the partial get found and deleted? | The safety property the UI promises | Broken promise |
| Does anything hold the device exclusively, and does `device_present()` return true? | Finding #1 above | "Kindle unplugged" over a live cable |

## Rough shape of the effort

Engineering, assuming a Windows machine and a Kindle are available:

| Work | Estimate |
|---|---|
| Make it compile (cfg-split `space`/`is_writable`, move `libc` under `cfg(unix)`, `GetDiskFreeSpaceExW`) | ~1 hour |
| Windows mass-storage detection (volume label + `documents` signature) | half a day |
| WPD-aware `device_present` — may need an upstream `mtp-rs` addition, which is the schedule risk | half a day, or more if upstream |
| `explain()` Windows arm + "capacity not reported" guard | 2–3 hours |
| `probe` honesty fixes (device list, vacuous ptpcamerad line) | ~1 hour |
| Packaging: bundle targets, WebView2 mode, CI matrix, `x86_64` | half a day to a day |
| Hardware validation: `probe`, `bench`, full GUI run, Stop mid-object | a day |

Call it **2–4 days of engineering**, gated on hardware access, with Authenticode
procurement as a separate track that starts earlier and finishes later than any of it.

The honest summary is that this is a *cheap port with an expensive proof*. Nothing in the
architecture resists it — the engine is backend-neutral, the library's Windows backend is
complete, and the one macOS-specific module already no-ops elsewhere. The reason not to
claim Windows support the day it compiles is the same reason this repo doesn't claim
frontend coverage from a green CI run.

## What this analysis did not verify

- The app crate's Windows typecheck (blocked on `llvm-rc` when cross-compiling from macOS).
- Anything under `cargo clippy -D warnings`, which CI also gates on. `cargo check` clean
  does not imply clippy clean.
- Anything at all against Windows hardware. Every runtime claim above is read from source.
- Whether a Kindle's WPD implementation reports the storage properties at all — the single
  assumption that could invalidate the approach on Windows, exactly as it could have on
  macOS.
- Linux, beyond noticing that it compiles and that `/Volumes` makes the mass-storage path
  dead there.
