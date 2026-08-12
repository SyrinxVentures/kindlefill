---
target: crates/kindlefill-app/ui/index.html
total_score: 22
max_score: 40
na_heuristics: 
p0_count: 2
p1_count: 3
timestamp: 2026-08-12T16-15-39Z
slug: crates-kindlefill-app-ui-index-html
---
Method: dual-agent (A: design review · B: detector + browser evidence), synthesized with independent verification of every P0 claim.

Target: `crates/kindlefill-app/ui/index.html` — the complete KindleFill frontend (693 lines: CSS + markup + inline ES module). Operate-mode surface. Shipped window `620×800`, floor `520×640` (`tauri.conf.json`).

## Design Health Score

| # | Heuristic | Score | Key Issue |
|---|-----------|-------|-----------|
| 1 | Visibility of System Status | 2 | The progress bar renders at **0px height** at the default window with all callouts, and at the minimum window with *any single* callout. No terminal state: `#fill`'s `finally` never resets the bar or meta row, so a finished or stopped fill keeps advertising a rate and ETA. |
| 2 | Match System / Real World | 3 | Copy is domain-true and excellent ("steering to", "top back up", "not present yet"). But `Fill` is the name of a button that sometimes deletes, and nothing explains why the window is 50–90 MB. |
| 3 | User Control and Freedom | 3 | Stop is real, sub-second, resumable; Esc reverts the folder field; the overwrite tick resets on every re-detect. No undo after Overwrite or Delete; the armed two-click delete can only be cancelled by waiting out 8s. |
| 4 | Consistency and Standards | 2 | Two destructive paths, two opposite idioms — and the irreversible one is the quiet one. `.names` lacks the `word-break` `#log` has. `validate()` is enforced in the click guard but not in `updateControls()`. Button caps mix Title and sentence case. |
| 5 | Error Prevention | 2 | Genuinely strong server-side gates (overwrite token, three-gate update delete, single-operation guard) undercut by three frontend holes: dead Fill on an invalid range, no capacity ceiling, Clean live on read-only storage. |
| 6 | Recognition Rather Than Recall | 3 | Folder named not counted; files listed by name; the overwrite label restates folder and consequence; live rows track the fill. The folder field hides behind a disclosure. |
| 7 | Flexibility and Efficiency | 1 | Zero accelerators. No autofocus, Return does nothing, no Cmd-. for Stop, no menu items. Tab order reaches Fill fifth, behind Refresh and Change folder. |
| 8 | Aesthetic and Minimalist Design | 3 | The hairline-divided plane with callouts as the only boxes is genuinely well-judged and consistently applied. But the log is 37% of the window and usually empty, and the bar renders permanently at 0% when idle. |
| 9 | Error Recovery | 2 | Recovery *copy* is good; its *delivery* is not. Every error, including a failure 15 minutes into a fill, is an undifferentiated 12px grey monospace line in a live region shared with routine progress. Read-only offers no remedy despite the README documenting one. |
| 10 | Help and Documentation | 1 | The app never states what it is for. No mention of Airplane Mode, no explanation of the default window, of "read-only", or of what a staged firmware image means for the task. |
| **Total** | | **22/40** | **Acceptable — significant improvements needed** |

No heuristic marked n/a. This is an Operate surface; 7 and 10 are applicable and scoring low, not exempt.

## Design Specificity Verdict

**Authored copy on a generic chassis.** The writing could not have been produced for anything else — the resume-is-not-a-conflict distinction, "Fill will remove filler to free space, then top back up", `fill_disk (not present yet)`, the sentence naming a stray folder and its exact consequence. This is domain-authored language most shipping utilities never reach.

The composition is a stock utility shell: right-aligned key/value table, two number fields, a horizontal button row, an 8px bar, a monospace log. Swap five strings and it is a disk imager or a firmware flasher.

The concrete miss: **the product's central idea is free space converging into a narrow window, and nothing on screen ever draws it.** The only bar in the app measures bytes written, not position relative to the target. In the below-window state, `Free now 40.00 MB` against a 50–90 MB target renders identically to every other row. Five numbers standing in one arithmetic relationship are presented as five unrelated rows plus a 12px sentence.

