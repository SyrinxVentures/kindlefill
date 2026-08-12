# Fix Wave 4 — What a stranger notices in the first five minutes

**Base commit:** `0885e46` (apply after waves 1–3, or independently — nothing here conflicts)
**Source:** `audits/vibe-audit-2026-08-11.md` findings F7, F8, F10, F11, F12, F13, F14, F15
**Severity:** all Low. Each is small; together they're the difference between "careful" and
"nearly careful".

Order below is roughly cheapest-first. Each item is independent — take them in any order,
or drop any with a stated reason.

---

## F11 · `cargo fmt --all --check` fails

The tree is clippy-clean but not rustfmt-clean (`core/src/lib.rs:23-28` has a stray
mid-list break; `core/src/engine.rs:75-77` and a few others exceed width).

```bash
cargo fmt --all
```

Review the diff before committing — if rustfmt collapses a deliberate line break that was
carrying meaning, add `#[rustfmt::skip]` at that site with a one-line reason rather than
leaving the repo unformattable. Land this **before** wave 3's CI, which gates on it.

---

## F12 · `Removal` / `next_removal` aren't re-exported from `lib.rs`

`core/src/lib.rs:29`:

```rust
pub use plan::{next_step, Step, Window, WindowError, GIB, KIB, MIB};
```

`plan.rs` has seven public items; these two are the only ones not re-exported. They're
reachable as `kindlefill_core::plan::next_removal`, so nothing breaks — but a consumer
following the crate's own convention won't find the half of the convergence API that
handles the below-window case, which the README describes as a headline feature. Almost
certainly an oversight from `0885e46`.

```rust
pub use plan::{next_removal, next_step, Removal, Step, Window, WindowError, GIB, KIB, MIB};
```

---

## F7 · `probe` is documented as "changes nothing"; it SIGKILLs a system daemon

`README.md:172`:

```
cargo run -p kindlefill-cli -- probe     # what do we see? changes nothing
```

`probe()` (`cli/src/main.rs:117`) calls `ptpcamerad::probe_privileges()`, which answers
"by trying" — `core/src/ptpcamerad.rs:57-65`:

```rust
pub fn probe_privileges() -> PrivilegeCheck {
    if !is_running() { return PrivilegeCheck::NotRunning; }
    match kill_once() { ... }          // pkill -9 -x ptpcamerad
```

So `probe` sends `SIGKILL` to Apple's camera daemon, and `open()` then starts a `Tamer`
that keeps killing it every 500 ms. The **design** is fine and well argued
(`ptpcamerad.rs:1-13`: scoped rather than permanent). The sentence is the problem — someone
runs `probe` on the strength of "changes nothing".

Fix the README, not the code. Something like: *"probe — what do we see? Doesn't touch
anything on the device. It does kill `ptpcamerad` once to find out whether it can, which
is the whole question; launchd restarts it within a second."* This makes the tool read as
more careful, not less.

---

## F15 · Windows support rests on an untested claim

`README.md:8` says "the stack is portable but untested elsewhere" — which is honest. The
code carries real Windows affordances (`app/src/main.rs:12` `windows_subsystem`, the
non-macOS `ptpcamerad` no-op at `ptpcamerad.rs:107-132`, `icon.ico` in the bundle set), so
a reader may reasonably assume someone tried.

Pre-empt the issue tracker with one explicit line: Windows and Linux have **never been
compiled or run**; the unknown is `mtp-rs`'s WPD/libusb backend, not this app's code; PRs
reporting results are welcome. Cheaper than answering it three times in issues.

---

## F14 · `parse_size` silently coerces negatives and non-numbers to 0

`cli/src/main.rs:68-72`:

```rust
digits.parse::<f64>()
    .map_err(|_| format!("not a size: {raw}"))
    .map(|n| (n * scale as f64) as u64)
```

Rust's float→int cast saturates, so `--low -5MB` parses cleanly to `0`, and `--low nanMB`
also yields `0` (NaN casts to 0). `Window::new(0, 90MB)` is valid, so the CLI accepts it
and fills toward a 45 MB aim without complaint. Not dangerous — the window still bounds
the fill — but the user asked for something the tool didn't do and wasn't told.

Reject before the cast:

```rust
.and_then(|n| {
    // `as u64` saturates, so a negative or NaN would silently become 0 — a window the
    // user never asked for. Refuse rather than reinterpret.
    if !n.is_finite() || n < 0.0 { return Err(format!("not a size: {raw}")); }
    Ok((n * scale as f64) as u64)
})
```

Add a unit test covering `-5MB`, `nanMB`, `infGB` and a valid `50MB`.

