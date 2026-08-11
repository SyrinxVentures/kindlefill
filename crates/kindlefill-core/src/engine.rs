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
use mtp_rs::{CancelToken, Error, NewObjectInfo, ObjectHandle, ObjectInfo, Storage};
use std::ops::ControlFlow;
use std::time::{Duration, Instant};

/// Fastest we emit [`Event::Progress`]. Uploads report every chunk, which on a 13 GB
/// fill is thousands of callbacks — more redraws than any UI wants and more IPC than a
/// Tauri bridge should carry.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// Default folder we put filler in. Matches the convention the Kindle modding guides
/// use, so a folder left by another tool is recognized and topped up rather than
/// duplicated. Configurable because that same convention means the folder may already
/// exist holding something that isn't ours.
pub const DEFAULT_FILL_DIR: &str = "fill_disk";

const FILL_PREFIX: &str = "fill_";
const FILL_SUFFIX: &str = ".bin";

/// Prefix and suffix of a staged over-the-air firmware image.
///
/// The Kindle downloads an update to the storage root as `update_*.bin` and installs
/// it on the next restart. Filling around one accomplishes nothing — the bytes are
/// already on the device — which is why this is worth detecting before a 17-minute
/// transfer rather than after.
const UPDATE_PREFIX: &str = "update_";
const UPDATE_SUFFIX: &str = ".bin";

/// Why a folder name can't be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameError {
    Empty,
    /// Contains a character that isn't a filename, or a leading/trailing space that
    /// would make the folder impossible to identify on the device.
    Illegal { name: String },
}

impl std::fmt::Display for NameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NameError::Empty => write!(f, "the folder name can't be empty"),
            NameError::Illegal { name } => write!(
                f,
                "{name:?} isn't a usable folder name — use letters, digits, spaces, \
                 dot, dash or underscore, and no leading or trailing space"
            ),
        }
    }
}

impl std::error::Error for NameError {}

/// Check a folder name before it reaches the device.
///
/// A path separator here would be the difference between creating one folder at the
/// root and walking somewhere else entirely, so it's rejected rather than sanitized —
/// silently rewriting a name the user typed would leave them looking for a folder
/// that isn't there.
pub fn validate_dir_name(name: &str) -> Result<(), NameError> {
    if name.is_empty() {
        return Err(NameError::Empty);
    }
    let illegal = name != name.trim()
        || name.chars().all(|c| c == '.')
        || name
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'));
    if illegal {
        return Err(NameError::Illegal {
            name: name.to_string(),
        });
    }
    Ok(())
}

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
    /// The caller cancelled. Not an error: the filler written so far is intact and
    /// valid, and re-running `fill` resumes from it.
    Cancelled { free: u64 },
}

