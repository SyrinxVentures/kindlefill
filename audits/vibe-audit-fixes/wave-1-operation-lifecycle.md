# Fix Wave 1 — One owner for control state, one operation at a time

**Base commit:** `0885e46` · **Source:** `audits/vibe-audit-2026-08-11.md` findings F1, F5, F9
**Severity:** High — this is the wave that matters. Land it before publishing.

You do not need the audit conversation. Everything required is quoted below.

---

## Read these first, in this order

1. `crates/kindlefill-app/ui/index.html` — lines 240–660 (the whole inline module)
2. `crates/kindlefill-app/src/main.rs` — lines 28–32 (`AppState`), 416–535 (the four commands)
3. `README.md` — the "Stop is wired to a real cancel token" paragraph, which this must keep true

---

## The bug, demonstrated

`setBusy()` disables eight controls and forgets the ninth. `ui/index.html:325-336`:

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

`#overwrite` lives inside `#foreignwarn`, which stays visible for the whole fill. Its
handler recomputes Fill's state **without consulting `busy`** — `ui/index.html:557-559`:

```js
$('overwrite').onchange = () => {
  $('fill').disabled = !device || !device.writable || blockedByForeign();
};
```

I confirmed the consequence by driving the real `index.html` against a stubbed backend
(`start_fill` = a never-settling promise; later `open_device()` calls rejecting with
`ExclusiveAccess`, which is what `main.rs:200-213` produces once a fill holds the device):

| Step | `#fill.disabled` | `#overwrite.disabled` | `start_fill` calls |
|---|---|---|---|
| fill running | `true` | **`false`** | 1 |
| **retick Overwrite mid-fill** | **`false`** | `false` | 1 |
| **click Fill again** | `true` | `false` | **2** |

The second `start_fill` fails to open the held device and rejects. The JS `finally` runs
regardless — `ui/index.html:606-610`:

```js
} finally {
  setBusy(false);        // fill #1 is still running
  await refresh();       // opens the device; fails; -> showNoDevice('No Kindle found')
}
```

**Measured end state, with fill #1 still running:** header reads `"No Kindle found"`,
`#stop.disabled === true`, `#dirname.disabled === false`, and the presence poll
(now un-gated because `busy === false`) alternates `"Kindle reconnected."` /
`"Another program is holding the Kindle…"` every 2 s for the rest of the transfer.

The backend makes it unrecoverable rather than merely ugly — `main.rs:428-435`:

```rust
let token = CancelToken::new();
{ *state.cancel.lock().unwrap() = Some(token.clone()); }   // published before it can fail
let mut session = open_device().await?;                     // fill #2 dies here
```

Fill #2 has already replaced fill #1's token, so even a reachable Stop would cancel a
token nothing is watching. `start_clean` (`main.rs:493`) is a second route to the same
state — it does `*state.cancel.lock().unwrap() = None;` unconditionally.

**User harm:** a 17-minute write that cannot be stopped, with the UI lying about device
state. The only escape is unplugging mid-write — which `core/src/lib.rs:57-67` and the
README both document as what wedges a Kindle into answering reads while refusing writes.

---

## The root cause: five sites answer "is Fill enabled"

| Site | Expression | `busy` | `!device` | `writable` | `blockedByForeign` |
|---|---|:-:|:-:|:-:|:-:|
| `setBusy` :327 | `on \|\| !device \|\| blockedByForeign()` | ✅ | ✅ | ❌ | ✅ |
| `renderDevice` :435 | `!d.writable \|\| blockedByForeign()` | ❌ | n/a | ✅ | ✅ |
| `overwrite.onchange` :558 | `!device \|\| !device.writable \|\| blockedByForeign()` | ❌ | ✅ | ✅ | ✅ |
| `refresh` :466 | `true` | — | — | — | — |
| `showNoDevice` :455 | `true` | — | — | — | — |

`#clean` has the same split: `setBusy:328` says `on || !device`, `renderDevice:436` says
`d.filler_files === 0`. That disagreement is why Clean flickers enabled after every
operation.

---

## What to do

### 1. Frontend — one `updateControls()` owner

Replace every direct `.disabled` assignment in the module with a single function derived
from one state object. Suggested shape:

```js
// The one place that decides what is clickable. Every transition calls this and
// nothing else touches .disabled — five sites used to answer this question and three
// of them disagreed, which is how Fill became clickable during a fill.
function updateControls() {
  const usable   = !!device && device.writable;
  const blocked  = blockedByForeign();
  const locked   = busy || detecting;

  $('fill').disabled       = locked || !usable || blocked;
  $('clean').disabled      = locked || !device || device.filler_files === 0;
  $('refresh').disabled    = locked;
  $('stop').disabled       = !busy;
  $('low').disabled        = locked;
  $('high').disabled       = locked;
  $('dirname').disabled    = locked;
  $('rename').disabled     = locked;
  $('delupdates').disabled = locked || !device || device.updates.length === 0;
  $('overwrite').disabled  = locked;          // <- the control that was forgotten
}
```

Then: `setBusy(on)` becomes `{ busy = on; updateControls(); }`; `renderDevice` ends with
`updateControls()` instead of its two direct assignments; `overwrite.onchange` becomes
`updateControls()`; `refresh()` sets `detecting = true; updateControls()` at the top and
`detecting = false; updateControls()` in its `finally`; `showNoDevice` sets `device = null`
then calls `updateControls()`.

