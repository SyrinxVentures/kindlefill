//! `kindlefill` — fill a Kindle's storage to a target free-space window, and undo it.
//!
//! Also the spike: `probe` and `bench` exist to answer the questions that can only be
//! answered with a real Kindle on the other end of the cable — does the device
//! enumerate, does reported free space actually move after a write, how fast is the
//! wire, and does taming `ptpcamerad` need root.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use kindlefill_core::{engine, human_bytes, is_exclusive_access, ptpcamerad, Event, Window, MIB};
use mtp_rs::{CancelToken, MtpDevice, NewObjectInfo, ObjectHandle, Storage};
use std::io::{self, IsTerminal};
use std::time::Instant;

#[derive(Parser)]
// `version` takes it from `CARGO_PKG_VERSION`, which the workspace supplies — the same
// single declaration the app window and the installers read, so `kindlefill --version`
// and the number on the KindleFill window can never disagree. A tool that reports what a
// device did needs to be able to say which build reported it; a bug report naming
// neither is a bug report about nothing.
#[command(
    name = "kindlefill",
    version,
    about = "Fill a Kindle's storage to block OTA updates"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Enumerate devices and report what we can see. Changes nothing.
    Probe,
    /// Measure write throughput and verify free space tracks writes. Cleans up after
    /// itself — and if it cannot, says exactly what to remove.
    Bench,
    /// Show current free space, any existing filler, and any staged firmware update.
    Status {
        #[arg(long, default_value = engine::DEFAULT_FILL_DIR)]
        dir: String,
    },
    /// Fill until free space lands inside the window.
    Fill {
        #[arg(long, default_value = "50MB", value_parser = parse_size)]
        low: u64,
        #[arg(long, default_value = "90MB", value_parser = parse_size)]
        high: u64,
        /// Folder to put filler in, at the storage root.
        #[arg(long, default_value = engine::DEFAULT_FILL_DIR)]
        dir: String,
        /// Empty that folder first, deleting anything already in it — including files
        /// this tool did not write. Off by default; nothing else deletes them.
        #[arg(long)]
        overwrite: bool,
    },
    /// Delete all filler this tool wrote.
    Clean {
        #[arg(long, default_value = engine::DEFAULT_FILL_DIR)]
        dir: String,
    },
    /// Empty a folder at the storage root, including files this tool didn't write.
    ///
    /// Separate from `clean`, which removes only filler it can prove it wrote. This is
    /// the deliberate, named way to take a folder back — and the recovery path when
    /// `bench` fails to tidy up after itself, whose `bench_*.bin` objects `clean`
    /// cannot match and `find_filler_folders` cannot see.
    ///
    /// `--dir` has no default on purpose: this is the one verb that deletes content
    /// the tool didn't write, and it should never be something a bare command run from
    /// muscle memory can do.
    Purge {
        #[arg(long)]
        dir: String,
    },
}

/// Accepts `50MB`, `90 MiB`, `2GB`, or a raw byte count.
fn parse_size(raw: &str) -> Result<u64, String> {
    let s = raw.trim().to_ascii_lowercase().replace(' ', "");
    let (digits, scale) = if let Some(d) = s.strip_suffix("gib").or_else(|| s.strip_suffix("gb")) {
        (d, 1024 * 1024 * 1024)
    } else if let Some(d) = s.strip_suffix("mib").or_else(|| s.strip_suffix("mb")) {
        (d, 1024 * 1024)
    } else if let Some(d) = s.strip_suffix("kib").or_else(|| s.strip_suffix("kb")) {
        (d, 1024)
    } else {
        (s.strip_suffix('b').unwrap_or(&s), 1)
    };
    digits
        .parse::<f64>()
        .map_err(|_| format!("not a size: {raw}"))
        .and_then(|n| {
            // `as u64` saturates, so a negative or a NaN would quietly become 0 — and
            // `Window::new(0, 90MB)` is perfectly valid, so `--low -5MB` was accepted
            // and filled toward a 45 MB aim without anyone being told. Refuse rather
            // than reinterpret: the user asked for something, and this isn't it.
            if !n.is_finite() || n < 0.0 {
                return Err(format!("not a size: {raw}"));
            }
            Ok((n * scale as f64) as u64)
        })
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Probe => probe().await,
        Command::Bench => bench().await,
        Command::Status { dir } => status(&dir).await,
        Command::Fill {
            low,
            high,
            dir,
            overwrite,
        } => fill(low, high, &dir, overwrite).await,
        Command::Clean { dir } => clean(&dir).await,
        Command::Purge { dir } => purge(&dir).await,
    }
}