#[derive(Debug)]
pub enum FillError {
    /// The storage reported itself read-only.
    ReadOnly { description: String },
    /// Not enough free space to reach the window — the device is already fuller
    /// than the target. Distinct from `Overfilled`: nothing was written.
    AlreadyBelowWindow { free: u64, low: u64 },
    /// The requested filler folder name isn't usable.
    BadName(NameError),
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
            FillError::BadName(e) => write!(f, "{e}"),
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

impl From<NameError> for FillError {
    fn from(e: NameError) -> Self {
        FillError::BadName(e)
    }
}

/// Re-read free space from the device. Never trust the cached `info()`.
async fn measure(storage: &mut Storage) -> Result<u64, Error> {
    storage.refresh().await?;
    Ok(storage.info().free_space)
}

/// Locate the filler folder at the storage root, if it exists.
///
/// Root only, and an exact name match. Nothing in this module ever recurses looking
/// for a folder to operate on — the one folder it will touch is the one named here.
pub async fn find_fill_dir(
    storage: &Storage,
    dir_name: &str,
) -> Result<Option<ObjectInfo>, Error> {
    let objects = storage.list_objects(Some(ObjectHandle::ROOT)).await?;
    Ok(objects
        .into_iter()
        .find(|o| o.is_folder() && o.filename == dir_name))
}

async fn ensure_fill_dir(storage: &Storage, dir_name: &str) -> Result<ObjectHandle, Error> {
    match find_fill_dir(storage, dir_name).await? {
        Some(existing) => Ok(existing.handle),
        None => {
            storage
                .create_folder(Some(ObjectHandle::ROOT), dir_name)
                .await
        }
    }
}

/// Staged over-the-air firmware images at the storage root.
///
/// Root-only and read-only: this reports what's there. Deleting any of it needs
/// [`delete_staged_updates`] and an explicit list of names from the user, because an
/// update image is the one piece of device content this tool will remove that it
/// didn't write.
pub async fn list_staged_updates(storage: &Storage) -> Result<Vec<ObjectInfo>, Error> {
    let mut found: Vec<ObjectInfo> = storage
        .list_objects(Some(ObjectHandle::ROOT))
        .await?
        .into_iter()
        .filter(|o| o.is_file() && is_staged_update(&o.filename))
        .collect();
    found.sort_by(|a, b| a.filename.cmp(&b.filename));
    Ok(found)
}

/// Whether a root filename looks like a staged firmware image.
///
/// Matched case-insensitively on `update_….bin`. Deliberately a shape rather than a
/// list of known model names: the names vary by device generation, and a new one
/// showing up unrecognized would be the failure that matters. Breadth is safe here
/// only because nothing is deleted without the user seeing the exact filenames first.
fn is_staged_update(filename: &str) -> bool {
    let lower = filename.to_ascii_lowercase();
    lower.starts_with(UPDATE_PREFIX) && lower.ends_with(UPDATE_SUFFIX)
}

/// Delete named staged firmware images from the storage root.
///
/// Three independent gates, all of which must agree before anything is deleted: the
/// object is at the **root**, its name matches the **staged-update shape**, and it is
/// in the caller's **explicit list**. The list alone is not trusted — it arrives from
/// the UI, and a name in it that isn't a root-level update image is ignored rather
/// than deleted. That's what keeps "delete the update" from ever becoming "delete a
/// book that was named convincingly".
///
/// Returns the objects actually removed.
pub async fn delete_staged_updates<F>(
    storage: &mut Storage,
    names: &[String],
    mut on_event: F,
) -> Result<Vec<ObjectInfo>, Error>
where
    F: FnMut(Event),
{
    let doomed: Vec<ObjectInfo> = list_staged_updates(storage)
        .await?
        .into_iter()
        .filter(|o| names.iter().any(|n| n == &o.filename))
        .collect();

    for update in &doomed {
        storage.delete(update.handle).await?;
        on_event(Event::Deleted {
            name: update.filename.clone(),
            bytes: update.size,
        });
    }

    let free = measure(storage).await?;
    on_event(Event::Finished { free });
    Ok(doomed)
}

/// The single place that decides whether a filename is filler *this tool wrote*.
///
/// Everything destructive keys off this one answer: `clean`'s delete set and the
/// resume sequence both come from here. They used to decide separately and
/// disagreed — the delete set accepted any `fill_*.bin` while the sequence required
/// digits, so a file a user dropped in `fill_disk` named `fill_notes.bin` was not a
/// sequence number but *was* deletable. One reading, one answer.
///
/// The round-trip check is what makes it exact rather than merely strict: a name is
/// ours only if formatting the number back reproduces it byte for byte. That rejects
/// `fill_12.bin` and `fill_00000007.bin` — same integer, not a name we would ever
/// write — without hard-coding a digit count that a long-running resume could
/// eventually exceed.
fn filler_sequence(filename: &str) -> Option<u32> {
    let digits = filename
        .strip_prefix(FILL_PREFIX)?
        .strip_suffix(FILL_SUFFIX)?;
    // `u32::from_str` accepts a leading `+`; we never write one.
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u32 = digits.parse().ok()?;
    (format!("{n:04}") == digits).then_some(n)
}

/// Filler objects currently in `fill_disk`.
///
/// Only files this tool would have written are matched. Anything else in the folder
/// is left strictly alone — `clean` must never delete a stray book.
pub async fn list_fillers(
    storage: &Storage,
    dir: ObjectHandle,
) -> Result<Vec<ObjectInfo>, Error> {
    let mut fillers: Vec<ObjectInfo> = storage
        .list_objects(Some(dir))
        .await?
        .into_iter()
        .filter(|o| o.is_file() && filler_sequence(&o.filename).is_some())
        .collect();
    fillers.sort_by(|a, b| a.filename.cmp(&b.filename));
    Ok(fillers)
}

/// A root-level folder holding filler this tool wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillerFolder {
    pub name: String,
    pub files: usize,
    pub bytes: u64,
}

