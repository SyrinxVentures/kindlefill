# Vibe-Audit — kindlefill

**Date:** 2026-08-11 · **Base commit:** `0885e46` · **Branch:** `main` (clean)
**Tier:** DEEP (single-session, full-source read) · **Posture:** report-first, no fixes applied

Repo state at audit time: 58 tests pass (39 unit + 19 virtual-device), `cargo clippy
--all-targets` clean, `cargo fmt --all --check` **fails**.

---

## 1. Repo fingerprint

| Axis | Finding |
|---|---|
| Type | Rust workspace, 3 crates: pure-logic core, clap CLI, Tauri 2 desktop app |
| Frontend | One static `ui/index.html`, ~420 lines of inline ES module, no framework, no build step |
| Deploy target | macOS desktop (ad-hoc signed `.app` + `.dmg`); not notarized |
| Network | **None.** No HTTP client anywhere. Device I/O is USB/MTP via `mtp-rs` 0.30.0 (crates.io, checksummed) |
| Server / DB / BaaS / payments / multiplayer | None |
| Persistence | None on the host — the app stores nothing between launches (this is itself load-bearing; see F4) |
| Precious data | **A stranger's Kindle library.** Books, personal documents, annotations, and ~25 GB of device storage. Also the device's *working state* — the README documents that an interrupted write can wedge a Kindle into answering reads while refusing writes |
| Bus factor | 1 (`git shortlog -sn` empty — solo, unattributed; expected) |
| CI | **None.** No `.github/` |

**Hotspots** (churn × complexity): `ui/index.html` (8 commits, 662 lines, zero tests),
`app/src/main.rs` (8, 564), `core/src/engine.rs` (7, 719). Deep-read budget went here.
All three carry findings.

**Seeds from history.** All 11 commits sampled; five prior fixes named in the audit
brief were used as sibling probes. Two produced new findings by asking "what else
answers this question?" — F4 (the folder-name-resets fix landed on `detect`/`clean`
but not on `fill`'s refusal message) and F2 (the `e.partial` handling landed on
`fill_with_cancel` but not on `bench`).

---

## 2. Applicability & Coverage Matrix

Three independent axes. **Applicability** is a property of the repo; **Coverage** is a
property of this run; **Outcome** is what was found.

| # | Category | Applicability | Coverage | Outcome |
|---|---|---|---|---|
| S1 | Secrets & credentials | APPLICABLE | RUN — HEAD + all 11 commits of `git log -p --all` (14,517 lines), key-shape scan, tracked-file scan | CLEAN |
| S2 | Auth & authorization | **N/A** — no server component; desktop app, no accounts, no remote endpoint | — | — |
| S3 | BaaS exposure | **N/A** — no Supabase/Firebase/any backend service in the stack | — | — |
| S4 | Injection & unsafe rendering | APPLICABLE (HTML + a shell boundary) | RUN — full `index.html` scan; `pkill`/`pgrep` invocations reviewed | CLEAN |
| S5 | Platform hardening | APPLICABLE (CSP only; no server) | RUN — `tauri.conf.json` CSP, `capabilities/default.json` | CLEAN |
| S6 | Client-trust boundary | APPLICABLE — the webview→Rust IPC *is* a trust boundary | RUN — all 6 `#[tauri::command]` entry points | **1** (F3) |
| S7 | Dependency security & provenance | APPLICABLE | SAMPLED — provenance + manifest reviewed by hand; `cargo audit` **NOT RUN** (not installed; needs install + network, outside the approved scope) | CLEAN (provenance) |
| C1 | Silent failure & error swallowing | APPLICABLE | RUN — every `?`, `let _ =`, `catch`, `.unwrap_or` in all 11 files | **2** (F2, F8) |
| C2 | Happy-path-only logic | APPLICABLE | RUN — all error paths in engine/app/cli | **2** (F1, F9) |
| C3 | Hallucinated APIs | APPLICABLE | RUN — `cargo clippy --all-targets` clean; Rust's type system covers the Rust side. JS side read line-by-line | CLEAN |
| C4 | Half-built features | APPLICABLE | RUN — every UI control traced to a handler; zero `TODO`/`FIXME`/`XXX` in the tree | CLEAN |
| C5 | Mock/demo/dev residue | APPLICABLE | RUN | CLEAN |
| C6 | Data lifecycle & destructive writes | APPLICABLE — **highest stakes here** | RUN — all 4 delete paths traced end to end | **2** (F2, F3) |
| T1 | Single-source-of-truth violations | APPLICABLE | RUN — 8 load-bearing decisions enumerated and grepped | **2** (F4, F5) |
| T2 | Dead code & orphans | APPLICABLE | RUN — every `pub` item reference-counted | CLEAN |
| T3 | Architectural coherence | APPLICABLE | RUN | **1** (F12) |
| T4 | Hardcoded config scatter | APPLICABLE | RUN | CLEAN |
| T5 | Data layer | **N/A** — no database. Host-side persistence is *nil*; device state is the only store, and its checks ran under C6 | — | — |
| P1 | Test-suite & CI honesty | APPLICABLE | RUN — all 58 tests read, can-it-go-red applied statically to each guard | **1** (F6) |
| P2 | Build, deploy & release integrity | APPLICABLE | RUN — lockfile, `.gitignore`, generated artifacts, bundle config | **1** (F11) |
| P3 | Docs & onboarding accuracy | APPLICABLE — **"a stranger reads this on GitHub"** | RUN — every README command checked against source | **3** (F2, F7, F15) |
| P4 | Observability | APPLICABLE (lite — local-only tool) | RUN — `KINDLEFILL_TRACE`, log pane, error surfacing | **1** (F1, boot/state honesty limb) |
| P5 | Licensing & asset provenance | APPLICABLE — going public, dual-licensed | RUN — both license files present, workspace `license` field set, icons are own-work | CLEAN |
| I1 | Container & IaC hygiene | **N/A** — no containers, no IaC, no compose/Dockerfile | — | — |
| I2 | State & backups | **N/A** — no server-side state to back up; host stores nothing | — | — |