/// Open the first Kindle-looking device, translating the macOS daemon collision into
/// something a user can act on.
async fn open() -> Result<MtpDevice> {
    let _tamer = ptpcamerad::Tamer::start();
    match MtpDevice::open_first().await {
        Ok(device) => Ok(device),
        Err(e) if is_exclusive_access(&e) => {
            bail!(
                "the device is held open by another process.\n\
                 On macOS that is almost always ptpcamerad (Apple's PTP camera daemon).\n\
                 Quit Android File Transfer and OpenMTP if running, then retry.\n\
                 Underlying error: {e}"
            )
        }
        Err(e) => Err(e).context("could not open an MTP device"),
    }
}

/// Kindles expose one writable storage; take the first that accepts writes.
async fn writable_storage(device: &MtpDevice) -> Result<Storage> {
    let storages = device.storages().await.context("could not list storages")?;
    if storages.is_empty() {
        bail!("device exposes no storages");
    }
    storages
        .into_iter()
        .find(|s| s.info().is_writable)
        .context("no writable storage on this device")
}

async fn probe() -> Result<()> {
    // A macOS-only concern gets a macOS-only section: printing "not running
    // (nothing to work around)" on an OS that never runs the daemon reads as a
    // check that passed, when no check happened.
    #[cfg(target_os = "macos")]
    {
        println!("== ptpcamerad ==");
        match ptpcamerad::probe_privileges() {
            ptpcamerad::PrivilegeCheck::NotRunning => {
                println!("  not running (nothing to work around)");
            }
            ptpcamerad::PrivilegeCheck::KillableUnprivileged => {
                println!("  running, and killable WITHOUT sudo");
                println!("  -> the GUI app will not need an admin prompt");
            }
            ptpcamerad::PrivilegeCheck::NeedsElevation { reason } => {
                println!("  running, and NOT killable as this user: {reason}");
                println!("  -> the GUI app will need an admin prompt or a privileged helper");
            }
        }
        println!();
    }

    println!("== devices ==");
    match MtpDevice::list_devices() {
        // "in the USB scan", not "found": on Windows this enumeration cannot see a
        // WPD-bound device at all, and a bare "none found" printed one line above a
        // storages section that then opens a Kindle reads as a contradiction.
        Ok(devices) if devices.is_empty() => println!("  none in the USB scan"),
        Ok(devices) => {
            for d in &devices {
                println!("  {}", d.display());
            }
        }
        Err(e) => println!("  USB enumeration failed: {e}"),
    }
    if cfg!(windows) {
        // The WPD device list is what `open_first` actually acts on for a Kindle on
        // Windows — the USB scan above can never show one (matching a vendor-class
        // Kindle needs an open the WPD driver binding forbids).
        if kindlefill_core::wpd::device_present() {
            println!("  WPD: at least one portable device present — the storages section opens it");
        } else {
            println!("  WPD: no portable devices");
        }
    }

    println!("\n== storages ==");
    let device = open().await?;
    let info = device.device_info();
    println!("  device: {} {}", info.manufacturer, info.model);
    for storage in device.storages().await? {
        let s = storage.info();
        // Capacity 0 is the backend's placeholder for "the device didn't answer",
        // not a measurement — printing "0 B free" here would be the same lie the
        // engine's SpaceUnreported guard exists to refuse.
        if s.total_capacity == 0 {
            println!(
                "  {:?}: capacity/free NOT REPORTED (fill would refuse — unlock the \
                 device and replug), writable={}, fs={:?}",
                s.description, s.is_writable, s.filesystem_type
            );
        } else {
            println!(
                "  {:?}: {} total, {} free, writable={}, fs={:?}",
                s.description,
                human_bytes(s.total_capacity),
                human_bytes(s.free_space),
                s.is_writable,
                s.filesystem_type
            );
        }
    }
    Ok(())
}