/// Find filler wherever it actually is, not only where we expected it.
///
/// Nothing remembers the folder name between launches, so it resets to the default
/// while the filler may be under any name the user chose. Looking only at the
/// configured name meant a relaunch reported "not present yet" over 24.88 GB of
/// filler sitting in a renamed folder — and worse, `clean` aimed at the default
/// deleted nothing and said it had succeeded.
///
/// Root-level only, and a folder counts only if it holds something [`list_fillers`]
/// recognises, so this can't mistake a books folder for ours.
pub async fn find_filler_folders(storage: &Storage) -> Result<Vec<FillerFolder>, Error> {
    let mut found = Vec::new();
    for object in storage.list_objects(Some(ObjectHandle::ROOT)).await? {
        if !object.is_folder() {
            continue;
        }
        let fillers = list_fillers(storage, object.handle).await?;
        if fillers.is_empty() {
            continue;
        }
        found.push(FillerFolder {
            name: object.filename,
            files: fillers.len(),
            bytes: fillers.iter().map(|f| f.size).sum(),
        });
    }
    // Biggest first: if there are several, the one holding the most is the one whose
    // space someone is trying to get back.
    found.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
    Ok(found)
}

/// What a [`clean`] actually did.
///
/// `removed` exists so a caller can tell "there was nothing to remove" from "removed
/// everything", which reporting only free space cannot. Saying "Filler removed" after
/// deleting nothing is how someone concludes their device is clean while 24.88 GB of
/// filler sits in a folder under a different name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanReport {
    pub free: u64,
    pub removed: usize,
    pub bytes: u64,
}

/// Everything in `fill_disk` that is *not* ours, so a caller can refuse to touch a
/// folder someone else's content is living in.
pub async fn list_foreign(
    storage: &Storage,
    dir: ObjectHandle,
) -> Result<Vec<ObjectInfo>, Error> {
    Ok(storage
        .list_objects(Some(dir))
        .await?
        .into_iter()
        .filter(|o| !(o.is_file() && filler_sequence(&o.filename).is_some()))
        .collect())
}

/// Empty the filler folder completely, including content this tool did not write.
///
/// This is the only operation in the crate that removes device content we can't prove
/// we created, and it exists solely because a caller asked to take the folder over
/// wholesale. Nothing calls it implicitly: `fill` does not, `clean` does not, and the
/// front ends reach it only behind an explicit opt-in that names the loss.
///
/// Depth-first, because MTP will not delete a folder that still has children — and
/// doing the recursion here rather than trusting the library to means the set of
/// objects removed is exactly the set this function walked.
///
/// Scoped to the named folder at the storage root: it takes a name, resolves it the
/// same way everything else does, and can't be pointed at the root itself.
pub async fn purge_fill_dir<F>(
    storage: &mut Storage,
    dir_name: &str,
    mut on_event: F,
) -> Result<usize, FillError>
where
    F: FnMut(Event) + Send,
{
    validate_dir_name(dir_name)?;
    let Some(dir) = find_fill_dir(storage, dir_name).await? else {
        return Ok(0);
    };
    let removed = purge_children(storage, dir.handle, &mut on_event).await?;
    let free = measure(storage).await?;
    on_event(Event::Finished { free });
    Ok(removed)
}

/// Delete everything under `parent`, children before parents. Boxed because it
/// recurses, and an `async fn` that calls itself needs an indirection to have a size.
fn purge_children<'a, F>(
    storage: &'a mut Storage,
    parent: ObjectHandle,
    on_event: &'a mut F,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<usize, Error>> + Send + 'a>>
where
    F: FnMut(Event) + Send,
{
    Box::pin(async move {
        let mut removed = 0;
        for child in storage.list_objects(Some(parent)).await? {
            if child.is_folder() {
                removed += purge_children(storage, child.handle, on_event).await?;
            }
            storage.delete(child.handle).await?;
            on_event(Event::Deleted {
                name: child.filename,
                bytes: child.size,
            });
            removed += 1;
        }
        Ok(removed)
    })
}

/// Next unused sequence number, so an interrupted run tops up instead of colliding.
fn next_sequence(existing: &[ObjectInfo]) -> u32 {
    existing
        .iter()
        .filter_map(|o| filler_sequence(&o.filename))
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
    dir_name: &str,
    on_event: F,
) -> Result<Outcome, FillError>
where
    F: FnMut(Event) + Send,
{
    fill_with_cancel(storage, window, dir_name, None, on_event).await
}