Watch two things while doing this:
- `refresh()` currently disables `#fill` **synchronously before its first `await`**. That
  ordering is load-bearing — it is what stops a click landing during a folder rename (this
  was investigated and confirmed safe; see appendix C of the report). Preserve it: call
  `updateControls()` before the `await invoke('detect', …)`, not after.
- `$('stop')` must stay driven by `busy` alone, so Stop remains live for the whole fill.

### 2. Backend — refuse concurrent operations

`AppState` currently holds only a cancel token. Add an in-flight guard so a second command
cannot race for the device even if the frontend is driven into a bad state:

```rust
#[derive(Default)]
struct AppState {
    /// Held for the duration of any device operation. The device is opened per
    /// operation, so two commands running at once means two `open_device()` calls and
    /// an ExclusiveAccess failure that strands the first — see wave-1 audit note.
    busy: Mutex<bool>,
    cancel: Mutex<Option<CancelToken>>,
}
```

Take the guard at the top of `start_fill`, `start_clean`, `delete_updates` and `detect`,
returning a clear `Err("Another operation is already running.".into())` rather than
racing. Release it on **every** exit path — an RAII guard struct with a `Drop` impl is the
way to do that without repeating the release before each `?`.

**The claim and the take must be one atomic operation.** A plain `Mutex<bool>` that you
read, drop, then set is a test-and-set race: two commands can both observe `false` before
either writes `true`, and an RAII `Drop` doesn't fix that — it's the *acquire* that has to
be indivisible. This matters because a `std::sync::MutexGuard` cannot be held across an
`await` in a Tauri command (the future must stay `Send`, which is why the existing code at
`main.rs:429-433` scopes its guard with a comment saying so).

Two shapes that are actually correct — pick either:

```rust
// (a) AtomicBool, claimed in one indivisible step.
struct OpGuard<'a>(&'a AtomicBool);
impl Drop for OpGuard<'_> {
    fn drop(&mut self) { self.0.store(false, Ordering::Release); }
}
fn claim(flag: &AtomicBool) -> Option<OpGuard<'_>> {
    flag.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .ok()
        .map(|_| OpGuard(flag))
}
```

```rust
// (b) tokio::sync::Mutex — its OwnedMutexGuard IS Send, so it can be held across the
// awaits that make up the operation. `try_lock_owned()` claims-or-fails atomically.
let _op = state.busy.clone().try_lock_owned()
    .map_err(|_| "Another operation is already running.".to_string())?;
```

(b) is fewer moving parts if you're willing to add `tokio` to the app crate's dependencies
— it's already a workspace dependency, so this is a manifest line, not a new package.
(a) keeps the dependency set unchanged. Do **not** hand-roll the flag pattern in prose
form; the race it introduces is the bug this wave is closing.

### 3. Backend — stop clobbering the cancel token

- In `start_fill`, publish the token **after** `open_device()` succeeds, so a failed open
  leaves no stale token (F9).
- Clear it only if the stored token is the one this invocation published — compare
  identity, don't blindly `= None`.
- In `start_clean` (`main.rs:493`), **delete** the unconditional
  `*state.cancel.lock().unwrap() = None;`. `start_clean` never sets a token; clearing one
  it doesn't own is how a running fill loses its Stop button.

---

## Definition of done

1. `cargo test --workspace` — 58 pass (no regression).
2. `cargo clippy --all-targets -- -D warnings` — clean.
3. `cargo fmt --all --check` — clean (the tree currently fails this; wave 4 covers it
   repo-wide, but leave anything you touch formatted).
4. **The harness assertion, proven red before your fix and green after.** Reproduce it:
   copy `ui/index.html` to a scratch file, inject a stub before `</head>` that makes
   `window.__TAURI__.core.invoke` return a fake `DeviceSnapshot` with
   `foreign: [{name:"mybook.azw3", bytes:4096, human:"4.00 KB"}]` for `detect`, `true` for
   `device_present`, and `new Promise(() => {})` for `start_fill`. Serve it, then run:

   ```js
   $('overwrite').click();  $('fill').click();  await sleep(200);
   $('overwrite').click();  $('overwrite').click();  await sleep(50);
   // BEFORE the fix: false  (Fill clickable mid-fill)
   // AFTER  the fix: true
   document.getElementById('fill').disabled
   ```

   Also assert `document.getElementById('overwrite').disabled === true` during the fill.
5. A second `start_fill` invoked while one is running returns the "already running" error
   **without** touching `state.cancel`.
6. Manual check on hardware if a Kindle is available: start a fill, confirm Stop still
   responds in ~1 s (the README claims "about a second"; don't regress it).

---

## Constraints

- **Repo conventions:** comments explain *why*, in prose, at the decision site — match the
  existing density and voice. Do not add a comment that restates the code.
- **Single-Path Principle:** `updateControls()` is a new shared owner. If the project
  later grows a Single Owners registry, register it. Do not leave a second site assigning
  `.disabled` — the point of this wave is that there is exactly one.
- `plan.rs` must stay I/O-free. This wave shouldn't touch it.
- No dependency changes.
- Do not "fix" the Overwrite-confirmation-across-folders concern — it was investigated and
  **refuted** (appendix C of the report); the blur-before-click ordering already closes it.
  The related structural gap is F3, handled in wave 3.