/// The measurement that decides the design: does free space actually move after a
/// write, and how fast does the wire run?
///
/// If reported free space does *not* track writes, the whole measure-write-remeasure
/// loop is invalid and the tool needs rethinking — so this reports the delta plainly
/// rather than asserting success.
async fn bench() -> Result<()> {
    let device = open().await?;
    let mut storage = writable_storage(&device).await?;

    storage.refresh().await?;
    let baseline = storage.info().free_space;
    println!("baseline free: {}", human_bytes(baseline));

    // `None`, not `Some(ObjectHandle::ROOT)` — see `engine::find_fill_dir`. WPD has no
    // root object to look up, so the sentinel spelling fails on Windows hardware.
    let dir = storage
        .create_folder(None, BENCH_DIR)
        .await
        .context("could not create a folder at the storage root")?;

    // Split so cleanup is reached on *every* exit, not only the happy one. Every step
    // of the measurement used to be a `?` straight out of the function, so a failure
    // at the 512 MiB upload — the longest, and therefore the likeliest — left up to
    // 656 MiB committed with the folder still there. Nothing in this tool could then
    // remove it: `clean` matches only `fill_NNNN.bin`, and `find_filler_folders` only
    // reports folders holding names `clean` recognises, so `kindlefill_bench` was
    // invisible to discovery as well.
    let mut written = Vec::new();
    let measured = bench_uploads(&mut storage, dir, &mut written).await;
    let cleaned = bench_cleanup(&mut storage, dir, &written).await;

    // The measurement failure is the cause; a cleanup failure on top of it is a
    // consequence, so the cause is reported first.
    measured?;
    if let Err(e) = cleaned {
        let stranded: u64 = BENCH_SIZES.iter().map(|(_, size)| size).sum();
        eprintln!();
        eprintln!(
            "!! could not remove {BENCH_DIR} — up to {} is still on the device.",
            human_bytes(stranded)
        );
        eprintln!("   Empty it with:  kindlefill purge --dir {BENCH_DIR}");
        return Err(e);
    }
    storage.refresh().await?;
    println!("free now {}", human_bytes(storage.info().free_space));
    Ok(())
}

/// The folder `bench` writes into, and the sizes it writes.
///
/// Constants because the recovery message has to name exactly what to remove, and a
/// name that drifts out of step with the message is worse than no message at all.
const BENCH_DIR: &str = "kindlefill_bench";
const BENCH_SIZES: [(&str, u64); 3] = [
    ("16MiB", 16 * MIB),
    ("128MiB", 128 * MIB),
    ("512MiB", 512 * MIB),
];