**Not run, stated plainly:** `cargo audit` (would require installing a tool and hitting
the advisory feed — outside the approved read-only scope). Windows: nothing was compiled
or run on Windows; the README's feasibility claim is assessed as a *documentation* claim
only (F15), not verified.

---

## 3. Executive summary

**15 findings: 0 critical, 2 high, 4 medium, 9 low. 14 CONFIRMED, 1 PLAUSIBLE. 1 REFUTED
(appendix D).**

The Rust core is genuinely good. `plan.rs` is I/O-free and stays that way; every
invariant in the audit brief holds under inspection — free space is re-read via
`storage.refresh()` after every write with no tally anywhere on the decision path, no
ladder rung comes within 3 GiB of the 32-bit `ObjectInfo` ceiling, deletion is bounded to
`filler_sequence`-matched names plus the two opt-in paths, and progress figures are
clamped at the source with tests that pin `0/0 → 1.0` rather than `NaN`. The
single-owner extraction of `filler_sequence` is correct and its round-trip check is
stronger than a digit-count would have been. **No P0 invariant violation was found.**

The defects cluster exactly where you predicted: **the untested frontend, and the
seam between it and the Rust commands.** Both high findings are cases where a safety
property was implemented once, correctly, and its sibling was left alone.

The single most important result: **the frontend's "one operation at a time" rule is
not enforced anywhere it matters.** `setBusy()` disables eight controls and forgets the
ninth — the Overwrite checkbox — and one of the three places that computes "is Fill
enabled" doesn't consult `busy`. Toggling that checkbox during a fill re-enables Fill,
and a second click starts a second `start_fill` that clobbers the first one's cancel
token. I confirmed this by running your actual `index.html` against a stubbed backend:
the result is a 17-minute transfer that **cannot be stopped**, behind a header reading
"No Kindle found", while the presence poll logs "Kindle reconnected" every two seconds.
The user's only exit is unplugging the cable mid-write — which your own README documents
as the thing that wedges a Kindle. There is no backend re-entrancy guard to catch it.

Second: **`bench` is documented as "self-cleaning" and isn't.** Every step is `?`, so a
failure part-way through leaves up to 656 MB in a `kindlefill_bench` folder that *no
command in this tool can remove* — the names don't match `filler_sequence` and the folder
isn't `fill_disk`. It also ignores the `e.partial` handle that `fill_with_cancel` handles
correctly 400 lines away. That's the same sibling-divergence shape as the `fill_notes.bin`
bug you already fixed.

Everything else is medium or low, and several are one-line fixes worth taking before the
repo goes public.

---

## 4. Findings

Grouped by theme, ranked by user harm.

### Theme A — Concurrency and the operation lifecycle

---

#### F1 · Fill can be restarted mid-fill, stranding an uncancellable transfer behind a false "No Kindle found"
**Severity: High** · **Verdict: CONFIRMED (executed, not inferred)** · C2, P4, T1
**Sites:** `ui/index.html:325-336`, `:557-559`, `:595-611`, `:641-657`; `app/src/main.rs:428-433`, `:459`, `:493`

`setBusy()` disables every interactive control except one:

```js
function setBusy(on) {
  busy = on;
  $('fill').disabled = on || !device || blockedByForeign();
  $('clean').disabled = on || !device;
  $('refresh').disabled = on;
  $('stop').disabled = !on;
  $('low').disabled = on; $('high').disabled = on;
  $('dirname').disabled = on; $('rename').disabled = on;
  $('delupdates').disabled = on;
}                                    // <- #overwrite is never disabled
```

The Overwrite checkbox lives in `#foreignwarn`, which stays visible for the whole fill
(nothing re-renders it). Its handler recomputes Fill's enabled state **without consulting
`busy`**:

```js
$('overwrite').onchange = () => {
  $('fill').disabled = !device || !device.writable || blockedByForeign();
};
```

**Failure scenario** (each step observed in a harness driving the real `index.html`
against a stubbed backend; `start_fill` modelled as a never-settling promise, and
subsequent `open_device()` calls modelled as failing with `ExclusiveAccess`, which is
what `main.rs:200-213` produces once a fill holds the device):

