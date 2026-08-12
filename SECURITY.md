# Security

## Reporting

Use [GitHub's private vulnerability reporting](https://github.com/SyrinxVentures/kindlefill/security/advisories/new)
rather than a public issue. There is one maintainer and no service-level agreement — you
will get a human reply, not a fast one.

## What would actually count

This is a local desktop tool with no network access, no accounts, and no server. That
rules out most of what "security issue" usually means, and leaves a short list of things
that would genuinely matter:

- **Deleting something it shouldn't.** The only content this removes that it did not write
  is behind two explicit, named opt-ins: `--overwrite` / the Overwrite tick, and Delete
  staged update. A path that removes anything else — a book in the filler folder, a file
  outside the named folder, a root-level file that isn't an `update_*.bin` — is the most
  serious class of bug this project has.
- **Escaping the named folder.** `validate_dir_name` rejects path separators rather than
  sanitizing them, and `purge_fill_dir` resolves one folder at the storage root and cannot
  be pointed at the root itself. A way around either is a real finding.
- **The confirmation not meaning what it says.** Overwrite is bound to a digest of the
  exact file listing the user was shown; a way to spend a confirmation on contents nobody
  saw is a finding even though the frontend is in-process and could send anything.
- **Supply chain.** The dependency set is small and deliberate — `mtp-rs`, `tauri`,
  `clap`, `serde`, `tokio`, `anyhow`. Advisories against any of them are worth reporting.

## What is known and is not a vulnerability

- **`probe` sends SIGKILL to `ptpcamerad`**, and a running fill keeps killing it. This is
  deliberate and documented: Apple's camera daemon exclusively claims any MTP device on
  connect and can't talk to a Kindle, so nothing else can either until it lets go. launchd
  restarts it within a second, and the taming is scoped to the operation rather than
  disabling the daemon permanently.
- **The released DMG is ad-hoc signed, not notarized.** Downloads are quarantined and take
  an extra step to open, described in the README. Notarization needs a paid Apple
  Developer account.
- **The frontend can send any string to the backend.** It is in-process; there is no trust
  boundary there. Backend checks exist to catch a frontend driven into a bad state, not an
  adversary.
- **Filling storage to near-full is the entire point.** A Kindle left at 70 MB free is
  working as designed. `clean` gives the space back.