---

## F10 · MiB is labelled "MB" throughout

`core/src/lib.rs:80-88` divides by `GIB`/`MIB`/`KIB` and labels the results GB/MB/KB;
`parse_size` maps both `mb` and `mib` to 1024²; `index.html:244` sets `MB = 1024 * 1024`.

**Internally consistent** — every surface agrees, so no arithmetic bug follows, and the
deliberate `humanBytes`/`human_bytes` duplication can't disagree. It's simply non-SI, and
the README quotes capacities ("25.46 GB") a reader will compare against Amazon's spec
sheet, which uses SI GB.

Pick one and be consistent:
- **(a)** Relabel to GiB/MiB/KiB — most correct, touches `human_bytes`, its test, the UI
  labels, and README figures.
- **(b)** Keep the labels, add one README line: "Sizes are binary units — 1 GB here means
  1 GiB (1024³ bytes), matching how the Kindle reports its own storage."

(b) is the smaller change and arguably the more useful one if the device itself reports
binary units. Verify which the Kindle actually does before choosing — `probe` prints the
raw `total_capacity`, so compare it against the advertised capacity.

---

## F8 · Every `Event::Deleted` is charged to the filler tally

`app/src/main.rs:263`:

```rust
Event::Deleted { bytes, .. } => device_update(app, None, -1, -(*bytes as i64)),
```

Three producers emit `Deleted`: `clean` and the fill's removal branch (genuinely filler),
`purge_fill_dir` (may be foreign content), and `delete_staged_updates` (a firmware image,
never filler). All three decrement `device.filler_files` / `filler_bytes` in the header, so
deleting a 1.5 GB staged update from a device holding 10 filler files momentarily reads
"9 files, 8.5 GB".

Self-corrects — the `Math.max(0, …)` clamps at `index.html:367-368` prevent nonsense and
every caller's `finally` runs `refresh()` within a second. The structural point is that the
event carries no kind, so the consumer has to guess.

Add a discriminant:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletedKind { Filler, Foreign, Update }

Event::Deleted { name: String, bytes: u64, kind: DeletedKind },
```

and have `forward` apply the delta only for `Filler`. Update the CLI's match arms
(`cli/src/main.rs:338`, `:383`, `:415`) — they ignore the field, so they only need the
pattern widened.

---

## F13 · Progress bar has no accessible name; the log live-region re-announces wholesale

`index.html:233`:

```html
<div class="bar" role="progressbar" aria-valuemin="0" aria-valuemax="100" aria-valuenow="0" id="bar">
```

No `aria-label`/`aria-labelledby`, so a screen reader announces an unnamed progressbar.
And `log()` (`:256-261`) does `el.textContent += …` on a container carrying
`role="log" aria-live="polite"`, replacing the whole text node — assistive tech may re-read
the entire log on each append rather than just the new line.

Worth fixing because the CSS here is otherwise conspicuously careful about accessibility:
the `--accent`/`--accent-on` split with documented contrast ratios, the
`prefers-reduced-motion` block, `.sr-only` on the heading. These two are out of character.

```html
<div class="bar" role="progressbar" aria-label="Fill progress"
     aria-valuemin="0" aria-valuemax="100" aria-valuenow="0" id="bar">
```

Set `aria-valuetext` to the human string in the progress listener (`:483-490`) so it
announces "37.4% — 9.31 GB of 24.88 GB" rather than a bare number. And append child nodes
in `log()` instead of rewriting `textContent`:

```js
function log(line) {
  const el = $('log');
  const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 20;
  // Append a node rather than rewriting textContent: the whole box is an aria-live
  // region, and replacing its text makes a screen reader re-read every prior line.
  el.appendChild(document.createTextNode((el.childNodes.length ? '\n' : '') + line));
  if (atBottom) el.scrollTop = el.scrollHeight;
}
```

Check `#log:empty::before` (the "Activity from the device appears here." placeholder at
`:167`) still disappears on the first append — `:empty` matches only when there are no
child nodes at all, so appending a text node should still clear it. Verify in the browser.

---

## Definition of done (whole wave)

- `cargo test --workspace` passes, with new tests for F14.
- `cargo clippy --all-targets -- -D warnings` clean.
- `cargo fmt --all --check` clean (F11 lands first).
- README changes for F7, F10(b), F15 read in the repo's existing voice — prose that
  explains why, not bullet-point release notes.
- Anything you decide **not** to do is recorded in the commit message with the reason.
  Dropping an item on judgement is fine; dropping it silently is what this audit exists to
  catch.
