# Fix Wave 3 — A backstop for the destructive path, and something that can go red

**Base commit:** `0885e46` (apply after waves 1–2) · **Source:** `audits/vibe-audit-2026-08-11.md` findings F3, F6
**Severity:** Medium — but this is the wave that makes the repo safe to take PRs against.

---

## F3 — `purge_fill_dir` empties any root folder its caller names; nothing server-side checks the user saw it

### Read first
- `crates/kindlefill-app/src/main.rs:416-450` (`start_fill`)
- `crates/kindlefill-core/src/engine.rs:365-421` (`purge_fill_dir`, `purge_children`)
- `crates/kindlefill-core/src/engine.rs:218-253` (`delete_staged_updates` — the pattern to copy)
- `crates/kindlefill-app/ui/index.html:407-422` (where the confirmation is reset)

### The gap

`start_fill` takes `dir_name` and `overwrite` as two independent parameters from the
webview and never relates them — `main.rs:417-450`:

```rust
async fn start_fill(app, state, low: u64, high: u64, dir_name: String, overwrite: bool)
    -> Result<String, String> {
    // ...
    if overwrite {
        let removed = engine::purge_fill_dir(storage, &dir_name, move |ev| forward(&handle, ev))
```

`purge_fill_dir` then recursively deletes everything under that folder, including content
this tool did not write. Its own guarantees hold — scoped to one named root folder,
can't be pointed at the root, `validate_dir_name` rejects separators. What it does **not**
verify is that the caller ever showed the user what is in there.

The entire binding between "the user confirmed *these files*" and "this folder gets
emptied" is one line of frontend JS — `index.html:419`:

```js
// A fresh listing is a fresh decision: never carry a tick across a re-detect, or a
// confirmation given for one folder's contents would apply to another's.
$('overwrite').checked = false;
```

That line is correct and always runs (verified — it fires on both branches of the
`if (foreign)`). But it is the **only** thing standing between a confirmation and a
deletion, and it lives in ~420 lines of untested JS whose state machine wave 1 showed can
be driven into unmodelled states.

Contrast `delete_staged_updates`, which is right by construction — `engine.rs:236-240`:

```rust
let doomed: Vec<ObjectInfo> = list_staged_updates(storage).await?
    .into_iter()
    .filter(|o| names.iter().any(|n| n == &o.filename))
    .collect();
```

The caller's list is intersected with what is *actually* a root-level `update_*.bin`.
Three gates, all server-side. `purge_fill_dir` has one gate, client-side.

**No live exploit was found** — no reachable path purges a folder the user hadn't
confirmed. This is defense-in-depth, filed at medium on that basis.

### What to do

Make the confirmation reference what it confirms.

1. Have `detect` include, in `DeviceSnapshot`, a digest of the foreign set it displayed —
   e.g. a stable hash over the sorted `(filename, size)` pairs for `dir_name`, plus
   `dir_name` itself. Add it as `overwrite_token: Option<String>` (None when there's
   nothing foreign, so no confirmation is needed).

2. Have the frontend echo it back: `start_fill` takes
   `overwrite: Option<String>` instead of `bool` — `null` for "don't overwrite", the token
   for "the user confirmed the set identified by this token".

3. Server-side, before purging: recompute the digest from the device **as it is now** and
   refuse if it doesn't match:

   ```rust
   // The name and the confirmation travel together, so a confirmation given for one
   // folder's contents cannot be spent on another's — and content that appeared
   // between the listing and the purge invalidates the confirmation rather than
   // being silently swept up with it.
   if overwrite_token != current_digest {
       return Err("The folder's contents changed since you confirmed them. \
                   Press Refresh and confirm again.".into());
   }
   ```

This also closes the (currently theoretical) window where content lands in the folder
between the user's confirmation and the purge.

Keep the CLI's `--overwrite` flag as-is: on the CLI the user types the folder name and the
flag in the same command, so the confirmation is already bound to the target by
construction. Note that reasoning in a comment so the asymmetry doesn't read as an
oversight.

### Done when
- A `tests/virtual_device.rs` case: compute a token, add a file to the folder, then assert
  the purge is refused with the stale token and accepted with a fresh one.
- `overwriting_empties_the_folder_and_nothing_outside_it` and
  `overwriting_a_folder_that_does_not_exist_is_a_no_op` still pass.
