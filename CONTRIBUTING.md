# Contributing

The most valuable contribution to this project is not code. It is a report from a Kindle
that isn't a Paperwhite Signature Edition on macOS, because that is the only device and
platform this has ever run on.

## The most useful thing you can do

Run it and say what happened. Especially:

- **A different Kindle model.** Does it enumerate? Does reported free space actually move
  after a write? `cargo run -p kindlefill-cli -- bench` answers both and prints the
  numbers to paste into an issue.
- **Windows or Linux.** Neither has ever been compiled. The unknown isn't this code, it's
  whether `mtp-rs`'s WPD and libusb backends behave like the macOS one.
- **The three paths never exercised on hardware** — `ptpcamerad` taming, deleting a staged
  firmware update, and Overwrite. They are covered by tests against a virtual device,
  which is not the same thing.

"It worked, here are the numbers" is as useful as a bug report. Both tell us something
the test suite can't.

## If you are sending code

```bash
cargo test --workspace
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

CI runs exactly these three on every push and pull request, so run them first. None of
them needs a Kindle: the engine is exercised against `mtp-rs`'s virtual device, which is
backed by a real directory and reports free space from real bytes.

The frontend has its own harness that CI does **not** run, because it needs a browser:

```bash
cd crates/kindlefill-app && python3 -m http.server 8000
# then open http://localhost:8000/ui-tests/controls.html
```

If you touch `ui/index.html`, run it.

## What the code is trying to be

Two conventions carry most of the weight here, and a change that breaks either will get
comments:

**Comments explain why, at the decision site.** Not what the line does — why it is shaped
that way, and usually what went wrong when it wasn't. If a comment would restate the
code, leave it out.

**One decision, one owner.** When two places answered "is this filename ours?" they
disagreed, and the permissive one was the one that deletes. There is now a single
`filler_sequence`, with a test that fails the build if a second site starts parsing those
names. The same applies to `updateControls()` in the frontend, which owns every
`.disabled` assignment, with a harness assertion enforcing it. Before adding a check,
look for the one that already exists and extend it.

Anything that deletes gets held to a higher standard than anything that writes. Filler is
cheap and replaceable; a book someone sideloaded in 2014 is not.

## Scope

This fills and unfills storage. It does not jailbreak anything, does not modify firmware,
and does not talk to Amazon's servers. Pull requests that change that are out of scope.

By contributing you agree your work is dual-licensed under MIT and Apache-2.0, matching
the project.
