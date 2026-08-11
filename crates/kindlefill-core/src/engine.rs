//! Device-facing half: create `fill_disk`, drive the convergence loop, and undo it.
//!
//! The one rule that keeps this correct: **free space is re-read from the device
//! after every single write**. `Storage::info()` is a cached snapshot taken when the
//! storage was opened; only `Storage::refresh()` issues a fresh `GetStorageInfo`.
//! Deciding the next write from the cached value would loop forever or overshoot,
//! and it would look fine in every test that didn't use a real device.

use crate::plan::{next_step, Step, Window};
use crate::rate::{FillProgress, RateEstimator};
use crate::zeros::ZeroStream;
use mtp_rs::{Error, NewObjectInfo, ObjectHandle, ObjectInfo, Storage};
use std::ops::ControlFlow;
use std::time::{Duration, Instant};

/// Fastest we emit [`Event::Progress`]. Uploads report every chunk, which on a 13 GB
/// fill is thousands of callbacks — more redraws than any UI wants and more IPC than a
/// Tauri bridge should carry.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// Folder we put filler in. Matches the convention the Kindle modding guides use, so
/// a folder left by another tool is recognized and topped up rather than duplicated.
pub const FILL_DIR: &str = "fill_disk";

const FILL_PREFIX: &str = "fill_";
const FILL_SUFFIX: &str = ".bin";

/// Emitted as work happens so a UI can show progress without polling.
#[derive(Debug, Clone)]
pub enum Event {
    /// Free space as measured before any work.
    Started { free: u64, aim: u64, total: u64 },
    /// Throughput and ETA, emitted during uploads at most every
    /// [`PROGRESS_INTERVAL`].
    Progress(FillProgress),
    /// One filler object landed. `free` is the freshly re-read value.
    Wrote { name: String, bytes: u64, free: u64 },
    /// One filler object was removed.
    Deleted { name: String, bytes: u64 },
    /// Terminal reading.
    Finished { free: u64 },
}

/// How a fill ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Free space is inside the requested window.
    InWindow { free: u64 },
    /// Free space was already below the window when we stopped. Recoverable by
    /// deleting filler — surfaced rather than silently "fixed" because the excess
    /// may be the device's own doing, not ours.
    Overfilled { free: u64, excess: u64 },
}

#[derive(Debug)]
pub enum FillError {
    /// The storage reported itself read-only.
    ReadOnly { description: String },
    /// Not enough free space to reach the window — the device is already fuller
    /// than the target. Distinct from `Overfilled`: nothing was written.
    AlreadyBelowWindow { free: u64, low: u64 },
    Mtp(Error),
}