- Manual: the app's Overwrite flow still works end to end.

---

## F6 — No CI, no frontend tests, no guardrail on the single-owner invariant

### Three limbs

**1. No CI.** No `.github/`. "58 tests pass, clippy clean" is a claim with nothing that can
go red. Add `.github/workflows/ci.yml`:

```yaml
name: CI
on: [push, pull_request]
jobs:
  check:
    runs-on: macos-latest      # the target platform; see note below
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with: { components: clippy, rustfmt }
      - uses: Swatinem/rust-cache@v2
      - run: cargo fmt --all --check
      - run: cargo clippy --all-targets -- -D warnings
      - run: cargo test --workspace
```

`cargo test` needs no hardware, so this genuinely gates. Two notes: run `cargo fmt --all
--check` **after** wave 4 lands (it currently fails on the existing tree). And
`kindlefill-app` pulls in Tauri's system deps — if the app crate makes CI slow or
fragile, gate `-p kindlefill-core -p kindlefill-cli` and say so in the workflow comment
rather than silently narrowing coverage.

**2. No guardrail on `filler_sequence`.** `engine.rs:255-267` is emphatic that this is the
single owner of "is this filler we wrote":

> Everything destructive keys off this one answer: `clean`'s delete set and the resume
> sequence both come from here. They used to decide separately and disagreed […] One
> reading, one answer.

The tests at `:697-718` pin its **behaviour** well. Nothing detects **copy N+1** — a
future `strip_prefix(FILL_PREFIX)` elsewhere reintroduces the exact `fill_notes.bin` bug
and the suite stays green. Add a grep-based guardrail:

```rust
/// The `fill_notes.bin` bug was two functions answering "is this ours" differently, and
/// the permissive half was the half that deletes. `filler_sequence` is now the single
/// owner. This fails the build if a second site starts reading the name apart, which is
/// how that bug would come back.
#[test]
fn only_one_place_decides_whether_a_name_is_filler() {
    let src = std::fs::read_to_string("src/engine.rs").unwrap();
    let sites = src.matches("strip_prefix(FILL_PREFIX)").count();
    assert_eq!(sites, 1, "a second site is parsing filler names; extend filler_sequence instead");
}
```

**Prove it red before trusting it:** paste a second `strip_prefix(FILL_PREFIX)` into
`engine.rs`, confirm the test fails, then revert. A guardrail that has never failed is a
hypothesis. Note in the commit message that you did this. If the audit's Single Owners
convention is adopted repo-wide, register `filler_sequence` and `updateControls` (wave 1)
there.

**3. Zero frontend tests.** ~420 lines of JS carrying findings F1, F5, F8 and F13. F1
needed a 60-line harness to demonstrate and would have been caught by any state-machine
test.

Add a minimal harness under `crates/kindlefill-app/ui/tests/` — no framework, no build
step, matching the frontend's existing philosophy. It needs: a stub for
`window.__TAURI__.core.invoke` / `.event.listen`, a fake `DeviceSnapshot` builder, and
assertions over `.disabled` after scripted interactions. The three cases worth having on
day one:

- Fill is disabled while a fill is running, **including** after toggling `#overwrite`
  (the wave-1 regression test).
- Fill is disabled when `writable === false`, and stays disabled after `setBusy(false)`.
- Clean is disabled when `filler_files === 0`, and stays disabled after `setBusy(false)`.

If you want it in CI, `node --test` with `jsdom`, or a headless-Chrome step, both work.
If you'd rather keep it a manual harness for now, that's a legitimate call — but write the
README line saying so, so "the frontend is tested" is never implied.

### Done when
- CI runs on push and fails when you deliberately break a test (verify once, then revert).
- The `filler_sequence` guardrail has been **proven red** and reverted.
- Frontend harness exists and its wave-1 case passes; if it isn't wired into CI, that's
  stated in the README.

---

## Constraints

- No dependency changes to the shipped crates. Test-only dev-dependencies are fine.
- The guardrail test belongs in `crates/kindlefill-core` (it reads `src/engine.rs`
  relative to the crate root — verify the path resolves under `cargo test` from both the
  workspace root and the crate directory).
- Don't weaken any existing test to make CI green. If something fails under
  `-D warnings` that didn't before, fix the code, not the gate.
