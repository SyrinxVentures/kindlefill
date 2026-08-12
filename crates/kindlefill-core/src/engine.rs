//! Device-facing half: create `fill_disk`, drive the convergence loop, and undo it.
//!
//! The one rule that keeps this correct: **free space is re-read from the device
//! after every single write**. `Storage::info()` is a cached snapshot taken when the
//! storage was opened; only `Storage::refresh()` issues a fresh `GetStorageInfo`.
//! Deciding the next write from the cached value would loop forever or overshoot,
//! and it would look fine in every test that didn't use a real device.

use crate::plan::{next_removal, next_step, Removal, Step, Window};
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
    Illegal {
        name: String,
    },
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
        || name.chars().any(|c| {
            c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
        });
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
    /// One object was removed. See [`DeletedKind`] — a consumer keeping a filler tally
    /// needs to know which deletions are filler, and the event used to make it guess.
    Deleted {
        name: String,
        bytes: u64,
        kind: DeletedKind,
    },
    /// Terminal reading.
    Finished { free: u64 },
}

/// What a [`Event::Deleted`] was.
///
/// Three unrelated things emit that event — `clean` and the fill's removal branch,
/// which are always our own filler; `purge_fill_dir`, which is whatever was in the
/// folder; and `delete_staged_updates`, which is a firmware image and never filler.
/// Without this the app subtracted all three from its "existing filler" tally, so
/// deleting a 1.5 GB update from a device holding ten filler files briefly read "9
/// files, 8.5 GB". Classified where the fact is known rather than inferred downstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletedKind {
    /// Filler this tool wrote, by [`filler_sequence`]'s reading of the name.
    Filler,
    /// Something in the filler folder that this tool did not write.
    Foreign,
    /// A staged over-the-air firmware image.
    Update,
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
    ReadOnly {
        description: String,
    },
    /// Not enough free space to reach the window — the device is already fuller
    /// than the target. Distinct from `Overfilled`: nothing was written.
    AlreadyBelowWindow {
        free: u64,
        low: u64,
        /// Filler found under root folders other than the one asked for. Empty is
        /// meaningful: it means the space genuinely isn't ours to give back. Carried
        /// on the error so neither front end has to re-ask the device — and so the two
        /// of them can't word the answer differently.
        elsewhere: Vec<FillerFolder>,
    },
    /// The requested filler folder name isn't usable.
    BadName(NameError),
    /// The folder no longer holds what the caller was shown when they confirmed
    /// emptying it. Refusing is the whole point: the alternative is deleting files
    /// nobody was ever asked about.
    StaleConfirmation,
    Mtp(Error),
}