impl std::fmt::Display for FillError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FillError::ReadOnly { description } => {
                write!(f, "storage {description:?} is read-only")
            }
            FillError::AlreadyBelowWindow { free, low } => write!(
                f,
                "only {free} bytes free, already below the {low}-byte target; \
                 nothing to fill"
            ),
            FillError::Mtp(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for FillError {}

impl From<Error> for FillError {
    fn from(e: Error) -> Self {
        FillError::Mtp(e)
    }
}

/// Re-read free space from the device. Never trust the cached `info()`.
async fn measure(storage: &mut Storage) -> Result<u64, Error> {
    storage.refresh().await?;
    Ok(storage.info().free_space)
}

/// Locate `fill_disk` at the storage root, if it exists.
pub async fn find_fill_dir(storage: &Storage) -> Result<Option<ObjectInfo>, Error> {
    let objects = storage.list_objects(Some(ObjectHandle::ROOT)).await?;
    Ok(objects
        .into_iter()
        .find(|o| o.is_folder() && o.filename == FILL_DIR))
}

async fn ensure_fill_dir(storage: &Storage) -> Result<ObjectHandle, Error> {
    match find_fill_dir(storage).await? {
        Some(existing) => Ok(existing.handle),
        None => {
            storage
                .create_folder(Some(ObjectHandle::ROOT), FILL_DIR)
                .await
        }
    }
}

/// Filler objects currently in `fill_disk`.
///
/// Only files this tool would have written are matched. Anything else the user put
/// in the folder is left strictly alone — `clean` must never delete a stray book.
pub async fn list_fillers(
    storage: &Storage,
    dir: ObjectHandle,
) -> Result<Vec<ObjectInfo>, Error> {
    let mut fillers: Vec<ObjectInfo> = storage
        .list_objects(Some(dir))
        .await?
        .into_iter()
        .filter(|o| {
            o.is_file()
                && o.filename.starts_with(FILL_PREFIX)
                && o.filename.ends_with(FILL_SUFFIX)
        })
        .collect();
    fillers.sort_by(|a, b| a.filename.cmp(&b.filename));
    Ok(fillers)
}

/// Next unused sequence number, so an interrupted run tops up instead of colliding.
fn next_sequence(existing: &[ObjectInfo]) -> u32 {
    existing
        .iter()
        .filter_map(|o| {
            o.filename
                .strip_prefix(FILL_PREFIX)?
                .strip_suffix(FILL_SUFFIX)?
                .parse::<u32>()
                .ok()
        })
        .max()
        .map_or(0, |n| n + 1)
}

/// Fill until the device reports free space inside `window`.
///
/// Resumable: re-running after an interrupt continues from the existing filler set
/// rather than starting over, because the decision is made from measured free space,
/// not from a tally of what this process wrote.
pub async fn fill<F>(
    storage: &mut Storage,
    window: Window,
    mut on_event: F,
) -> Result<Outcome, FillError>
where
    F: FnMut(Event) + Send,
{
    if !storage.info().is_writable {
        return Err(FillError::ReadOnly {
            description: storage.info().description.clone(),
        });
    }

    let free_start = measure(storage).await?;
    if free_start < window.low() {
        return Err(FillError::AlreadyBelowWindow {
            free: free_start,
            low: window.low(),
        });
    }

    // What the job set out to move. Measured against `aim`, not `high`, because that's
    // where the loop actually steers.
    let total = free_start.saturating_sub(window.aim());
    on_event(Event::Started {
        free: free_start,
        aim: window.aim(),
        total,
    });

    let dir = ensure_fill_dir(storage).await?;
    let mut seq = next_sequence(&list_fillers(storage, dir).await?);
    let mut rate = RateEstimator::new();

    loop {
        let free = measure(storage).await?;
        match next_step(free, window) {
            Step::Done => {
                on_event(Event::Finished { free });
                return Ok(Outcome::InWindow { free });
            }
            Step::Overfilled { free, excess } => {
                on_event(Event::Finished { free });
                return Ok(Outcome::Overfilled { free, excess });
            }
            Step::Write(bytes) => {
                let name = format!("{FILL_PREFIX}{seq:04}{FILL_SUFFIX}");

                // Ground truth for everything already committed, taken from the device
                // rather than from a tally. In-object progress is added on top and then
                // discarded — the next iteration re-derives this from a fresh reading,
                // so a drifting estimate can never accumulate.
                let committed = free_start.saturating_sub(free);

                let upload = {
                    let now = Instant::now();
                    // `None` = nothing emitted for this object yet, so the first
                    // callback always reports. The bar should appear the moment a
                    // write starts rather than after a silent interval.
                    let mut last_emit: Option<Instant> = None;
                    let mut last_sample = (now, 0u64);
                    let rate = &mut rate;
                    let on_event = &mut on_event;

                    storage
                        .upload_with_progress(
                            Some(dir),
                            NewObjectInfo::file(&name, bytes),
                            ZeroStream::new(bytes),
                            move |p| {
                                let now = Instant::now();
                                rate.observe(
                                    p.bytes_transferred.saturating_sub(last_sample.1),
                                    now.duration_since(last_sample.0),
                                );
                                last_sample = (now, p.bytes_transferred);

                                let due = last_emit
                                    .is_none_or(|t| now.duration_since(t) >= PROGRESS_INTERVAL);
                                if due {
                                    last_emit = Some(now);
                                    let done = committed.saturating_add(p.bytes_transferred);
                                    on_event(Event::Progress(FillProgress {
                                        done,
                                        total,
                                        rate: rate.rate(),
                                        eta: rate.eta(total.saturating_sub(done)),
                                    }));
                                }
                                ControlFlow::Continue(())
                            },
                        )
                        .await
                };

                if let Err(e) = upload {
                    // A failed data phase can leave a partial object on the device.
                    // The library deliberately doesn't auto-delete it; if we leaked it,
                    // it would consume space that no `clean` run could find.
                    if let Some(partial) = e.partial {
                        let _ = storage.delete(partial).await;
                    }
                    return Err(FillError::Mtp(e.source));
                }
                seq += 1;
                let free = measure(storage).await?;
                on_event(Event::Wrote { name, bytes, free });
            }
        }
    }
}

/// Remove every filler object, then the `fill_disk` folder itself.
///
/// The folder is only removed if nothing but filler was in it, so a book someone
/// dropped in there survives — and so does the folder holding it.
pub async fn clean<F>(storage: &mut Storage, mut on_event: F) -> Result<u64, Error>
where
    F: FnMut(Event),
{
    let Some(dir) = find_fill_dir(storage).await? else {
        let free = measure(storage).await?;
        on_event(Event::Finished { free });
        return Ok(free);
    };

    for filler in list_fillers(storage, dir.handle).await? {
        storage.delete(filler.handle).await?;
        on_event(Event::Deleted {
            name: filler.filename,
            bytes: filler.size,
        });
    }

    if storage.list_objects(Some(dir.handle)).await?.is_empty() {
        storage.delete(dir.handle).await?;
    }

    let free = measure(storage).await?;
    on_event(Event::Finished { free });
    Ok(free)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(name: &str) -> ObjectInfo {
        let mut o = ObjectInfo::default();
        o.filename = name.to_string();
        o
    }

    #[test]
    fn sequence_starts_at_zero_on_an_empty_folder() {
        assert_eq!(next_sequence(&[]), 0);
    }

    #[test]
    fn sequence_resumes_after_the_highest_existing_filler() {
        let existing = vec![obj("fill_0000.bin"), obj("fill_0007.bin"), obj("fill_0003.bin")];
        assert_eq!(next_sequence(&existing), 8);
    }

    #[test]
    fn sequence_ignores_names_this_tool_did_not_write() {
        let existing = vec![obj("mybook.azw3"), obj("fill_notanumber.bin"), obj("fill_.bin")];
        assert_eq!(next_sequence(&existing), 0);
    }
}
