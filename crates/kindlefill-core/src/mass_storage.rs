//! Kindle USB mass-storage support for older models that Finder mounts directly.
//!
//! Modern Kindles speak MTP; older ones appear as `/Volumes/Kindle`.  The two
//! transports are deliberately separate, but share the engine's filename and
//! confirmation rules so cleanup is equally narrow in either mode.

use crate::engine::{self, CleanReport, DeletedKind, Event, FillError, FillerFolder, Outcome};
use crate::plan::{next_removal, next_step, Removal, Step, Window};
use crate::rate::{FillProgress, RateEstimator};
use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const VOLUMES: &str = "/Volumes";
const KINDLE_VOLUME: &str = "Kindle";
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
const CHUNK: usize = 1024 * 1024;

/// A root-level entry on a Kindle mounted by macOS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub bytes: u64,
    pub is_dir: bool,
}

/// Capacity measured from the mounted filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Space {
    pub total: u64,
    pub free: u64,
}

/// A Kindle exposed by macOS as a USB mass-storage volume.
#[derive(Debug, Clone)]
pub struct MountedKindle {
    root: PathBuf,
}

impl MountedKindle {
    /// Find the default, Finder-mounted Kindle volume without considering arbitrary
    /// removable drives.  The `documents` directory is a second signature: a volume
    /// merely named Kindle must not become a fill target.
    pub fn find() -> io::Result<Option<Self>> {
        Self::find_in(Path::new(VOLUMES))
    }

