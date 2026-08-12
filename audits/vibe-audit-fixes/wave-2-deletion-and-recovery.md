# Fix Wave 2 — Leave nothing behind, and say where it went

**Base commit:** `0885e46` (apply after wave 1) · **Source:** `audits/vibe-audit-2026-08-11.md` findings F2, F4
**Severity:** High (F2) / Medium (F4)

Both are sibling-divergence bugs of the kind already fixed once in this repo: a hazard was
handled correctly in one place and its twin was left alone.

---

## F2 — `bench` strands up to 656 MB with no recovery path, and the README calls it "self-cleaning"

### Read first
- `crates/kindlefill-cli/src/main.rs:166-223` (`bench`)
- `crates/kindlefill-core/src/engine.rs:598-614` (the twin that gets it right)
- `README.md:173`

### The bug

`cli/src/main.rs:166-223`:

```rust
async fn bench() -> Result<()> {
    // ...
    let dir = storage.create_folder(Some(ObjectHandle::ROOT), "kindlefill_bench").await
        .context("could not create a folder at the storage root")?;

    let mut written = Vec::new();
    for (label, size) in [("16MiB", 16*MIB), ("128MiB", 128*MIB), ("512MiB", 512*MIB)] {
        let name = format!("bench_{label}.bin");
        let before = { storage.refresh().await?; storage.info().free_space };
        // ...
        let handle = storage.upload(...).await
            .map_err(|e| anyhow::anyhow!("upload of {label} failed: {}", e.source))?;
        //                                                              ^^^^^^^^
        //                                          e.partial is dropped on the floor
        written.push(handle);
        storage.refresh().await?;
        // ...
    }

    print!("cleaning up... ");            // <- only reached on the fully-happy path
    for handle in written { storage.delete(handle).await?; }
    storage.delete(dir).await?;
```

Every step between `create_folder` and the cleanup loop is `?`. A failure at the 512 MiB
upload — the most likely, being the longest — exits with 16 MiB + 128 MiB committed, the
folder present, and possibly a partial third object.

**Nothing in this tool can remove any of it.** `clean` only deletes names satisfying
`filler_sequence` (`fill_NNNN.bin`), and `bench_512MiB.bin` doesn't match.
`find_filler_folders` only reports folders containing `filler_sequence` matches, so
`kindlefill_bench` is invisible to discovery too. `purge_fill_dir` would work, but requires
guessing the folder name and passing `--dir kindlefill_bench --overwrite`, documented
nowhere.

### The twin, 400 lines away

`engine.rs:598-604` handles exactly this hazard, with the reasoning written down:

```rust
if let Err(e) = upload {
    // A failed data phase can leave a partial object on the device.
    // The library deliberately doesn't auto-delete it; if we leaked it,
    // it would consume space that no `clean` run could find.
    if let Some(partial) = e.partial { let _ = storage.delete(partial).await; }
```

`bench` names the same error's `.source` in the same expression and ignores `.partial`.

### The doc claim — `README.md:173`

```
cargo run -p kindlefill-cli -- bench     # throughput + free-space sanity; self-cleaning
```

True on the happy path, false exactly where it matters.

### What to do

1. Restructure `bench` so cleanup runs on **every** exit. Move the measurement loop into an
   inner function (or an `async` block) returning `Result`, keep `written` and `dir` in the
   outer scope, then clean up before propagating:

   ```rust
   let result = run_bench_uploads(&mut storage, dir, &mut written).await;
   let cleanup = cleanup_bench(&mut storage, dir, &written).await;
   result?;                     // report the real failure first
   cleanup?;
   ```

2. Handle `e.partial` the way `fill_with_cancel` does — delete the partial object before
   propagating.

3. If cleanup itself fails, print a recovery instruction that actually works — and note
   that **right now there isn't one**. Check the CLI surface before writing the message:

   - `clean --dir kindlefill_bench` deletes only `fill_NNNN.bin` names, so it leaves
     `bench_512MiB.bin` exactly where it is.
   - `fill --dir kindlefill_bench --overwrite` *would* empty the folder — and then fill
     the device to the target window, which is emphatically not what someone recovering
     from a failed benchmark wants.

   So `purge_fill_dir` exists in the engine but no CLI verb reaches it standalone. The
   right fix is to expose one:

   ```rust
   /// Empty a folder at the storage root, including files this tool didn't write.
   /// Separate from `clean`, which only removes filler it can prove it wrote — this is
   /// the deliberate, named way to take a folder back, and the recovery path when
   /// `bench` fails to tidy up after itself.
   Purge {
       #[arg(long)]
       dir: String,
   },
   ```

   wired straight to `engine::purge_fill_dir`. Then the message can name something real:

   ```
   !! could not remove kindlefill_bench — up to 656 MB is still on the device.
      Remove it with:  kindlefill purge --dir kindlefill_bench
   ```

   Adding `purge` widens the CLI's destructive surface, so gate it the way the rest of
   the crate gates deletion: it takes an explicit `--dir`, has no default, and prints what
   it removed. Document it in the README's CLI section alongside the existing warning that
   `--overwrite` is the only other thing that deletes files this tool didn't write. If you
   would rather not add the verb, the alternative is a message telling the user to delete
   the folder with a file manager or Calibre — but do **not** ship a message naming a
   command that doesn't do what it says.

