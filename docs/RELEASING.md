# Releasing

Tag-driven: `git tag -a v0.1.0 -m ... && git push origin v0.1.0`. The workflow
re-runs the three CI gates, builds, signs, notarizes, injects the DMG's Finder
layout, and publishes — picking its release notes from what `spctl` actually says
about the built bundle rather than from whether the job went green.

## Signing and notarization

A local build is **ad-hoc signed** and opens by double-click, because macOS only
quarantines files that arrive from elsewhere. Releases are a different build: the
workflow signs them with a Developer ID and sends them to Apple's notary service, which
is what lets a *downloaded* copy open without the Privacy & Security detour.

Two halves, and they need different credentials.

**Signing** needs a **Developer ID Application** certificate — `APPLE_CERTIFICATE` (the
`.p12`, base64-encoded), `APPLE_CERTIFICATE_PASSWORD` and `APPLE_SIGNING_IDENTITY`. This
is a distinct certificate type: an *Apple Distribution* certificate signs for the App
Store and cannot be used to notarize for distribution outside it, so having one does not
mean you have the other.

**Notarization** takes either an App Store Connect API key — `APPLE_API_ISSUER`,
`APPLE_API_KEY`, `APPLE_API_KEY_PATH` — or an Apple ID with an app-specific password:
`APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID`. Prefer the API key where one already
exists; it can be revoked on its own and isn't a password to somebody's account.

Without any of it the workflow still produces a working DMG. It just isn't notarized,
and the release notes say so rather than claiming otherwise: the build asks `spctl`
whether Gatekeeper actually accepts the bundle and picks between two sets of notes from
the answer, so a failed notarization cannot be published as a successful one.

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


## The DMG's Finder layout

`bundle_dmg.sh` sets the window size, background and icon positions by driving Finder
over AppleScript. A CI runner has no Finder session, so it skips all of it and ships a
DMG that carries the background image and volume icon but nothing telling Finder to use
them — it opens as a plain file list.

`crates/kindlefill-app/dmg/DS_Store` is that missing instruction, captured from a local
build where Finder does exist, and the workflow injects it into the DMG afterwards. Safe
to do after notarization: the ticket is stapled to the `.app` inside, and the injection
only rewrites the container around it. The workflow re-mounts the result and fails if
`.DS_Store`, `.VolumeIcon.icns` or the background are missing.

If you change the DMG window size or icon positions in `tauri.conf.json`, regenerate it:

```bash
cd crates/kindlefill-app && cargo tauri build
MP=$(hdiutil attach ../../target/release/bundle/dmg/*.dmg -nobrowse -readonly | grep -o '/Volumes/.*')
cp "$MP/.DS_Store" dmg/DS_Store
hdiutil detach "$MP"
```