**Deterministic scan**: 1 finding, `layout-transition` at `index.html:153` (`transition: width` on `.bar > i`). **False positive** — the element is childless inside a fixed-height `overflow:hidden` track, so there is nothing to reflow, and against the backend's 100ms progress cadence the easing closes 93.8% of the gap, leaving ~7ms of staleness. The detector caught none of the three real defects below.

## What's Working

1. **Callouts as the only boxes, each carrying its own action.** The content plane is unbroken hairlines, so a bordered amber well reads as genuinely exceptional rather than as one more card. The destructive button never gets separated from the only text that says what it deletes. A real principle, applied consistently.
2. **Copy that anticipates the wrong mental model and corrects it before it forms.** "Fill will remove filler to free space, then top back up" pre-empts the exact surprise of a button named Fill starting to delete.
3. **Verified-correct accessibility foundations.** All text contrast passes both themes (lowest enabled 5.64:1 light / 6.73:1 dark); focus indicators pass at 5.42:1 / 6.09:1; all 7 reachable controls show `:focus-visible` with a closed, logical tab cycle; reduced-motion coverage is complete (zero animations, all 11 transitions covered); the progressbar announces `aria-valuetext` "16.1% — 4.00 GB of 24.89 GB" rather than a bare number; zero console errors and zero external requests in all 10 states.

## Priority Issues

### [P0] The progress bar renders at zero height during the operation it exists for

`.bar` is a flex item of the column-flex `.block.grow` with default `flex-shrink: 1`. `#log` has `min-height: 60px`; once it hits that floor, all remaining shrink is absorbed by the bar, which goes 8px → 2px — and with `box-sizing: border-box` and 1px borders, the inner `#barfill` goes to **0px**.

Measured independently (iframes at exact CSS sizes; `resize_window` below 768px triggers mobile emulation and a 980px fallback viewport, so it cannot measure this):

| window | state | `#barfill` height |
|---|---|---|
| 620×800 default | connected / updates | 6px — healthy |
| 620×800 default | all | **0px** |
| 520×640 minimum | connected | 6px — healthy |
| 520×640 minimum | **updates** | **0px** |
| 520×640 minimum | **foreign** | **0px** |
| 520×640 minimum | all | **0px** |

Not a contrived worst case: at the app's own permitted minimum window, a *single* callout — including a staged firmware update, which is routine on a Kindle — makes progress invisible for the entire multi-minute fill. `minHeight: 640` sits well below the ~571px where the collapse begins.

**Fix**: `.bar, .meta { flex: none; }` — verified to restore `#barfill` to 6px in all three collapsed cases. `.block` already carries `flex: none`; these two were the omission.

### [P0] `updateControls()` claims to be the single owner of enablement; three conditions bypass it

The function carries a comment written after a real bug: "The one place that decides what is clickable… nothing else in this module assigns `.disabled`." Three conditions never reach it.

- **Invalid range → dead click.** `updateControls()` has `blockedByForeign()` but not `validate()`; the click guard has both. With `low=50 / high=52` the warning fires, `#fill.disabled === false`, and clicking produces no log line, no bar movement, no error — nothing at all. Compounding it, `$(id).oninput = estimate` means range edits never call `updateControls()`, so adding the term alone would not fire on input. Both halves need fixing.
- **No capacity ceiling.** 500000 / 900000 MB on a 25 GB device yields *"674436 MB below the target — Fill will remove filler to free space, then top back up. Takes seconds."* with Fill armed. A reassuring false statement for an impossible request. Inputs have `min` but no `max`, and nothing compares the window against `device.total`.
- **Clean ignores `writable`.** `#fill` uses `usable = !!device && device.writable`; `#clean` omits it. On read-only storage Fill is greyed and **Remove Fill Folder & Fill Content is fully enabled**.

A visibly-enabled primary button that does nothing is the most damaging thing an Operate surface can do: cause and effect break, and there is no error to read because none was raised.

**Fix**: move all three into `updateControls()`; have `oninput` call it; add `high <= device.total` to `validate()` naming the device capacity. Then land a guardrail test so a fourth `.disabled =` outside the owner fails the build — the same shape as the existing `filler_sequence` guardrail.

### [P1] The 17-minute operation has no ending, and afterwards the UI still describes it as running