/// The measurement itself. Every handle it commits is pushed to `written` before
/// anything else can fail, so the caller's cleanup sees them even when this returns
/// an error.
async fn bench_uploads(
    storage: &mut Storage,
    dir: ObjectHandle,
    written: &mut Vec<ObjectHandle>,
) -> Result<()> {
    for (label, size) in BENCH_SIZES {
        let name = format!("bench_{label}.bin");
        let before = {
            storage.refresh().await?;
            storage.info().free_space
        };

        let start = Instant::now();
        let handle = match storage
            .upload(
                Some(dir),
                NewObjectInfo::file(&name, size),
                kindlefill_core::ZeroStream::new(size),
            )
            .await
        {
            Ok(handle) => handle,
            Err(e) => {
                // A failed data phase can leave a partial object on the device, which
                // the library deliberately doesn't auto-delete — the same hazard
                // `fill_with_cancel` handles, and it was ignored here. Handing it to
                // the shared cleanup rather than deleting it inline keeps one removal
                // path instead of two that can disagree.
                if let Some(partial) = e.partial {
                    written.push(partial);
                }
                return Err(anyhow::anyhow!("upload of {label} failed: {}", e.source));
            }
        };
        let elapsed = start.elapsed();
        written.push(handle);

        storage.refresh().await?;
        let after = storage.info().free_space;
        let observed = before.saturating_sub(after);
        let mbps = size as f64 / MIB as f64 / elapsed.as_secs_f64();

        println!(
            "  {label}: {:.1} MB/s ({:.1}s) | free {} -> {} (delta {}, overhead {})",
            mbps,
            elapsed.as_secs_f64(),
            human_bytes(before),
            human_bytes(after),
            human_bytes(observed),
            observed as i64 - size as i64,
        );
        if observed == 0 {
            println!("  !! free space did not move — the convergence loop cannot work as designed");
        }
    }
    Ok(())
}

/// Remove exactly what `bench` created — the handles it collected, then the folder.
///
/// Bounded to handles this run holds, which is the same guarantee the rest of the
/// crate gives: nothing is removed by name-matching or by walking the device.
async fn bench_cleanup(
    storage: &mut Storage,
    dir: ObjectHandle,
    written: &[ObjectHandle],
) -> Result<()> {
    print!("cleaning up... ");
    let _ = io::Write::flush(&mut io::stdout());
    for handle in written {
        storage.delete(*handle).await?;
    }
    storage.delete(dir).await?;
    Ok(())
}

async fn status(dir_name: &str) -> Result<()> {
    let device = open().await?;
    let mut storage = writable_storage(&device).await?;
    storage.refresh().await?;
    let s = storage.info();
    println!(
        "{:?}: {} free of {}",
        s.description,
        human_bytes(s.free_space),
        human_bytes(s.total_capacity)
    );

    match engine::find_fill_dir(&storage, dir_name).await? {
        None => println!("no {dir_name} folder"),
        Some(dir) => {
            let fillers = engine::list_fillers(&storage, dir.handle).await?;
            let total: u64 = fillers.iter().map(|f| f.size).sum();
            println!(
                "{dir_name}: {} filler object(s), {}",
                fillers.len(),
                human_bytes(total)
            );
            // Anything else in there is the user's, and `clean` will leave it — say so
            // here rather than letting them discover the folder survived and wonder why.
            // Grouped by size: the trim granularity available after a fill is
            // exactly the set of small objects the final approach laid down.
            let mut by_size: std::collections::BTreeMap<u64, usize> = Default::default();
            for f in &fillers {
                *by_size.entry(f.size).or_default() += 1;
            }
            for (size, count) in by_size.iter().rev() {
                println!("  {count} x {}", human_bytes(*size));
            }
            let foreign = engine::list_foreign(&storage, dir.handle).await?;
            for other in &foreign {
                println!("  (not ours, will be left alone: {})", other.filename);
            }
        }
    }

    // Filling around an already-downloaded update accomplishes nothing, so this is
    // worth knowing before a 17-minute transfer rather than after one.
    let updates = engine::list_staged_updates(&storage).await?;
    if updates.is_empty() {
        println!("no staged firmware update");
    } else {
        for u in &updates {
            println!(
                "staged firmware update: {} ({})",
                u.filename,
                human_bytes(u.size)
            );
        }
        println!("  filling around a staged update accomplishes nothing — remove it first");
    }
    Ok(())
}

