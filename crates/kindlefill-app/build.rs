use std::process::Command;

fn main() {
    stamp_build();
    tauri_build::build();
}

/// Put the commit this was built from into the binary, for the window to show.
///
/// The version alone cannot answer the question people actually ask of it. `0.2.0` is
/// the same string before and after a day of fixes, so "is the fix in the build I am
/// looking at?" is unanswerable from the version — and that question comes up on every
/// round trip between a change and a device on the cable. The commit distinguishes two
/// builds of one version; the `-modified` suffix distinguishes a build from uncommitted
/// work, which is what a build under test almost always is.
///
/// Degrades rather than fails. A source tarball with no `.git`, or a machine without
/// `git` on `PATH`, still builds — it just reports `unknown` and says nothing false.
fn stamp_build() {
    let commit = run_git(&["rev-parse", "--short=8", "HEAD"]).unwrap_or_else(|| "unknown".into());
    // Empty output means a clean tree. A failed call means we cannot tell, and claiming
    // "clean" on no evidence is the one answer worth avoiding — an unmarked build that
    // silently carried uncommitted changes is exactly the confusion this exists to stop.
    let suffix = match run_git(&["status", "--porcelain"]) {
        Some(out) if out.is_empty() => "",
        Some(_) => "-modified",
        None => "-unverified",
    };
    println!("cargo:rustc-env=KINDLEFILL_BUILD={commit}{suffix}");

    // Without these the stamp freezes at whatever the first build saw. `HEAD` covers
    // commits and branch switches, `index` covers staging — between them, the common
    // ways the answer changes without this crate's own sources changing.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
}

fn run_git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}