`$('fill').onclick`'s `finally` does `setBusy(false); await refresh();` and touches neither `#barfill` nor the meta row. `$('clean').onclick`'s `finally` resets all three. So after a fill completes, is stopped, or errors, the bar holds its last width and the meta row holds its last rate and ETA indefinitely — a stopped fill sits at "37.4% … · 10m 26s left" forever.

Success itself is one 12px grey monospace line appended to a box captioned "Activity from the device appears here." No banner, no dot change, no statement of the outcome the user actually came for.

Peak-end: the end of a 17-minute wait is the memory the user keeps, and it is currently indistinguishable from the middle.

**Fix**: extract one `resetProgress()` and call it from both handlers. Give the meta row a terminal state — "Done — 68.4 MB free, inside 50–90 MB" / "Stopped at 37.4% — 9.31 GB written, resumable" / "Failed — <reason>" — with a matching dot colour.

### [P1] The destructive confirmation is under-signalled, and the two destructive paths use opposite idioms

Ticking `#overwrite` changes nothing about `#fill`: measured after `.click()`, `disabled=false`, `background rgb(42,99,216)`, label "Fill" — byte-identical to the Fill that writes into an empty folder. Meanwhile `#delupdates`, deleting a file Amazon will re-download, turns red and relabels to "Click again to delete permanently". **The irreversible path gets the weaker treatment.**

The checkbox itself is 13×13 with a 21px-tall label hit area (3px under the 24px floor) and **no app-authored focus style** — the stylesheet's only focus rules are for `input[type=number]`, `input[type=text]` and `button:focus-visible`, none of which match a checkbox. It falls back to the UA default, which differs in WKWebView from Chrome.

`.names` computes `word-break: normal` while `#log` computes `break-word` — a Single-Path divergence between two 12px monospace surfaces that both display filenames. Not reachable by window resize (`minWidth: 520` keeps horizontal scroll at 0), but at 200% zoom a realistic firmware filename produces 133px of horizontal scroll.

**Fix**: when overwrite is ticked, swap `#fill` to `.danger` and relabel it "Overwrite & Fill" — the button must state what it will do. Pick one destructive idiom and use it for both. Add `overflow-wrap: anywhere` to `.names` and a focus style for the checkbox.

### [P1] The app never says what it is for, and withholds remedies it already has

Nowhere does the UI state its purpose. A first-run user sees five rows of device facts and two number fields, with no statement of what filling a Kindle accomplishes, why 50–90, or that Airplane Mode is the companion action the README calls out. The empty state is identical to the populated one except two values.

On read-only storage the app renders a red dot and a parenthetical, no remedy — while the README documents it precisely ("Unplug and replug, or restart the Kindle") and the estimate line cheerfully continues to promise "About 24.90 GB to write — roughly 17 min".

With no device, the diagnosis is at the top in 14px and the remedy is at the bottom in 12px monospace inside a box captioned "Activity from the device appears here" — a different typeface, a different region, ~500px away.

With a staged update detected — directly relevant to the user's entire reason for being here — it renders in the *neutral* well, quieter than the foreign-content warning, with no sentence connecting the two facts.

**Fix**: one 14px line under the status block stating purpose and the default window's reasoning. Turn read-only into a callout carrying the remedy and blank the estimate. Move the no-device guidance into the status block. Promote the update callout to the warn surface and state what it means for the fill.

### [P2] Input borders fail non-text contrast

`--line-strong` as the input border computes **1.61:1** light / **1.77:1** dark against `--surface`, against a 3:1 requirement (WCAG 1.4.11). Decisive because the input's own fill (`--well`) is only 1.09:1 against the page in both themes — the border is the sole thing identifying the field, at roughly half the required contrast.

The same token on secondary buttons is contestable rather than failing, since each carries a visible text label. Container borders (`.callout`, `#log`, `.bar` track) are decoration and out of scope.

Also: the CSS's own declared ratios at line 13 are optimistic — "6.4:1 on surface" is actually 6.15, "6.1:1 on well" is actually 5.64. Both still pass, but the comment is not a reliable record.

**Fix**: darken `--line-strong` to clear 3:1 against `--surface` in both themes, or give inputs a dedicated border token.

## Persona Red Flags