| Step | `#fill.disabled` | `#overwrite.disabled` | `start_fill` calls | header |
|---|---|---|---|---|
| after detect (folder holds foreign content) | `true` | `false` | 0 | Kindle connected |
| user ticks Overwrite | `false` | `false` | 0 | Kindle connected |
| user clicks Fill — transfer running | `true` | **`false`** | 1 | Kindle connected |
| untick Overwrite *during the fill* | `true` | `false` | 1 | Kindle connected |
| **retick Overwrite during the fill** | **`false`** | `false` | 1 | Kindle connected |
| **click Fill a second time** | `true` | `false` | **2** | Kindle connected |

Then the second `start_fill` fails to open the held device and rejects. The JS `finally`
runs unconditionally:

```js
} finally {
  setBusy(false);        // <- fill #1 is still running
  await refresh();       // <- opens the device; fails; -> showNoDevice('No Kindle found')
}
```

Measured end state, with fill #1 **still running**:

- `devline` = `"No Kindle found"`
- `#stop.disabled` = `true` — the Stop button is gone
- `#dirname.disabled` = `false` — the folder field is live again mid-transfer
- presence poll un-gated (`busy === false`), so it alternates
  `"Kindle reconnected."` / `"Another program is holding the Kindle…"` every 2 s
  for the remainder of the transfer

The backend makes it unrecoverable rather than merely ugly. `start_fill` stores its token
*before* it can fail:

```rust
let token = CancelToken::new();
{ *state.cancel.lock().unwrap() = Some(token.clone()); }   // main.rs:428-433
let mut session = open_device().await?;                     // fill #2 dies here
```

Fill #2 has already replaced fill #1's token. Even if the user could reach Stop,
`cancel_fill` would now cancel a token nothing is watching. `start_clean:493` is a second
route to the same state — it sets `*state.cancel.lock() = None` unconditionally.

**Harm:** a 17-minute write the user cannot stop, with the UI actively lying about device
state. The only escape is unplugging mid-write — which `lib.rs:57-67` and the README both
document as what leaves a Kindle "answering reads while refusing every write."

**Sibling status:** three separate call sites compute "is Fill enabled" (see F5); this is
the one that omits `busy`. The backend has **no** re-entrancy guard on any of the four
device-opening commands.

**Fix direction:** (1) one `updateControls()` owner for button state, called from every
transition, reading one predicate set; (2) add `#overwrite` to it; (3) a backend
`Mutex<()>` / `AtomicBool` "operation in flight" guard so a second `start_fill`,
`start_clean`, `delete_updates` or `detect` is refused with a clear error instead of
racing for the device; (4) only clear `state.cancel` if the stored token is the one this
invocation put there.

---

#### F9 · `AppState.cancel` is stored before the first fallible call
**Severity: Low** · **Verdict: CONFIRMED** · C2
**Site:** `app/src/main.rs:428-435`, `:443-445`

The token is published to shared state before `open_device()` and before
`purge_fill_dir()`. Either returning `Err` leaves `Some(stale_token)` behind, because
both use `?` and skip the `= None` at line 459.

**Accurate harm: hygiene, not behaviour.** A stale token is overwritten by the next
`start_fill`, and `cancel_fill` on a token nothing awaits is a no-op — Stop is disabled
on those paths anyway. Flagged in the audit brief, so recorded; it is not a live bug on
its own. It becomes load-bearing only as one limb of F1, where the *clobbering* (not the
staleness) is what strands the fill. Fix it as part of F1's ownership rule.

---

### Theme B — Deletion, and what the tool can prove it wrote

---

#### F2 · `bench` strands up to 656 MB on a user's Kindle with no in-tool recovery path, and the README calls it "self-cleaning"
**Severity: High** · **Verdict: CONFIRMED** · C1, C6, P3
**Sites:** `cli/src/main.rs:166-223`; `README.md:173`; twin at `core/src/engine.rs:598-604`

```rust
async fn bench() -> Result<()> {
    // ...
    let dir = storage.create_folder(Some(ObjectHandle::ROOT), "kindlefill_bench").await?;
    let mut written = Vec::new();
    for (label, size) in [("16MiB", 16*MIB), ("128MiB", 128*MIB), ("512MiB", 512*MIB)] {
        // ...
        let handle = storage.upload(...).await
            .map_err(|e| anyhow::anyhow!("upload of {label} failed: {}", e.source))?;
        //                                                              ^^^^^^^^
        //   e.partial is dropped on the floor
        written.push(handle);
        storage.refresh().await?;     // any of these `?` exits with objects committed
    }
    print!("cleaning up... ");        // <- only reached on the fully-happy path
    for handle in written { storage.delete(handle).await?; }
    storage.delete(dir).await?;
```

Every step between folder creation and cleanup is `?`. A failure at the 512 MiB upload —
the most likely one, being the longest — exits with the 16 MiB and 128 MiB objects
committed, the folder present, and possibly a partial third object.