    pub fn find_in(volumes: &Path) -> io::Result<Option<Self>> {
        let root = volumes.join(KINDLE_VOLUME);
        if root.is_dir() && root.join("documents").is_dir() {
            Ok(Some(Self { root }))
        } else {
            Ok(None)
        }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn space(&self) -> io::Result<Space> {
        let path = CString::new(self.root.as_os_str().as_encoded_bytes())
            .expect("mounted path contains no NUL bytes");
        let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: `path` is NUL-terminated and `stats` points to writable storage.
        if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a zero return from statvfs initializes `stats`.
        let stats = unsafe { stats.assume_init() };
        let block = stats.f_frsize;
        Ok(Space {
            total: (stats.f_blocks as u64).saturating_mul(block),
            free: (stats.f_bavail as u64).saturating_mul(block),
        })
    }

    pub fn is_writable(&self) -> bool {
        let Ok(path) = CString::new(self.root.as_os_str().as_encoded_bytes()) else {
            return false;
        };
        // SAFETY: `path` is NUL-terminated. `access` does not modify the volume.
        unsafe { libc::access(path.as_ptr(), libc::W_OK) == 0 }
    }

    pub fn entries(&self, directory: &Path) -> io::Result<Vec<Entry>> {
        let mut entries = Vec::new();
        for child in fs::read_dir(directory)? {
            let child = child?;
            let meta = child.metadata()?;
            let name = child.file_name().to_string_lossy().into_owned();
            entries.push(Entry {
                name,
                bytes: meta.len(),
                is_dir: meta.is_dir(),
            });
        }
        Ok(entries)
    }

    fn named_root_dir(&self, name: &str) -> io::Result<Option<PathBuf>> {
        engine::validate_dir_name(name).map_err(io::Error::other)?;
        let path = self.root.join(name);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => Err(io::Error::other(
                "the filler folder is a symlink; refusing to follow it",
            )),
            Ok(meta) if meta.is_dir() => Ok(Some(path)),
            Ok(_) => Ok(None),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn find_fill_dir(&self, name: &str) -> io::Result<Option<Entry>> {
        let Some(path) = self.named_root_dir(name)? else {
            return Ok(None);
        };
        Ok(Some(Entry {
            name: name.to_string(),
            bytes: fs::metadata(path)?.len(),
            is_dir: true,
        }))
    }

    fn ensure_fill_dir(&self, name: &str) -> io::Result<PathBuf> {
        if let Some(path) = self.named_root_dir(name)? {
            return Ok(path);
        }
        let path = self.root.join(name);
        fs::create_dir(&path)?;
        Ok(path)
    }

    pub fn list_fillers(&self, name: &str) -> io::Result<Vec<Entry>> {
        let Some(path) = self.named_root_dir(name)? else {
            return Ok(Vec::new());
        };
        let mut entries: Vec<_> = self
            .entries(&path)?
            .into_iter()
            .filter(|e| !e.is_dir && engine::filler_sequence(&e.name).is_some())
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    pub fn list_foreign(&self, name: &str) -> io::Result<Vec<Entry>> {
        let Some(path) = self.named_root_dir(name)? else {
            return Ok(Vec::new());
        };
        Ok(self
            .entries(&path)?
            .into_iter()
            .filter(|e| e.is_dir || engine::filler_sequence(&e.name).is_none())
            .collect())
    }

    pub fn overwrite_token(&self, name: &str) -> io::Result<Option<String>> {
        let foreign = self.list_foreign(name)?;
        Ok(engine::overwrite_token_for_entries(
            name,
            foreign.iter().map(|e| (e.name.as_str(), e.bytes)),
        ))
    }

    pub fn list_staged_updates(&self) -> io::Result<Vec<Entry>> {
        let mut updates: Vec<_> = self
            .entries(&self.root)?
            .into_iter()
            .filter(|e| !e.is_dir && engine::is_staged_update(&e.name))
            .collect();
        updates.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(updates)
    }

    pub fn find_filler_folders(&self) -> io::Result<Vec<FillerFolder>> {
        let mut folders = Vec::new();
        for entry in self.entries(&self.root)? {
            if !entry.is_dir || engine::validate_dir_name(&entry.name).is_err() {
                continue;
            }
            let fillers = self.list_fillers(&entry.name)?;
            if !fillers.is_empty() {
                folders.push(FillerFolder {
                    name: entry.name,
                    files: fillers.len(),
                    bytes: fillers.iter().map(|e| e.bytes).sum(),
                });
            }
        }
        folders.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.name.cmp(&b.name)));
        Ok(folders)
    }

    pub fn delete_staged_updates<F>(
        &self,
        names: &[String],
        mut on_event: F,
    ) -> io::Result<Vec<Entry>>
    where
        F: FnMut(Event),
    {
        let doomed: Vec<_> = self
            .list_staged_updates()?
            .into_iter()
            .filter(|e| names.iter().any(|name| name == &e.name))
            .collect();
        for entry in &doomed {
            fs::remove_file(self.root.join(&entry.name))?;
            on_event(Event::Deleted {
                name: entry.name.clone(),
                bytes: entry.bytes,
                kind: DeletedKind::Update,
            });
        }
        on_event(Event::Finished {
            free: self.space()?.free,
        });
        Ok(doomed)
    }

    pub fn purge_fill_dir_confirmed<F>(
        &self,
        name: &str,
        confirmed: &str,
        on_event: F,
    ) -> Result<usize, FillError>
    where
        F: FnMut(Event),
    {
        if self.overwrite_token(name).map_err(io_error)?.as_deref() != Some(confirmed) {
            return Err(FillError::StaleConfirmation);
        }
        self.purge_fill_dir(name, on_event)
    }

    pub fn purge_fill_dir<F>(&self, name: &str, mut on_event: F) -> Result<usize, FillError>
    where
        F: FnMut(Event),
    {
        engine::validate_dir_name(name)?;
        let Some(path) = self.named_root_dir(name).map_err(io_error)? else {
            return Ok(0);
        };
        let removed = purge_children(&path, &mut on_event).map_err(io_error)?;
        on_event(Event::Finished {
            free: self.space().map_err(io_error)?.free,
        });
        Ok(removed)
    }

    pub fn fill_with_cancel<F>(
        &self,
        window: Window,
        name: &str,
        cancel: &AtomicBool,
        mut on_event: F,
    ) -> Result<Outcome, FillError>
    where
        F: FnMut(Event),
    {
        engine::validate_dir_name(name)?;
        if !self.is_writable() {
            return Err(FillError::ReadOnly {
                description: self.root.display().to_string(),
            });
        }
        let start = self.space().map_err(io_error)?.free;
        if start < window.low() && self.list_fillers(name).map_err(io_error)?.is_empty() {
            let elsewhere = self
                .find_filler_folders()
                .map_err(io_error)?
                .into_iter()
                .filter(|f| f.name != name)
                .collect();
            return Err(FillError::AlreadyBelowWindow {
                free: start,
                low: window.low(),
                elsewhere,
            });
        }
        let total = start.abs_diff(window.aim());
        on_event(Event::Started {
            free: start,
            aim: window.aim(),
            total,
        });
        let dir = self.ensure_fill_dir(name).map_err(io_error)?;
        let mut seq = self
            .list_fillers(name)
            .map_err(io_error)?
            .iter()
            .filter_map(|e| engine::filler_sequence(&e.name))
            .max()
            .map_or(0, |n| n + 1);
        let mut rate = RateEstimator::new();
        let job_start = Instant::now();

        loop {
            let free = self.space().map_err(io_error)?.free;
            if cancel.load(Ordering::Acquire) {
                on_event(Event::Finished { free });
                return Ok(Outcome::Cancelled { free });
            }
            match next_step(free, window) {
                Step::Done => {
                    on_event(Event::Finished { free });
                    return Ok(Outcome::InWindow { free });
                }
                Step::Overfilled { free, excess } => {
                    let fillers = self.list_fillers(name).map_err(io_error)?;
                    let sizes: Vec<_> = fillers.iter().map(|e| e.bytes).collect();
                    match next_removal(free, window, &sizes) {
                        Removal::Remove(index) => {
                            let victim = &fillers[index];
                            fs::remove_file(dir.join(&victim.name)).map_err(io_error)?;
                            on_event(Event::Deleted {
                                name: victim.name.clone(),
                                bytes: victim.bytes,
                                kind: DeletedKind::Filler,
                            });
                        }
                        Removal::Exhausted => {
                            on_event(Event::Finished { free });
                            return Ok(Outcome::Overfilled { free, excess });
                        }
                    }
                }
                Step::Write(bytes) => {
                    let file_name = format!("fill_{seq:04}.bin");
                    let path = dir.join(&file_name);
                    let committed = total.saturating_sub(free.abs_diff(window.aim()));
                    let written = write_zeros(&path, bytes, cancel, |done| {
                        let now = Instant::now();
                        rate.observe(
                            committed.saturating_add(done),
                            now.duration_since(job_start),
                        );
                        on_event(Event::Progress(FillProgress {
                            done: committed.saturating_add(done),
                            total,
                            rate: rate.rate(),
                            eta: rate.eta(total.saturating_sub(committed.saturating_add(done))),
                        }));
                    })
                    .map_err(io_error)?;
                    if !written {
                        let free = self.space().map_err(io_error)?.free;
                        on_event(Event::Finished { free });
                        return Ok(Outcome::Cancelled { free });
                    }
                    seq += 1;
                    on_event(Event::Wrote {
                        name: file_name,
                        bytes,
                        free: self.space().map_err(io_error)?.free,
                    });
                }
            }
        }
    }

    pub fn clean<F>(&self, name: &str, mut on_event: F) -> Result<CleanReport, FillError>
    where
        F: FnMut(Event),
    {
        engine::validate_dir_name(name)?;
        let Some(dir) = self.named_root_dir(name).map_err(io_error)? else {
            let free = self.space().map_err(io_error)?.free;
            on_event(Event::Finished { free });
            return Ok(CleanReport {
                free,
                removed: 0,
                bytes: 0,
            });
        };
        let mut removed = 0;
        let mut bytes = 0;
        for filler in self.list_fillers(name).map_err(io_error)? {
            fs::remove_file(dir.join(&filler.name)).map_err(io_error)?;
            removed += 1;
            bytes += filler.bytes;
            on_event(Event::Deleted {
                name: filler.name,
                bytes: filler.bytes,
                kind: DeletedKind::Filler,
            });
        }
        if self.entries(&dir).map_err(io_error)?.is_empty() {
            fs::remove_dir(dir).map_err(io_error)?;
        }
        let free = self.space().map_err(io_error)?.free;
        on_event(Event::Finished { free });
        Ok(CleanReport {
            free,
            removed,
            bytes,
        })
    }
}

fn io_error(error: io::Error) -> FillError {
    FillError::Mtp(mtp_rs::Error::Other {
        detail: error.to_string(),
    })
}

fn purge_children<F>(path: &Path, on_event: &mut F) -> io::Result<usize>
where
    F: FnMut(Event),
{
    let mut removed = 0;
    for child in fs::read_dir(path)? {
        let child = child?;
        let meta = fs::symlink_metadata(child.path())?;
        let name = child.file_name().to_string_lossy().into_owned();
        let bytes = meta.len();
        if meta.is_dir() {
            removed += purge_children(&child.path(), on_event)?;
            fs::remove_dir(child.path())?;
        } else {
            fs::remove_file(child.path())?;
        }
        on_event(Event::Deleted {
            kind: if !meta.is_dir() && engine::filler_sequence(&name).is_some() {
                DeletedKind::Filler
            } else {
                DeletedKind::Foreign
            },
            name,
            bytes,
        });
        removed += 1;
    }
    Ok(removed)
}

fn write_zeros<F>(path: &Path, bytes: u64, cancel: &AtomicBool, mut progress: F) -> io::Result<bool>
where
    F: FnMut(u64),
{
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let zeros = [0u8; CHUNK];
    let mut written = 0;
    let mut last_emit: Option<Instant> = None;
    while written < bytes {
        if cancel.load(Ordering::Acquire) {
            drop(file);
            fs::remove_file(path)?;
            return Ok(false);
        }
        let size = usize::try_from((bytes - written).min(CHUNK as u64)).expect("chunk fits usize");
        if let Err(error) = file.write_all(&zeros[..size]) {
            drop(file);
            let _ = fs::remove_file(path);
            return Err(error);
        }
        written += size as u64;
        let now = Instant::now();
        if last_emit.is_none_or(|last| now.duration_since(last) >= PROGRESS_INTERVAL)
            || written == bytes
        {
            last_emit = Some(now);
            progress(written);
        }
    }
    file.sync_all()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mounted() -> (tempfile::TempDir, MountedKindle) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(KINDLE_VOLUME);
        fs::create_dir_all(root.join("documents")).unwrap();
        (tmp, MountedKindle { root })
    }

    #[test]
    fn only_the_default_kindle_volume_with_documents_is_detected() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(MountedKindle::find_in(tmp.path()).unwrap().is_none());
        fs::create_dir(tmp.path().join(KINDLE_VOLUME)).unwrap();
        assert!(MountedKindle::find_in(tmp.path()).unwrap().is_none());
        fs::create_dir(tmp.path().join(KINDLE_VOLUME).join("documents")).unwrap();
        assert!(MountedKindle::find_in(tmp.path()).unwrap().is_some());
    }

    #[test]
    fn clean_removes_only_exact_filler_names() {
        let (_tmp, kindle) = mounted();
        let dir = kindle.root.join("fill_disk");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("fill_0000.bin"), [0; 4]).unwrap();
        fs::write(dir.join("fill_notes.bin"), [0; 4]).unwrap();
        let report = kindle.clean("fill_disk", |_| {}).unwrap();
        assert_eq!(report.removed, 1);
        assert!(!dir.join("fill_0000.bin").exists());
        assert!(dir.join("fill_notes.bin").exists());
    }
}