**Riley (stress tester)** — the most exposed persona; every item verified, not hypothesised.
- `low=50 / high=52`: warning appears, Fill stays enabled, click produces **zero** observable change.
- `500000 / 900000`: "Takes seconds." with Fill armed, for an operation the hardware cannot perform.
- `0` in low: "Enter both bounds." — but both bounds *are* entered. The message names the wrong problem.
- Presses Stop mid-fill: the meta row keeps announcing "14m 38s left" indefinitely.
- Opens the app at 520×640 with a staged update present: the progress bar is invisible for the whole fill.
- Sets an already-in-window device: "Already inside the target window — nothing to do." and Fill remains enabled.

**Sam (accessibility-dependent)**
- **Zero landmarks**, one heading (`h1.sr-only`) across three unnamed `<section>`s. Navigating by heading or landmark yields nothing.
- The overwrite checkbox has no authored focus style and a 21px-tall hit area — the gate on permanent deletion is the least keyboard-legible control in the app.
- Errors append into the `role="log"` region with no distinguishing markup: a fill failing at minute 15 is announced at the same priority and in the same voice as "Wrote fill_0004.bin".
- `#rangewarn` (`role="alert"`) announces the range is invalid while Fill stays focusable and activatable — activating it does nothing and announces nothing.
- Focus on load is `BODY`; no autofocus, no default-button Return, no accelerators.
- The idle bar is a permanent `progressbar` with `aria-valuenow="0"` and no `aria-valuetext` — AT meets a 0% progress bar when nothing is running.

**Jordan (first-timer)**
- Opens the app and **nothing on screen says what it does.** No purpose statement, no explanation of the default window, no mention of Airplane Mode.
- The empty state is identical to the populated state.
- On read-only, gets a parenthetical and a greyed button, with the remedy sitting unused in the README.
- With no device, must find the fix at the bottom of the window, in monospace, in a box labelled "Activity from the device appears here" — which does not look like where instructions live.
- The estimate degrading to a bare `—` reads as a broken field, not "no estimate is possible yet".

## Minor Observations

- `Storage: Internal Storage` is the top row and never varies on a single-storage device; `Free now`, which drives everything, is third of five at identical weight.
- The idle bar renders permanently at 0% — reads as a stalled operation rather than "nothing running".
- The stale pre-flight estimate ("roughly 17 min") stays on screen *during* a fill next to the live "14m 38s left". Two time figures, one wrong.
- `#clean` is 241px wide against `#fill`'s 47px — the destructive tertiary action is 5.1× wider than the primary and the largest control in the app.
- `#estimate`, the sentence a 17-minute commitment rests on, is 12px against a 14px body.
- `Change folder…` stays enabled with no device; using it fires a detect that fails.
- The folder field opens *below* the foreign-content callout, so the callout's advice "Use a different folder name" is followed by a control revealed by a button rendered underneath it.
- Button capitalisation mixes Title Case (`Remove Fill Folder & Fill Content`) and sentence case (`Change folder…`).
- In dark mode `--well` (#16171a) is darker than `--surface` (#1d1f23), so neutral callouts nearly disappear into the plane — the same information carries visibly less signal in dark than in light.
- Disabled `.primary` at `opacity: .4` on dark `--accent` produces a lavender that pulls harder than the *enabled* outlined Stop beside it: during a fill the brightest thing in the button row is the dead control.
- The status dot is red for read-only while the adjacent text reads "Kindle **connected** (storage is read-only)".
- `fillList()` renders every item uncapped; a folder of 200 foreign files produces 200 `<li>`s and unbounded callout growth.
- `.callout .note` at 12px means "permanently deletes everything in it" is the smallest type in the window.

## Questions to Consider

1. **What if `Free now` were a picture instead of a number?** One bar — capacity, current free, target band — turns "40 MB against a 50–90 MB window" into something seen in 200ms rather than parsed across three rows and a 12px sentence. It would also be the first element in this app that no other utility could wear.
2. **The log is 37% of the window and the least designed thing in it.** If the 17-minute wait is the dominant experience, is a debug console the right thing to give it — with the real signal in an 8px hairline that sometimes measures zero?
3. **Two destructive paths, two opposite confirmation idioms, and the weaker one guards the irreversible act.** The codebase has a Single-Path principle for exactly this shape of divergence; it was applied to `updateControls()` and the filler-name matcher, and not to confirmation.
4. **The app knows the user's goal is blocking updates, and knows a firmware image is already on the device.** Why are those two facts on screen with no sentence connecting them?