impl std::fmt::Display for FillError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FillError::ReadOnly { description } => {
                write!(f, "storage {description:?} is read-only")
            }
            FillError::AlreadyBelowWindow {
                free,
                low,
                elsewhere,
            } => {
                write!(
                    f,
                    "only {free} bytes free, already below the {low}-byte target; \
                     nothing to fill"
                )?;
                // "Nothing to fill" is true and, on its own, misleading: the folder
                // name resets to the default every launch, so the space this tool
                // could give back is routinely sitting under a name the user chose.
                // Same wording as `clean`'s equivalent, because it is the same answer.
                if let Some(f2) = elsewhere.first() {
                    write!(
                        f,
                        " — but {} of filler is in {}; switch to that folder and try \
                         again",
                        crate::human_bytes(f2.bytes),
                        f2.name
                    )?;
                }
                Ok(())
            }
            FillError::BadName(e) => write!(f, "{e}"),
            FillError::StaleConfirmation => write!(
                f,
                "the folder's contents changed since they were confirmed — \
                 re-check the folder and confirm again"
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
pub async fn find_fill_dir(storage: &Storage, dir_name: &str) -> Result<Option<ObjectInfo>, Error> {
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
            kind: DeletedKind::Update,
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
pub async fn list_fillers(storage: &Storage, dir: ObjectHandle) -> Result<Vec<ObjectInfo>, Error> {
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
pub async fn list_foreign(storage: &Storage, dir: ObjectHandle) -> Result<Vec<ObjectInfo>, Error> {
    Ok(storage
        .list_objects(Some(dir))
        .await?
        .into_iter()
        .filter(|o| !(o.is_file() && filler_sequence(&o.filename).is_some()))
        .collect())
}

/// A fingerprint of the content in `dir_name` that this tool didn't write.
///
/// `None` means there is nothing foreign in there — no confirmation is needed for that
/// folder, and none can be given for it.
///
/// Pure, so the caller that has already listed the folder doesn't list it twice, and so
/// the one that hasn't ([`current_overwrite_token`]) reaches the same answer by the same
/// route. The folder name is folded in deliberately: a confirmation is *for a folder's
/// contents*, and two folders that happen to hold identically-named files are still two
/// different decisions.
///
/// Not a security primitive. The webview is in-process and could send any string it
/// likes; the failure being closed off is a *stale* confirmation — one given for another
/// folder, or for a listing taken before something else landed in it — not a forged one.
#[must_use]
pub fn overwrite_token(dir_name: &str, foreign: &[ObjectInfo]) -> Option<String> {
    if foreign.is_empty() {
        return None;
    }
    let mut names: Vec<(&str, u64)> = foreign
        .iter()
        .map(|o| (o.filename.as_str(), o.size))
        .collect();
    // Sorted so the token describes the set, not the order the device happened to
    // list it in — MTP gives no ordering guarantee, and a token that changed between
    // two identical listings would refuse every confirmation.
    names.sort_unstable();

    let mut digest = Fnv1a::new();
    digest.write(dir_name.as_bytes());
    for (name, size) in names {
        digest.write(&[0]);
        digest.write(name.as_bytes());
        digest.write(&size.to_le_bytes());
    }
    Some(format!("{:016x}", digest.finish()))
}

/// [`overwrite_token`] for the folder as the device has it right now.
pub async fn current_overwrite_token(
    storage: &Storage,
    dir_name: &str,
) -> Result<Option<String>, Error> {
    let Some(dir) = find_fill_dir(storage, dir_name).await? else {
        return Ok(None);
    };
    Ok(overwrite_token(
        dir_name,
        &list_foreign(storage, dir.handle).await?,
    ))
}

/// FNV-1a, written out rather than reached for.
///
/// The token has to mean the same thing to the `detect` that displayed a listing and
/// the purge that acts on it. `DefaultHasher` is documented as free to change between
/// Rust releases, which is fine for a `HashMap` and wrong for a value two calls compare
/// for equality. Eight lines of arithmetic that can't drift is the cheaper guarantee.
struct Fnv1a(u64);

impl Fnv1a {
    fn new() -> Self {
        Fnv1a(0xcbf2_9ce4_8422_2325)
    }
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = (self.0 ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

/// Empty the folder, but only if what's in it is still what the caller was shown.
///
/// [`purge_fill_dir`] takes a name and empties it; the binding between "the user was
/// shown these files" and "this folder gets emptied" lived entirely in one line of
/// frontend JS resetting a checkbox. That line is correct and always runs — but it was
/// the *only* thing standing between a confirmation and a recursive delete, and it
/// lives in a state machine that has already been driven somewhere it shouldn't go.
/// This is the same binding expressed server-side: the token is recomputed from the
/// device as it is now, and a mismatch refuses instead of deleting. It also closes the
/// window where something lands in the folder between the listing and the purge.
///
/// The CLI's `--overwrite` deliberately doesn't come through here: there the folder
/// name and the flag are typed in one command, so the confirmation is bound to its
/// target by construction and a token would only be ceremony.
pub async fn purge_fill_dir_confirmed<F>(
    storage: &mut Storage,
    dir_name: &str,
    confirmed: &str,
    on_event: F,
) -> Result<usize, FillError>
where
    F: FnMut(Event) + Send,
{
    validate_dir_name(dir_name)?;
    if current_overwrite_token(storage, dir_name).await?.as_deref() != Some(confirmed) {
        return Err(FillError::StaleConfirmation);
    }
    purge_fill_dir(storage, dir_name, on_event).await
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
            // Asked of `filler_sequence` rather than assumed: a purge removes both
            // our filler and the folder's other contents, and a consumer keeping a
            // filler tally has to be able to tell which was which.
            let kind = if child.is_file() && filler_sequence(&child.filename).is_some() {
                DeletedKind::Filler
            } else {
                DeletedKind::Foreign
            };
            on_event(Event::Deleted {
                name: child.filename,
                bytes: child.size,
                kind,
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
    if free_start < window.low()
        && match find_fill_dir(storage, dir_name).await? {
            Some(dir) => list_fillers(storage, dir.handle).await?.is_empty(),
            None => true,
        }
    {
        // Below the target with no filler of ours *in the folder we were given*. The
        // refusal stands — `fill` manages the folder it was asked about, and going
        // hunting for another one to empty would be exactly the surprise this crate
        // avoids everywhere else. But look before answering, so the message can say
        // where the space went instead of implying it was never ours.
        let elsewhere = find_filler_folders(storage)
            .await?
            .into_iter()
            .filter(|f| f.name != dir_name)
            .collect();
        return Err(FillError::AlreadyBelowWindow {
            free: free_start,
            low: window.low(),
            elsewhere,
        });
    }

    // What the job set out to move, in whichever direction it has to go: `abs_diff`
    // because a fill that starts *below* the window climbs toward the aim by deleting
    // rather than descending toward it by writing.
    let total = free_start.abs_diff(window.aim());
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
                // Below the window: give space back rather than giving up. Only ever
                // filler this tool wrote, chosen by `next_removal`, and the loop then
                // re-measures exactly as it does after a write.
                let fillers = list_fillers(storage, dir).await?;
                let sizes: Vec<u64> = fillers.iter().map(|f| f.size).collect();
                match next_removal(free, window, &sizes) {
                    Removal::Remove(i) => {
                        let victim = &fillers[i];
                        storage.delete(victim.handle).await?;
                        on_event(Event::Deleted {
                            name: victim.filename.clone(),
                            bytes: victim.size,
                            kind: DeletedKind::Filler,
                        });
                        continue;
                    }
                    // Nothing of ours left to give back; the shortfall isn't ours.
                    Removal::Exhausted => {
                        on_event(Event::Finished { free });
                        return Ok(Outcome::Overfilled { free, excess });
                    }
                }
            }
            Step::Write(bytes) => {
                let name = format!("{FILL_PREFIX}{seq:04}{FILL_SUFFIX}");

                // Ground truth for everything already committed, taken from the device
                // rather than from a tally: how much closer to the aim we are than when
                // we started. In-object progress is added on top and then discarded —
                // the next iteration re-derives this from a fresh reading, so a drifting
                // estimate can never accumulate. Reduces to `free_start - free` for the
                // ordinary downward fill.
                let committed = total.saturating_sub(free.abs_diff(window.aim()));

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
        return Ok(CleanReport {
            free,
            removed: 0,
            bytes: 0,
        });
    };

    let (mut removed, mut bytes) = (0usize, 0u64);
    for filler in list_fillers(storage, dir.handle).await? {
        storage.delete(filler.handle).await?;
        removed += 1;
        bytes += filler.size;
        on_event(Event::Deleted {
            name: filler.filename,
            bytes: filler.size,
            kind: DeletedKind::Filler,
        });
    }

    if storage.list_objects(Some(dir.handle)).await?.is_empty() {
        storage.delete(dir.handle).await?;
    }

    let free = measure(storage).await?;
    on_event(Event::Finished { free });
    Ok(CleanReport {
        free,
        removed,
        bytes,
    })
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
        let existing = vec![
            obj("fill_0000.bin"),
            obj("fill_0007.bin"),
            obj("fill_0003.bin"),
        ];
        assert_eq!(next_sequence(&existing), 8);
    }

    #[test]
    fn sequence_ignores_names_this_tool_did_not_write() {
        let existing = vec![
            obj("mybook.azw3"),
            obj("fill_notanumber.bin"),
            obj("fill_.bin"),
        ];
        assert_eq!(next_sequence(&existing), 0);
    }

    /// The `fill_notes.bin` bug was two functions answering "is this ours?"
    /// differently, and the permissive half was the half that deletes.
    /// [`filler_sequence`] is now the single owner of that question — but the tests
    /// below pin its *behaviour*, and behaviour tests stay green while a second site
    /// quietly reintroduces the same divergence somewhere else in the file. This fails
    /// the build when that happens.
    ///
    /// Both spellings are counted: a copy written against the string literal would slip
    /// past a check that only knew the constant, which is the obvious way a guardrail
    /// like this gets walked around by accident.
    #[test]
    fn only_one_place_decides_whether_a_name_is_filler() {
        // Relative to the crate root, which is where cargo runs a test binary from,
        // whether it was invoked at the workspace root or in this directory.
        let src = std::fs::read_to_string("src/engine.rs").expect("read engine.rs");
        // Everything above `#[cfg(test)]`, because both needles appear verbatim in the
        // lines just below and counting those would leave this permanently red.
        let (module, _) = src
            .split_once("#[cfg(test)]")
            .expect("engine.rs has a test module");
        let sites = module.matches("strip_prefix(FILL_PREFIX)").count()
            + module.matches(r#"strip_prefix("fill_")"#).count();
        assert_eq!(
            sites, 1,
            "a second site is parsing filler names; extend filler_sequence instead"
        );
    }

    /// The property that matters for `clean`: a name is ours only if we would write
    /// exactly it. Everything rejected here is a file `clean` must leave alone, and
    /// `list_fillers` — the delete set — is filtered by this same function, so this
    /// is the guard against deleting someone's book.
    #[test]
    fn only_names_this_tool_would_write_are_recognized_as_filler() {
        for ours in [
            "fill_0000.bin",
            "fill_0001.bin",
            "fill_9999.bin",
            "fill_10000.bin",
        ] {
            assert_eq!(
                filler_sequence(ours).map(|n| format!("{FILL_PREFIX}{n:04}{FILL_SUFFIX}")),
                Some(ours.to_string()),
                "{ours} should round-trip as filler"
            );
        }
        for theirs in [
            "mybook.azw3",
            "fill_notes.bin", // the bug: prefix+suffix matched, so it was deletable
            "fill_backup.bin",
            "fill_.bin",
            "fill_12.bin", // right number, not a name we write
            "fill_00000007.bin",
            "fill_+001.bin",
            "fill_0001.bin.bak",
            "notfill_0001.bin",
        ] {
            assert_eq!(filler_sequence(theirs), None, "{theirs} must not be ours");
        }
    }
}