**Nothing in this tool can remove any of it.** `clean` only deletes names that satisfy
`filler_sequence` (`fill_NNNN.bin`); `bench_512MiB.bin` doesn't. `find_filler_folders`
only reports folders containing `filler_sequence` matches, so `kindlefill_bench` is
invisible to discovery too. `purge_fill_dir` would work but requires the user to guess the
folder name and pass `--dir kindlefill_bench --overwrite`, which is documented nowhere.
The user is left with up to 656 MB gone and no way to find out why.

**The twin, and the sibling question answered.** `fill_with_cancel` handles exactly this
hazard 400 lines away, with a comment explaining why:

```rust
// engine.rs:598-604
if let Err(e) = upload {
    // A failed data phase can leave a partial object on the device.
    // The library deliberately doesn't auto-delete it; if we leaked it,
    // it would consume space that no `clean` run could find.
    if let Some(partial) = e.partial { let _ = storage.delete(partial).await; }
```

`bench` names the same field (`e.source`) in the same expression and ignores `e.partial`.
The reasoning was written down once and not applied to the sibling — the same shape as the
`list_fillers` / `next_sequence` divergence already fixed.

**The doc claim.** `README.md:173`:

> `cargo run -p kindlefill-cli -- bench     # throughput + free-space sanity; self-cleaning`

True on the happy path, false on exactly the path where it matters. This is pattern #2
from the brief — an operation that can't distinguish "did it" from "didn't" — one level
up, at the doc.

**Fix direction:** wrap the body so cleanup runs on every exit (collect results, then
clean up before propagating); handle `e.partial` as `fill_with_cancel` does; and on
unrecoverable cleanup failure, print the exact folder name and the exact
`--dir kindlefill_bench --overwrite` command that removes it. Soften the README to
"cleans up after itself, and tells you what to remove if it can't."

---

#### F3 · `purge_fill_dir` will empty any root folder its caller names; nothing server-side checks the user was shown that folder's contents
**Severity: Medium** · **Verdict: CONFIRMED** · S6, C6
**Sites:** `app/src/main.rs:417-450`; `core/src/engine.rs:378-394`

`start_fill` takes `dir_name` and `overwrite` as two independent parameters from the
webview and never relates them:

```rust
async fn start_fill(app, state, low: u64, high: u64, dir_name: String, overwrite: bool)
    -> Result<String, String> {
    // ...
    if overwrite {
        let removed = engine::purge_fill_dir(storage, &dir_name, ...).await
```

`purge_fill_dir` then deletes everything under that folder, recursively, including content
this tool did not write. Its own doc comment is precise about the guarantee it *does*
offer — scoped to one named root folder, can't be pointed at the root — and that
guarantee holds. What it does **not** verify is that the caller ever showed the user what
is in there.

