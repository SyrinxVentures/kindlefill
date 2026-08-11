//! `kindlefill` — fill a Kindle's storage to a target free-space window, and undo it.
//!
//! Also the spike: `probe` and `bench` exist to answer the questions that can only be
//! answered with a real Kindle on the other end of the cable — does the device
//! enumerate, does reported free space actually move after a write, how fast is the
//! wire, and does taming `ptpcamerad` need root.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use kindlefill_core::{
    engine, human_bytes, is_exclusive_access, ptpcamerad, Event, Window, MIB,
};
use mtp_rs::{CancelToken, MtpDevice, NewObjectInfo, ObjectHandle, Storage};
use std::io::{self, IsTerminal};
use std::time::Instant;

#[derive(Parser)]
#[command(name = "kindlefill", about = "Fill a Kindle's storage to block OTA updates")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Enumerate devices and report what we can see. Changes nothing.
    Probe,
    /// Measure write throughput and verify free space tracks writes. Cleans up after itself.
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
    },
    /// Delete all filler this tool wrote.
    Clean {
        #[arg(long, default_value = engine::DEFAULT_FILL_DIR)]
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
        .map(|n| (n * scale as f64) as u64)
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Probe => probe().await,
        Command::Bench => bench().await,
        Command::Status { dir } => status(&dir).await,
        Command::Fill { low, high, dir } => fill(low, high, &dir).await,
        Command::Clean { dir } => clean(&dir).await,
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

    println!("\n== devices ==");
    match MtpDevice::list_devices() {
        Ok(devices) if devices.is_empty() => println!("  none found"),
        Ok(devices) => {
            for d in &devices {
                println!("  {}", d.display());
            }
        }
        Err(e) => println!("  enumeration failed: {e}"),
    }

    println!("\n== storages ==");
    let device = open().await?;
    let info = device.device_info();
    println!("  device: {} {}", info.manufacturer, info.model);
    for storage in device.storages().await? {
        let s = storage.info();
        println!(
            "  {:?}: {} total, {} free, writable={}, fs={:?}",
            s.description,
            human_bytes(s.total_capacity),
            human_bytes(s.free_space),
            s.is_writable,
            s.filesystem_type
        );
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

    let dir = storage
        .create_folder(Some(ObjectHandle::ROOT), "kindlefill_bench")
        .await
        .context("could not create a folder at the storage root")?;

    let mut written = Vec::new();
    for (label, size) in [("16MiB", 16 * MIB), ("128MiB", 128 * MIB), ("512MiB", 512 * MIB)] {
        let name = format!("bench_{label}.bin");
        let before = { storage.refresh().await?; storage.info().free_space };

        let start = Instant::now();
        let handle = storage
            .upload(
                Some(dir),
                NewObjectInfo::file(&name, size),
                kindlefill_core::ZeroStream::new(size),
            )
            .await
            .map_err(|e| anyhow::anyhow!("upload of {label} failed: {}", e.source))?;
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

    print!("cleaning up... ");
    for handle in written {
        storage.delete(handle).await?;
    }
    storage.delete(dir).await?;
    storage.refresh().await?;
    println!("free now {}", human_bytes(storage.info().free_space));
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
        .map_or_else(|| "estimating".to_string(), kindlefill_core::human_duration);
    let rate = p
        .rate
        .map_or_else(|| "--".to_string(), |r| format!("{:.1} MB/s", r / MIB as f64));

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

async fn fill(low: u64, high: u64, dir_name: &str) -> Result<()> {
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
            Event::Deleted { .. } => {}
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
    let free = engine::clean(&mut storage, dir_name, |event| match event {
        Event::Deleted { name, bytes } => {
            removed += bytes;
            println!("  deleted {name} ({})", human_bytes(bytes));
        }
        Event::Finished { free } => println!("free now {}", human_bytes(free)),
        _ => {}
    })
    .await?;
    println!("reclaimed {} — {} free", human_bytes(removed), human_bytes(free));
    Ok(())
}