4. Soften `README.md:173` to something true on both paths — e.g. "cleans up after itself,
   and tells you exactly what to remove if it can't."

### Done when
- A forced upload failure (temporarily make the third upload return `Err`) leaves **no**
  `kindlefill_bench` folder on the virtual device, and the error still surfaces.
- Revert the forced failure; `bench` behaves as before on the happy path.
- Add a `tests/virtual_device.rs` case covering it if the virtual device can be made to
  fail an upload; if it can't, say so in the commit message rather than claiming coverage.

---

## F4 — Below the window, `fill` says "nothing to fill" without mentioning filler in a renamed folder

### Read first
- `crates/kindlefill-core/src/engine.rs:476-488` (the refusal), `:120-126` and `:134-138` (the message)
- `crates/kindlefill-app/src/main.rs:505-514` (the correct sibling)
- `crates/kindlefill-core/src/engine.rs:306-315` (why the folder name is unknown at launch)

### The bug

`fill` decides "does this device have filler I can give back?" by looking **only at the
configured folder** — `engine.rs:476-488`:

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

rendering as:

```
only 71303168 bytes free, already below the 78643200-byte target; nothing to fill
```

The **refusal** is defensible — `fill` manages its configured folder. The **message** is
not. Nothing persists the folder name between launches (`engine.rs:306-315` says so
explicitly), so the default is exactly what a relaunched app holds while gigabytes of
filler sit under the name the user chose. The user is told, truthfully, that there is
nothing to fill — and left to conclude the space is the device's, when this tool wrote it
and could give it back.

`start_clean` already answers this same question correctly — `main.rs:505-514`:

```rust
let others = engine::find_filler_folders(storage).await.map_err(|e| explain(&e))?;
match others.iter().find(|f| f.name != dir_name) {
    Some(f) => Ok(format!(
        "Nothing to remove in {dir_name}, but {} of filler is in {} — switch to \
         that folder and try again.", human_bytes(f.bytes), f.name)),
```

The "find filler wherever it is" fix (commit `97305ce`) landed on `detect` and `clean`.
`fill`'s refusal path is the twin it missed.

### What to do

Before returning `AlreadyBelowWindow`, call `find_filler_folders` and carry the result on
the error so **both** front ends render it without re-deciding:

```rust
AlreadyBelowWindow {
    free: u64,
    low: u64,
    /// Filler found under other root folders. Empty is meaningful: it means the space
    /// genuinely isn't ours. Carried on the error so neither front end has to re-ask
    /// the device — and so they can't word the answer differently.
    elsewhere: Vec<FillerFolder>,
},
```

and extend the `Display` impl to name the folder when the list is non-empty, matching
`start_clean`'s wording. Then verify the CLI (`cli/src/main.rs:386`, which surfaces the
error via `?`) and the app (`explain_fill` → `other.to_string()`) both show it.

### Done when
- A new `tests/virtual_device.rs` case: fill under `"somewhere_else"`, then call `fill`
  with the default `DIR` and a window above current free space, and assert the error names
  `somewhere_else`. Model it on the existing
  `cleaning_the_wrong_folder_reports_that_it_removed_nothing` test, which is the same
  shape for `clean`.
- The existing `below_the_window_with_nothing_of_ours_to_remove_is_still_an_error` test
  still passes — with an *empty* `elsewhere`, proving the two cases are distinguishable.

---

## Constraints (both fixes)

- `plan.rs` stays I/O-free — `find_filler_folders` is an `engine` concern, keep it there.
- Deletion stays bounded: this wave adds **no** new delete path. `bench`'s cleanup removes
  only handles it collected itself, which is the existing guarantee.
- Match the repo's comment voice: explain the hazard and why the code is shaped that way,
  in prose, at the site.
- `cargo test --workspace`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo fmt --all --check` on anything you touch.