The entire binding between "the user confirmed this folder's contents" and "this folder
gets emptied" is one line of frontend JS (`index.html:419`, `$('overwrite').checked =
false` inside `renderDevice`). That line is correct and always runs (verified — appendix
C). But it is the *only* thing standing between a confirmation and a deletion, it lives in
the ~420 lines of untested JS, and F1 demonstrates that the frontend's state machine can
be driven into states its author didn't model.

Contrast `delete_staged_updates`, which gets this right by construction: it intersects the
caller's name list with what is *actually* a root-level `update_*.bin`, so a name arriving
from the UI that isn't one deletes nothing. Three gates, all server-side. `purge_fill_dir`
has one gate, and it's client-side.

**This is defense-in-depth, not a live exploit** — no path was found that reaches
`purge_fill_dir` with a folder the user hadn't confirmed. Filed at medium on that basis.

**Fix direction:** make the confirmation reference what it confirms. Have `detect` return
a token/digest of the foreign set it displayed for `dir_name`; require `start_fill` to
echo it back with `overwrite`, and refuse the purge if it doesn't match what's on the
device now. That also closes the (currently theoretical) window where content appears in
the folder between the confirmation and the purge.

---

#### F4 · Below the window, `fill` says "nothing to fill" without mentioning the filler sitting in a renamed folder — `clean` answers the same question correctly
**Severity: Medium** · **Verdict: CONFIRMED** · T1, C1
**Sites:** `core/src/engine.rs:476-488`, `:134-138`; correct sibling at `app/src/main.rs:505-514`

`fill` decides whether the device has filler it can give back by looking **only at the
configured folder**:

```rust
let free_start = measure(storage).await?;
if free_start < window.low()
    && match find_fill_dir(storage, dir_name).await? {
        Some(dir) => list_fillers(storage, dir.handle).await?.is_empty(),
        None => true,
    }
{
    return Err(FillError::AlreadyBelowWindow { free: free_start, low: window.low() });
}
```

which renders as:

```
only 71303168 bytes free, already below the 78643200-byte target; nothing to fill
```

The refusal itself is defensible — `fill` manages its configured folder. **The message is
not.** Nothing persists the folder name between launches (the fix note at
`engine.rs:306-315` says so explicitly), so the default is exactly what a relaunched app
will be holding while 24.88 GB of filler sits under the name the user chose. The user is
told, truthfully, that there is nothing to fill — and left to conclude the space is the
device's, when this tool wrote it and could give it back.

This is the same defect class as the `clean` bug you already fixed, and `start_clean`
already carries the correct answer:

```rust
// main.rs:505-514 — the shape this should match
let others = engine::find_filler_folders(storage).await...;
match others.iter().find(|f| f.name != dir_name) {
    Some(f) => Ok(format!("Nothing to remove in {dir_name}, but {} of filler is in {} — \
                           switch to that folder and try again.", human_bytes(f.bytes), f.name)),
```

The fix landed on `detect` and on `clean`. `fill`'s refusal path is the twin it missed.

**Fix direction:** before returning `AlreadyBelowWindow`, call `find_filler_folders` and
name the folder — same wording as `start_clean`. Consider carrying the candidate list on
the error variant so both front ends render it without re-deciding.

---

### Theme C — One question, several answers

---

#### F5 · Five sites decide "is Fill enabled", each from a different subset of the conditions
**Severity: Medium** · **Verdict: CONFIRMED** · T1
**Sites:** `ui/index.html:327`, `:435`, `:455`, `:466`, `:558`

| Site | Expression | `busy` | `!device` | `writable` | `blockedByForeign` |
|---|---|:-:|:-:|:-:|:-:|
| `setBusy` :327 | `on \|\| !device \|\| blockedByForeign()` | ✅ | ✅ | ❌ | ✅ |
| `renderDevice` :435 | `!d.writable \|\| blockedByForeign()` | ❌ | n/a | ✅ | ✅ |
| `overwrite.onchange` :558 | `!device \|\| !device.writable \|\| blockedByForeign()` | ❌ | ✅ | ✅ | ✅ |
| `refresh` :466 | `true` | — | — | — | — |
| `showNoDevice` :455 | `true` | — | — | — | — |

Three of the five disagree, and every disagreement is reachable:

- **`setBusy` omits `writable`.** On a read-only storage, finishing any operation
  (`setBusy(false)`) enables Fill. Backend catches it — `fill_with_cancel:469` returns
  `ReadOnly` — so this one fails safe, and it self-corrects on the trailing `refresh()`.
- **`overwrite.onchange` omits `busy`.** This is the F1 trigger, and it does *not* fail
  safe.
- **`renderDevice` omits `busy`.** Currently unreachable (it only runs via `refresh`,
  which callers gate on `busy`) — but it's the same latent shape, and nothing enforces
  the gate.

`#clean` has the same split across `setBusy:328` (`on || !device`) and `renderDevice:436`
(`d.filler_files === 0`), which is why Clean flickers enabled-then-disabled after every
operation.

**Fix direction:** one `updateControls()` reading `{busy, device, detecting}` and deriving
every button's state; every transition calls it and nothing else touches `.disabled`.
This is the single-owner extraction that also fixes F1.

---

#### F12 · `Removal` and `next_removal` are the only public `plan` items not re-exported from `lib.rs`
**Severity: Low** · **Verdict: CONFIRMED** · T3
**Sites:** `core/src/lib.rs:29`; `core/src/plan.rs:161`, `:191`

```rust
pub use plan::{next_step, Step, Window, WindowError, GIB, KIB, MIB};
```

`plan.rs` exports seven public items; five are re-exported, `Removal` and `next_removal`
are not. They're reachable as `kindlefill_core::plan::next_removal`, so nothing breaks —
but a consumer following the crate's own convention won't find the half of the convergence
API that handles the below-window case, which the README describes as a headline feature.
Almost certainly an oversight from the bidirectional-fill commit (`0885e46`) rather than a
decision. Also: `lib.rs:23-28` has a stray line break mid-list that `cargo fmt` would
close (see F11).

---

### Theme D — Documentation a stranger will read

---

#### F7 · `probe` is documented as "changes nothing"; it SIGKILLs a system daemon
**Severity: Low** (harm) / **Medium** (public embarrassment) · **Verdict: CONFIRMED** · P3
**Sites:** `README.md:172`; `cli/src/main.rs:117`; `core/src/ptpcamerad.rs:57-65`, `:40-51`

```
cargo run -p kindlefill-cli -- probe     # what do we see? changes nothing
```

`probe()` calls `ptpcamerad::probe_privileges()`, which is documented as answering "by
trying, not assumed" — and the trying is real:

```rust
pub fn probe_privileges() -> PrivilegeCheck {
    if !is_running() { return PrivilegeCheck::NotRunning; }
    match kill_once() { ... }          // pkill -9 -x ptpcamerad
```

So `probe` sends `SIGKILL` to Apple's camera daemon, and separately `open()` starts a
`Tamer` that keeps killing it every 500 ms for the duration. The design is defensible and
well argued in `ptpcamerad.rs:1-13` — scoped rather than permanent. The README sentence is
the problem: a reader running `probe` on the strength of "changes nothing" is signalling a
system daemon they weren't told about.

**Fix direction:** README — "probe: what do we see? Doesn't touch the device's contents
(it does kill `ptpcamerad`, briefly — see below)." One clause, and it makes the tool look
more careful rather than less.

---

#### F15 · Windows support is claimed on an untested basis
**Severity: Low** · **Verdict: CONFIRMED** · P3
**Sites:** `README.md:8`; `app/src/main.rs:12`; `core/src/ptpcamerad.rs:107-132`

The README says "the stack is portable but untested elsewhere", and the code carries real
Windows affordances (`windows_subsystem = "windows"`, a non-macOS `ptpcamerad` no-op
module, `icon.ico` in the bundle set). Nothing has been compiled or run on Windows. The
current wording is *honest* — it says untested — so this is a low-severity note rather
than a false claim. Worth pre-empting the issue tracker: state plainly that
Windows/Linux have never been built, and that the MTP backend difference (WPD vs libusb)
is the unknown, not the app code.

---

### Theme E — Small, cheap, worth taking before the repo is public

---

#### F8 · Every `Event::Deleted` is charged to the filler tally, including staged updates and foreign files
**Severity: Low** · **Verdict: CONFIRMED** · C1
**Sites:** `app/src/main.rs:263`; `ui/index.html:363-374`

```rust
Event::Deleted { bytes, .. } => device_update(app, None, -1, -(*bytes as i64)),
```

Three producers emit `Deleted`: `clean`/the fill's removal branch (genuinely filler),
`purge_fill_dir` (may be foreign content), and `delete_staged_updates` (a firmware image,
never filler). All three decrement `device.filler_files` / `filler_bytes` in the header.
Deleting a 1.5 GB staged update from a device holding 10 filler files momentarily reads
"9 files, 8.5 GB".

Self-corrects — the `Math.max(0, …)` clamps prevent nonsense, and every caller's `finally`
runs `refresh()` within a second. Low on that basis. The structural point is that the
event carries no kind, so the consumer has to guess.

**Fix direction:** add a discriminant to `Event::Deleted` (`kind: Filler | Foreign |
Update`) and let `forward` attribute the delta correctly.

---

#### F11 · `cargo fmt --all --check` fails
**Severity: Low** · **Verdict: CONFIRMED** · P2
`cargo clippy --all-targets` is clean but the tree is not rustfmt-clean (`lib.rs:23-28`
and `engine.rs:75-77` among others). For a repo about to accept contributions, the first
PR will either fight the formatter or silently reformat unrelated lines. Fix before
publishing, then gate it (F6).

---

#### F10 · MiB is labelled "MB" throughout
**Severity: Low** · **Verdict: CONFIRMED** · P3
**Sites:** `core/src/lib.rs:80-88`; `core/src/plan.rs:12-14`; `cli/src/main.rs:57-72`; `ui/index.html:244`

`human_bytes` divides by `GIB`/`MIB`/`KIB` and labels the result GB/MB/KB; `parse_size`
maps both `mb` and `mib` to 1024²; the UI's `MB` constant is 1024². **Internally
consistent** — every surface agrees, so no arithmetic bug follows, and the deliberate
`humanBytes`/`human_bytes` duplication can't disagree (appendix C). It is simply
non-SI, and the README quotes device capacities ("25.46 GB") that a reader will compare
against Amazon's spec sheet, which uses SI GB. Either switch the labels to GiB/MiB or add
one README line saying the tool means binary units.

---

#### F13 · Progress bar has no accessible name; the log live-region re-announces wholesale
**Severity: Low** · **Verdict: CONFIRMED** · accessibility
**Sites:** `ui/index.html:233`, `:237`, `:256-261`

`<div class="bar" role="progressbar" aria-valuemin="0" aria-valuemax="100"
aria-valuenow="0" id="bar">` has no `aria-label`/`aria-labelledby`, so a screen reader
announces an unnamed progressbar. And `log()` does `el.textContent += …` on a container
carrying `role="log" aria-live="polite"`, which replaces the whole text node — assistive
tech may re-read the entire log on each append rather than just the new line.

Worth noting because the CSS work here is otherwise conspicuously careful about
accessibility: the `--accent`/`--accent-on` split, the documented contrast ratios, the
`prefers-reduced-motion` block, `.sr-only` on the heading. These two gaps are out of
character with the rest.

**Fix direction:** `aria-label="Fill progress"` plus `aria-valuetext` carrying the human
string; append child nodes to the log instead of rewriting `textContent`.

---

#### F14 · `parse_size` silently coerces negatives and non-numbers to 0
**Severity: Low** · **Verdict: CONFIRMED** · C2
**Site:** `cli/src/main.rs:57-72`

```rust
digits.parse::<f64>()
    .map_err(|_| format!("not a size: {raw}"))
    .map(|n| (n * scale as f64) as u64)
```

Rust's float→int cast saturates, so `--low -5MB` parses cleanly to `0` and `--low nanMB`
also yields `0` (NaN casts to 0). `Window::new(0, 90MB)` is valid, so the CLI accepts it
and fills toward a 45 MB aim without complaint. Not dangerous — the window still bounds
the fill — but the user asked for something the tool didn't do and wasn't told.

**Fix direction:** reject non-finite and negative values explicitly before the cast.

---

#### F6 · No CI, no frontend tests, and no guardrail protecting the single-owner invariant
**Severity: Medium** · **Verdict: CONFIRMED** · P1
**Sites:** repo root (no `.github/`); `ui/index.html` (~420 lines of JS, zero tests)

Three limbs:

1. **No CI.** No `.github/`. "58 tests pass, clippy clean" is a claim with nothing that
   can go red. For a public repo taking PRs, nothing stops a contributor breaking the
   convergence loop.
2. **No guardrail on `filler_sequence`.** The comment at `engine.rs:255-267` is emphatic
   that this function is the single owner of "is this filler we wrote", and the tests at
   `:697-718` pin its *behaviour* well. But nothing detects **copy N+1** — a future
   `strip_prefix(FILL_PREFIX)` elsewhere reintroduces the exact `fill_notes.bin` bug and
   the suite stays green. Per the Single-Path Principle, a new shared owner should land
   with a grep-based guardrail proven red.
3. **Zero frontend tests.** F1, F5, F8 and F13 all live in the untested JS; F1 in
   particular took a 60-line harness to demonstrate and would have been caught by any
   state-machine test. The harness I built for this audit is a working starting point.

**Can-it-go-red check applied to the existing suite:** the 58 tests are genuinely good —
no vacuous asserts, no tests of mocks, no `.only`/`#[ignore]`. `virtual_device.rs` runs
against a real directory, and several tests pin properties rather than outputs
(`filler_and_foreign_partition_the_folder_exactly` is a real invariant test). The guard
that *cannot* go red is the repo-level one: there isn't one.

**Fix direction:** a `.github/workflows/ci.yml` running `cargo test --workspace`,
`cargo clippy --all-targets -- -D warnings`, `cargo fmt --all --check`; a guardrail test
that greps the tree for a second implementation of the filler-name predicate and fails on
a second match; and a headless DOM test for the frontend state machine.

---

## Appendix A — CLEAN (checked, found healthy)

Recorded so absence of a finding is distinguishable from absence of looking.

| Area | Evidence |
|---|---|
| **P0: free space never tallied** | Every decision reads `measure()` → `storage.refresh()` → `info().free_space`. `engine.rs:508` (loop head), `:616` (after each write), `:475` (start), `:250`, `:392`, `:660`. The only running total, `committed` (`:550`), is re-derived from a fresh device reading each iteration and explicitly discarded — comment at `:544-549` is accurate |
| **P0: no object near 4 GiB** | `LADDER` tops out at 1 GiB; `plan.rs:434-438` asserts every rung `< u32::MAX` |
| **P0: deletion is bounded** | Four delete paths, all traced: `clean` (filler only), fill's removal branch (`list_fillers` set only), `purge_fill_dir` (opt-in), `delete_staged_updates` (three gates). No fifth path exists |
| **P0: progress clamped** | `fraction()` returns 1.0 for `total == 0`, `.clamp(0.0, 1.0)` otherwise; `eta` clamps *seconds* before `from_secs_f64` to dodge its overflow panic. Pinned by `fraction_never_leaves_the_unit_interval` and `eta_is_capped_rather_than_astronomical` |
| **P0: `plan.rs` stays I/O-free** | Imports nothing but `std::fmt`; no `async`, no `mtp_rs` |
| **Deliberate deviation: `humanBytes` ≡ `human_bytes`** | Same unit table, same 1024 bases, same 2-dp format, same `N B` fallback. JS input is clamped `≥ 0` (`index.html:367-368`) so it can't be handed a negative Rust would never see, and magnitudes stay far inside `Number.MAX_SAFE_INTEGER`. **They cannot disagree.** Reasoning holds |
| **Deliberate deviation: `human_duration` vs `human_eta`** | Genuinely different questions; `human_eta`'s `.max(step)` floor correctly prevents "0s" while work remains (`rate.rs:221`), pinned by `a_nearly_finished_eta_never_rounds_down_to_nothing`. Reasoning holds |
| **Deliberate deviation: `.callout` wells in a card-free layout** | Consistent: no other boxed element on the plane, so callouts read as exceptional. The `--warn-surface`/`--warn-ink` pair carries its own foreground rather than reusing a text colour on a tint. Reasoning holds |
| **Overwrite tick can't be spent on another folder** | `renderDevice` sets `$('overwrite').checked = false` on **both** branches (`:419`, `:421`), so every re-detect clears it unconditionally. README claim verified |
| **Rate estimator is bytes ÷ elapsed** | `windowed()` subtracts two cumulative readings over real elapsed time; no rate averaging anywhere. Backwards readings clear history rather than blending (`:81-90`). Pinned by `bursty_transfer_reports_its_true_average_not_its_peaks` and `reported_rate_does_not_depend_on_callback_cadence` |
| **Secrets** | HEAD and all 11 commits (14,517 diff lines) scanned for key shapes: zero hits. No tracked `.env`/`.pem`/`.key`. `.claude/settings.local.json` is untracked |
| **Injection / unsafe rendering** | Zero `innerHTML`/`eval`/`document.write`/`insertAdjacentHTML`. All DOM built via `textContent` + `createElement` — including the filename lists, which is exactly where a device-supplied string reaches the UI. The one shell boundary (`pkill`/`pgrep`) uses absolute paths and fixed argv with no interpolation |
| **CSP & capabilities** | `default-src 'self'; style-src 'self' 'unsafe-inline'`; capability set is `core:default` only — no fs, shell, or http permissions |
| **Dependency provenance** | `mtp-rs 0.30.0` from crates.io with checksum (not a path/git dep). All others are ecosystem-standard. No slopsquat candidates |
| **Dead code** | Every `pub` item has real callers; zero `TODO`/`FIXME`/`XXX`/`todo!()` in the tree |
| **Mock/dev residue** | No `localhost`/`example.com`/`mockData`/`faker`. `KINDLEFILL_TRACE` and `KINDLEFILL_DEVTOOLS` are env-gated and the latter is `#[cfg(debug_assertions)]` — the healthy gated-harness pattern |
| **Generated artifacts** | `gen/schemas/` is `.gitignore`d **and** genuinely untracked (`git ls-files` empty) — one producer, no conflict risk |
| **Licensing** | Both `LICENSE-MIT` and `LICENSE-APACHE` present; workspace `license = "MIT OR Apache-2.0"`; contribution clause present; matches `mtp-rs` |
| **Test-suite honesty** | 58 tests, no vacuous asserts, no mocked units, no `.only`/`#[ignore]`; several are property/invariant tests |

## Appendix B — PLAUSIBLE (couldn't be settled statically)

**Device-driven oscillation between the write and removal branches.** `plan.rs:186-189`
argues termination from "`next_step` never proposes a write that lands below `low`", and
`max_safe` (`:147`) enforces it for the *planned* size. Actual free space after a write
falls by `bytes + per-file overhead`, so the real landing point can be below the planned
one. In practice the margin is large — the ladder pick never exceeds `free - aim`, leaving
at least half the window width (≥ 2 MiB) of slack against the 4 KB overhead the README
measured — so this is safe on the observed hardware. What can't be settled statically is
the *device-driven* case: if the Kindle grows its own files (indexing, thumbnails) faster
than the loop converges, free space can re-enter the removal branch after a write, and the
engine has no equivalent of the test's `assert_eq!(writes, 0, "removal after a write means
oscillation")` guard. **What would settle it:** a long fill on hardware with the device
actively indexing, watching for a `Deleted` event emitted after any `Wrote` event.
Cheap instrumentation: `KINDLEFILL_TRACE=1` already logs both.

## Appendix C — REFUTED

**"The Overwrite confirmation can be spent on a different folder."** I pursued this
because it would have been a critical data-loss path: `$('fill').onclick` reads
`dirName()` from the live input and `overwrite` from the live checkbox, and the checkbox
is only reset inside `renderDevice`, which sits behind an `await`. The suspected sequence
was: tick Overwrite for `fill_disk`, type a new folder name, click Fill before the
re-detect lands, and purge the *new* folder on a confirmation given for the old one.

**Refuted.** Two independent mechanisms close it. (1) `blur` fires before `click`, so
`$('dirname').onblur → applyDirName() → refresh()` runs first, and `refresh()` sets
`$('fill').disabled = true` **synchronously**, before its first `await` — a disabled
button doesn't dispatch `click`. (2) When a detect is already in flight, the re-entrant
`refresh()` returns early without re-enabling anything, and the completing detect's
`renderDevice → finally → refresh()` sequence is fully synchronous up to the next await,
so no task boundary exists for a click to land in.

Recorded so the next audit doesn't re-find it. Note that F3 makes the related-but-weaker
*structural* point — that this safety property has no server-side backstop — on its own
evidence, at medium.

## Appendix D — Coverage honesty

**Exhaustive:** every source file was read in full (11 files, ~3,400 lines excluding
`Cargo.lock` and generated schemas). No sampling was needed anywhere in the source tree —
this is a small repo and the whole of it was read. All 58 tests read individually. All 11
commits reviewed.

**Executed, not just read:** `cargo test --workspace` (58 pass), `cargo clippy
--all-targets` (clean), `cargo fmt --all --check` (fails), and a stubbed-backend DOM
harness driving the real `ui/index.html` for F1 (two runs: control-state matrix, then
the downstream failure state).

**Bounded / not covered:**
- `cargo audit` **NOT RUN** — not installed; would need a tool install plus a network
  call to the advisory DB, outside the approved scope. Dependency *provenance* was
  checked by hand; known-vulnerability status was not.
- **Windows and Linux: never compiled.** F15 assesses the README's claim as a claim.
- **Hardware paths: not exercised.** Four paths rest on the test suite alone — the
  bidirectional removal branch, `purge_fill_dir`, `delete_staged_updates`, and
  `ptpcamerad` taming. The audit did not connect a device (read-only scope). Appendix B
  is the one question that genuinely needs hardware.
- **Frontend: one harness assertion, not a suite.** The harness proved F1. F5, F8 and
  F13 were confirmed by reading, not by execution.