/// Like [`fill`], but abortable.
///
/// A full fill is a ~17-minute operation on a 32 GB device, so any UI driving it needs
/// a working Stop button — and one that stops in a second, not at the end of the
/// current object. Cancellation is therefore checked inside the upload's own progress
/// callback, not merely between objects: a 1 GiB write is ~40 seconds on its own.
///
/// Stopping is safe at any point. The half-written object is deleted, everything
/// already committed stays valid, and a later `fill` resumes from it.
pub async fn fill_with_cancel<F>(
    storage: &mut Storage,
    window: Window,
    dir_name: &str,
    cancel: Option<&CancelToken>,
    mut on_event: F,
) -> Result<Outcome, FillError>
where
    F: FnMut(Event) + Send,
{
    validate_dir_name(dir_name)?;
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

    let dir = ensure_fill_dir(storage, dir_name).await?;
    let mut seq = next_sequence(&list_fillers(storage, dir).await?);
    let mut rate = RateEstimator::new();
    // One clock for the whole job. The rate window spans object boundaries, so it
    // needs a timeline that doesn't restart with each upload.
    let job_start = Instant::now();

    loop {
        let free = measure(storage).await?;
        if cancel.is_some_and(CancelToken::is_cancelled) {
            on_event(Event::Finished { free });
            return Ok(Outcome::Cancelled { free });
        }
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
                    let rate = &mut rate;
                    let on_event = &mut on_event;
                    let _ = now;

                    storage
                        .upload_with_progress(
                            Some(dir),
                            NewObjectInfo::file(&name, bytes),
                            ZeroStream::new(bytes),
                            move |p| {
                                // Checked here rather than only between objects so Stop
                                // responds within a chunk instead of up to ~40s later.
                                if cancel.is_some_and(CancelToken::is_cancelled) {
                                    return ControlFlow::Break(());
                                }
                                let now = Instant::now();
                                let done = committed.saturating_add(p.bytes_transferred);
                                // Cumulative job bytes against elapsed job time, so the
                                // estimator can divide across a window that outlives any
                                // single object — the pauses between them are exactly
                                // what a throughput figure has to include.
                                rate.observe(done, now.duration_since(job_start));

                                let due = last_emit
                                    .is_none_or(|t| now.duration_since(t) >= PROGRESS_INTERVAL);
                                if due {
                                    last_emit = Some(now);
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
                    // Breaking out of the progress callback surfaces here as Cancelled.
                    // That's a deliberate stop, not a failure: the partial object is
                    // already gone and everything committed before it is still good.
                    if matches!(e.source, Error::Cancelled) {
                        let free = measure(storage).await?;
                        on_event(Event::Finished { free });
                        return Ok(Outcome::Cancelled { free });
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
pub async fn clean<F>(
    storage: &mut Storage,
    dir_name: &str,
    mut on_event: F,
) -> Result<CleanReport, FillError>
where
    F: FnMut(Event),
{
    // Validated here rather than left to callers, for the same reason `fill` does it:
    // there are two front ends, and a guard that each one has to remember is a guard
    // one of them will eventually forget.
    validate_dir_name(dir_name)?;
    let Some(dir) = find_fill_dir(storage, dir_name).await? else {
        let free = measure(storage).await?;
        on_event(Event::Finished { free });
        return Ok(CleanReport { free, removed: 0, bytes: 0 });
    };

    let (mut removed, mut bytes) = (0usize, 0u64);
    for filler in list_fillers(storage, dir.handle).await? {
        storage.delete(filler.handle).await?;
        removed += 1;
        bytes += filler.size;
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
    Ok(CleanReport { free, removed, bytes })
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

    /// The property that matters for `clean`: a name is ours only if we would write
    /// exactly it. Everything rejected here is a file `clean` must leave alone, and
    /// `list_fillers` — the delete set — is filtered by this same function, so this
    /// is the guard against deleting someone's book.
    #[test]
    fn only_names_this_tool_would_write_are_recognized_as_filler() {
        for ours in ["fill_0000.bin", "fill_0001.bin", "fill_9999.bin", "fill_10000.bin"] {
            assert_eq!(
                filler_sequence(ours).map(|n| format!("{FILL_PREFIX}{n:04}{FILL_SUFFIX}")),
                Some(ours.to_string()),
                "{ours} should round-trip as filler"
            );
        }
        for theirs in [
            "mybook.azw3",
            "fill_notes.bin",   // the bug: prefix+suffix matched, so it was deletable
            "fill_backup.bin",
            "fill_.bin",
            "fill_12.bin",      // right number, not a name we write
            "fill_00000007.bin",
            "fill_+001.bin",
            "fill_0001.bin.bak",
            "notfill_0001.bin",
        ] {
            assert_eq!(filler_sequence(theirs), None, "{theirs} must not be ours");
        }
    }
}