/// Draw an in-place progress bar, or fall back to plain lines when output isn't a
/// terminal — carriage-return redraws turn a piped log into an unreadable single line.
fn render_progress(p: &kindlefill_core::FillProgress, tty: bool) {
    let eta = p
        .eta
        .map_or_else(|| "estimating".to_string(), kindlefill_core::human_eta);
    let rate = p.rate.map_or_else(
        || "--".to_string(),
        |r| format!("{:.1} MB/s", r / MIB as f64),
    );

    if !tty {
        println!(
            "  {:.0}% | {} of {} | {rate} | {eta} left",
            p.percent(),
            human_bytes(p.done),
            human_bytes(p.total)
        );
        return;
    }

    const WIDTH: usize = 32;
    let filled = ((p.fraction() * WIDTH as f64).round() as usize).min(WIDTH);
    eprint!(
        "\r  [{}{}] {:5.1}% | {} / {} | {rate} | {eta} left    ",
        "#".repeat(filled),
        "-".repeat(WIDTH - filled),
        p.percent(),
        human_bytes(p.done),
        human_bytes(p.total),
    );
    let _ = io::Write::flush(&mut io::stderr());
}

async fn fill(low: u64, high: u64, dir_name: &str, overwrite: bool) -> Result<()> {
    let window = Window::new(low, high).map_err(|e| anyhow::anyhow!("{e}"))?;
    let device = open().await?;
    let mut storage = writable_storage(&device).await?;
    let tty = io::stderr().is_terminal();

    // Ctrl-C stops the fill cleanly rather than killing the process mid-object. That
    // matters: a hard kill leaves a partial object the device still counts against free
    // space, and nothing would ever delete it.
    let cancel = CancelToken::new();
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                eprintln!("\nstopping after the current chunk...");
                cancel.cancel();
            }
        });
    }

    if overwrite {
        let removed = engine::purge_fill_dir(&mut storage, dir_name, |event| {
            if let Event::Deleted { name, bytes, .. } = event {
                println!("  deleted {name} ({})", human_bytes(bytes));
            }
        })
        .await?;
        println!("emptied {dir_name}: {removed} item(s) deleted");
    }

    let started = Instant::now();
    let mut drew_bar = false;
    let outcome = engine::fill_with_cancel(
        &mut storage,
        window,
        dir_name,
        Some(&cancel),
        |event| match event {
            Event::Started { free, aim, total } => println!(
                "starting at {} free, steering to {} — {} to write",
                human_bytes(free),
                human_bytes(aim),
                human_bytes(total)
            ),
            Event::Progress(p) => {
                render_progress(&p, tty);
                drew_bar = true;
            }
            Event::Wrote { name, bytes, free } => {
                // Close the bar's line before printing, or the two overwrite each other.
                if drew_bar && tty {
                    eprintln!();
                    drew_bar = false;
                }
                println!(
                    "  wrote {name} ({}) -> {} free",
                    human_bytes(bytes),
                    human_bytes(free)
                );
            }
            Event::Finished { free } => {
                if drew_bar && tty {
                    eprintln!();
                    drew_bar = false;
                }
                println!("finished at {} free", human_bytes(free));
            }
            // Reclaimed debris from a killed run is worth a line — it is space
            // appearing from somewhere the user did not ask about, and silence here is
            // what let 94 MB sit unaccounted for in the first place. The removal branch's
            // own deletions stay quiet: those are the fill trimming its way into the
            // window, which the free-space readings already narrate.
            Event::Deleted { name, bytes, kind } => {
                if kind == engine::DeletedKind::Interrupted {
                    if drew_bar && tty {
                        eprintln!();
                        drew_bar = false;
                    }
                    println!(
                        "  reclaimed {name} ({}) — left by a run that was killed mid-write",
                        human_bytes(bytes)
                    );
                }
            }
            Event::DeleteRefused { name, bytes } => {
                if drew_bar && tty {
                    eprintln!();
                    drew_bar = false;
                }
                eprintln!(
                    "  !! {name} ({}) could not be deleted — the device refused it. \
                     Unplug and replug the Kindle, then try again.",
                    human_bytes(bytes)
                );
            }
        },
    )
    .await?;

    match outcome {
        engine::Outcome::InWindow { free } => println!(
            "done in {:.0}s — {} free, inside {}..{}",
            started.elapsed().as_secs_f64(),
            human_bytes(free),
            human_bytes(low),
            human_bytes(high)
        ),
        engine::Outcome::Overfilled { free, excess } => println!(
            "overfilled: {} free, {} below target. Run `kindlefill clean` to recover.",
            human_bytes(free),
            human_bytes(excess)
        ),
        engine::Outcome::Cancelled { free } => println!(
            "stopped at {} free. Filler written so far is intact — \
             re-run `fill` to resume, or `clean` to undo.",
            human_bytes(free)
        ),
    }
    Ok(())
}

async fn clean(dir_name: &str) -> Result<()> {
    let device = open().await?;
    let mut storage = writable_storage(&device).await?;
    let mut removed = 0u64;
    let report = engine::clean(&mut storage, dir_name, |event| match event {
        Event::Deleted { name, bytes, .. } => {
            removed += bytes;
            println!("  deleted {name} ({})", human_bytes(bytes));
        }
        // The one case where saying nothing would be a lie by omission: the object is
        // still on the device and its bytes are still spent.
        Event::DeleteRefused { name, bytes } => eprintln!(
            "  !! {name} ({}) was NOT deleted — the device refused it. Unplug and \
             replug the Kindle, then run this again.",
            human_bytes(bytes)
        ),
        Event::Finished { free } => println!("free now {}", human_bytes(free)),
        _ => {}
    })
    .await?;
    if report.removed == 0 {
        println!("nothing to remove in {dir_name}");
        for f in engine::find_filler_folders(&storage).await? {
            println!(
                "  filler is in {}: {} in {} file(s)",
                f.name,
                human_bytes(f.bytes),
                f.files
            );
        }
    }
    println!(
        "reclaimed {} — {} free",
        human_bytes(removed),
        human_bytes(report.free)
    );
    Ok(())
}

/// Empty a named root folder, whatever is in it.
///
/// The one CLI verb that removes content this tool didn't write, so it says what it
/// deleted, by name, as it goes. It reaches [`engine::purge_fill_dir`] — the same
/// function behind `fill --overwrite` — rather than reimplementing the walk, which is
/// also why the folder itself survives: `--overwrite` needs somewhere to write into
/// immediately afterwards. The bytes are what matter here; an empty folder isn't worth
/// a second copy of "is this folder empty" living in the CLI.
async fn purge(dir_name: &str) -> Result<()> {
    let device = open().await?;
    let mut storage = writable_storage(&device).await?;
    let mut refused = 0usize;
    let removed = engine::purge_fill_dir(&mut storage, dir_name, |event| match event {
        Event::Deleted { name, bytes, .. } => {
            println!("  deleted {name} ({})", human_bytes(bytes));
        }
        Event::DeleteRefused { name, bytes } => {
            refused += 1;
            eprintln!(
                "  !! {name} ({}) was NOT deleted — the device refused it.",
                human_bytes(bytes)
            );
        }
        _ => {}
    })
    .await?;

    // Told apart from an empty folder, because the two need opposite reactions: one is
    // "there was nothing to do", the other is "there was, and it did not happen".
    if refused > 0 {
        println!(
            "{refused} item(s) could not be deleted and are still on the device. This is \
             usually an interrupted transfer the device has not released — unplug and \
             replug the Kindle, then run this again."
        );
    } else if removed == 0 {
        println!("nothing to remove — no {dir_name} folder at the storage root, or it was empty");
    } else {
        println!("emptied {dir_name}: {removed} item(s) deleted (the folder itself is left)");
    }
    storage.refresh().await?;
    println!("free now {}", human_bytes(storage.info().free_space));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mtp_rs::{VirtualDeviceConfig, VirtualStorageConfig};

    #[test]
    fn sizes_that_are_not_sizes_are_refused_rather_than_rounded_to_zero() {
        assert_eq!(parse_size("50MB"), Ok(50 * MIB));
        assert_eq!(parse_size("2GB"), Ok(2 * 1024 * MIB));
        assert_eq!(parse_size(" 90 MiB "), Ok(90 * MIB));
        assert_eq!(parse_size("1024"), Ok(1024));
        assert_eq!(
            parse_size("0MB"),
            Ok(0),
            "zero is a number, if a strange one"
        );

        // Each of these used to parse cleanly to 0 and fill toward an aim nobody asked
        // for: the float cast saturates on negatives and maps NaN to zero.
        for bad in ["-5MB", "nanMB", "infGB", "-0.5GB", "-1", "not-a-size"] {
            assert!(parse_size(bad).is_err(), "{bad} should be refused");
        }
    }

    /// `bench` used to be a straight line of `?` from "create the folder" to "clean it
    /// up", so a failed upload exited with everything before it committed and the
    /// folder still there — bytes that `clean` could not match and
    /// `find_filler_folders` could not see, on a device whose owner had no way to find
    /// out.
    ///
    /// The failure is forced by putting a directory where the first upload's file has
    /// to go, not by running it out of room: the virtual device reports free space from
    /// its configured capacity but writes through to a real directory, so an
    /// over-capacity upload reports a nonsense delta and *succeeds*. A name already held
    /// by a directory is an error the backing filesystem actually produces, and produces
    /// the same way everywhere.
    ///
    /// It used to clear the folder's write bit instead. That is a Unix-only lever:
    /// Windows has no write bit on a directory, and `set_readonly` there sets an
    /// attribute Explorer reads but `CreateFile` ignores, so the upload succeeded and
    /// this test failed on Windows alone. Any replacement has to fail the same way on
    /// every platform CI runs.
    #[tokio::test]
    async fn a_failed_bench_upload_leaves_nothing_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let device = MtpDevice::builder()
            .open_virtual(VirtualDeviceConfig {
                manufacturer: "Amazon".into(),
                model: "Virtual Kindle".into(),
                serial: "BENCH001".into(),
                storages: vec![VirtualStorageConfig {
                    description: "Internal Storage".into(),
                    capacity: 200 * MIB,
                    backing_dir: tmp.path().to_path_buf(),
                    read_only: false,
                }],
                watch_backing_dirs: false,
                event_poll_interval: std::time::Duration::ZERO,
                ..Default::default()
            })
            .await
            .expect("virtual device should open");
        let mut storage = writable_storage(&device).await.expect("storage");

        let dir = storage
            .create_folder(Some(ObjectHandle::ROOT), BENCH_DIR)
            .await
            .expect("create bench folder");

        // Stands in for the objects a real run commits before the failing one. They
        // are in `written`, and if cleanup doesn't reach them they are stranded for
        // good — no verb in the tool could find a `bench_*.bin` to remove.
        let committed = storage
            .upload(
                Some(dir),
                // A name none of BENCH_SIZES uses, so it can neither collide with the
                // blocker below nor be re-keyed underneath us by an upload being
                // measured.
                NewObjectInfo::file("bench_committed.bin", MIB),
                kindlefill_core::ZeroStream::new(MIB),
            )
            .await
            .expect("seed a committed object");
        let mut written = vec![committed];

        let bench_path = tmp.path().join(BENCH_DIR);
        // Sits on the exact name the first measured upload will create. The device
        // never sees it — `watch_backing_dirs` is off, so it is in no object list — and
        // it is removed before cleanup runs, because cleanup has to be able to take the
        // folder with it and nothing here is a handle `written` holds.
        let blocker = bench_path.join(format!("bench_{}.bin", BENCH_SIZES[0].0));
        std::fs::create_dir(&blocker).expect("block the first upload's name");
        let measured = bench_uploads(&mut storage, dir, &mut written).await;
        std::fs::remove_dir(&blocker).expect("unblock before cleanup");

        assert!(
            measured.is_err(),
            "an unwritable folder must fail the upload"
        );

        bench_cleanup(&mut storage, dir, &written)
            .await
            .expect("cleanup must run after a failed measurement");

        // Asked of the backing directory rather than the device's object list: this is
        // the thing a user would be left with, and it is what "self-cleaning" claims.
        assert!(
            !bench_path.exists(),
            "a failed bench must leave no {BENCH_DIR} behind"
        );
    }
}
