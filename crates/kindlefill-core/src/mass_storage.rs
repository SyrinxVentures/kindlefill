//! Kindle USB mass-storage support for older models the OS mounts directly.
//!
//! Modern Kindles speak MTP; older ones appear as a volume named `Kindle` —
//! `/Volumes/Kindle` on macOS, a udisks2 mount point on Linux, a removable drive
//! letter with that volume label on Windows.  The two transports are deliberately
//! separate, but share the engine's filename and confirmation rules so cleanup is
//! equally narrow in either mode.

use crate::engine::{self, CleanReport, DeletedKind, Event, FillError, FillerFolder, Outcome};
use crate::plan::Window;
use crate::zeros::CHUNK;
#[cfg(unix)]
use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

const KINDLE_VOLUME: &str = "Kindle";

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

/// A Kindle exposed by the OS as a mounted USB mass-storage volume.
#[derive(Debug, Clone)]
pub struct MountedKindle {
    root: PathBuf,
}

impl MountedKindle {
    /// Find the default, OS-mounted Kindle volume without considering arbitrary
    /// removable drives.  The `documents` directory is a second signature everywhere:
    /// a volume merely named Kindle must not become a fill target.
    ///
    /// Where "named Kindle" lives differs per OS — a `/Volumes/Kindle` mount point on
    /// macOS, a udisks2 mount point on Linux, a removable drive's volume *label* on
    /// Windows — but the two-signature rule is the same on all three.
    #[cfg(target_os = "macos")]
    pub fn find() -> io::Result<Option<Self>> {
        Self::find_in(Path::new("/Volumes"))
    }

    /// See the macOS `find`. udisks2 mounts removable media under
    /// `/run/media/<user>/<label>` (Fedora, Arch) or `/media/<user>/<label>` (Debian,
    /// Ubuntu); both are checked. Untested against hardware — see the README's
    /// Platforms section.
    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn find() -> io::Result<Option<Self>> {
        let Some(user) = std::env::var_os("USER") else {
            return Ok(None);
        };
        for base in ["/run/media", "/media"] {
            let candidate = Path::new(base).join(&user);
            if let Some(kindle) = Self::find_in(&candidate)? {
                return Ok(Some(kindle));
            }
        }
        Ok(None)
    }

    /// See the macOS `find`. Windows has no path that encodes the volume name, so
    /// this walks the removable drive letters and matches the volume *label* against
    /// `Kindle` — the same first signature the mount-point platforms read from the
    /// path — then requires `documents` exactly as they do.
    #[cfg(windows)]
    pub fn find() -> io::Result<Option<Self>> {
        for root in windows_impl::removable_drives_labeled(KINDLE_VOLUME) {
            if root.join("documents").is_dir() {
                return Ok(Some(Self { root }));
            }
        }
        Ok(None)
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

    #[cfg(unix)]
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

    /// The Unix sibling above reports `f_bavail` — bytes available to the caller,
    /// not raw free blocks — so this reads `GetDiskFreeSpaceExW`'s caller-available
    /// figure rather than the volume-wide total, keeping the two measurements the
    /// same quantity.
    #[cfg(windows)]
    pub fn space(&self) -> io::Result<Space> {
        windows_impl::space(&self.root)
    }

    #[cfg(unix)]
    pub fn is_writable(&self) -> bool {
        let Ok(path) = CString::new(self.root.as_os_str().as_encoded_bytes()) else {
            return false;
        };
        // SAFETY: `path` is NUL-terminated. `access` does not modify the volume.
        unsafe { libc::access(path.as_ptr(), libc::W_OK) == 0 }
    }

    /// The Unix sibling asks `access(W_OK)`; Windows has no such per-caller probe
    /// for a FAT volume (there is no owner to check against), so the closest honest
    /// question is whether the *volume* is writable — its read-only flag, which is
    /// what a hardware write-protect switch or a `readonly` mount sets.
    #[cfg(windows)]
    pub fn is_writable(&self) -> bool {
        windows_impl::volume_is_writable(&self.root)
    }

    pub fn entries(&self, directory: &Path) -> io::Result<Vec<Entry>> {
        let mut entries = Vec::new();
        for child in fs::read_dir(directory)? {
            let child = child?;
            // `symlink_metadata`, not `metadata`: a symlink must be seen as itself
            // (and so never as a directory to recurse into or a filler to count),
            // matching how `purge_children` classifies below. An entry whose stat
            // fails — deleted between readdir and stat, or unreadable, as macOS's own
            // `.Trashes` is on non-FAT volumes — is skipped rather than failing the
            // whole listing: one bad entry must not make the device undetectable.
            let Ok(meta) = child.path().symlink_metadata() else {
                continue;
            };
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
            // The in-flight marker is ours, so it is not foreign — the MTP `list_foreign`
            // makes the same exclusion, and the two must agree or Fill would be held
            // behind an overwrite confirmation on one transport and not the other.
            .filter(|e| {
                e.is_dir
                    || (engine::filler_sequence(&e.name).is_none()
                        && !engine::is_inflight_marker(&e.name))
            })
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
            // A folder this scan can't read (a root-owned `.Trashes`, say — dot
            // names pass `validate_dir_name`) is treated as holding no filler rather
            // than failing the sweep: this is advisory breadth, and one unreadable
            // directory must not turn detect or a completed clean into an error.
            let Ok(fillers) = self.list_fillers(&entry.name) else {
                continue;
            };
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

    /// The fill itself is `engine::run_fill` — the same loop the MTP transport runs —
    /// with this volume's filesystem operations behind [`engine::FillStorage`]. The
    /// adapter's futures never await, so `block_on` drives them without a runtime.
    pub fn fill_with_cancel<F>(
        &self,
        window: Window,
        name: &str,
        cancel: &AtomicBool,
        on_event: F,
    ) -> Result<Outcome, FillError>
    where
        F: FnMut(Event) + Send,
    {
        engine::validate_dir_name(name)?;
        let mut target = MassFill { kindle: self, name };
        futures::executor::block_on(engine::run_fill(
            &mut target,
            window,
            || cancel.load(Ordering::Acquire),
            on_event,
        ))
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
        // Debris and marker together, never the marker alone — see the MTP `clean` for
        // why: the marker is what makes an unnameable file attributable to us, so
        // removing it while the file stays would strand that space for good.
        if dir.join(engine::INFLIGHT_MARKER).exists() {
            for debris in self.list_foreign(name).map_err(io_error)? {
                if debris.is_dir {
                    continue;
                }
                fs::remove_file(dir.join(&debris.name)).map_err(io_error)?;
                removed += 1;
                bytes += debris.bytes;
                on_event(Event::Deleted {
                    name: debris.name,
                    bytes: debris.bytes,
                    kind: DeletedKind::Interrupted,
                });
            }
            fs::remove_file(dir.join(engine::INFLIGHT_MARKER)).map_err(io_error)?;
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
    FillError::Io(error)
}

/// [`engine::FillStorage`] over a mounted volume, so [`engine::run_fill`] — the
/// single owner of the fill's behaviour — drives this transport too. Every method
/// body is synchronous; the `async` is only the shape the shared loop consumes.
struct MassFill<'a> {
    kindle: &'a MountedKindle,
    name: &'a str,
}

impl MassFill<'_> {
    fn dir(&self) -> PathBuf {
        self.kindle.root.join(self.name)
    }
}

impl engine::FillStorage for MassFill<'_> {
    /// The name alone identifies a filler on a filesystem; there is no handle.
    type FillerId = ();

    fn check_writable(&self) -> Result<(), FillError> {
        if self.kindle.is_writable() {
            Ok(())
        } else {
            Err(FillError::ReadOnly {
                description: self.kindle.root.display().to_string(),
            })
        }
    }

    async fn free(&mut self) -> Result<u64, FillError> {
        Ok(self.kindle.space().map_err(io_error)?.free)
    }

    async fn fillers(&mut self) -> Result<Vec<engine::FillerFile<()>>, FillError> {
        Ok(self
            .kindle
            .list_fillers(self.name)
            .map_err(io_error)?
            .into_iter()
            .map(|e| engine::FillerFile {
                name: e.name,
                bytes: e.bytes,
                id: (),
            })
            .collect())
    }

    async fn filler_elsewhere(&mut self) -> Result<Vec<FillerFolder>, FillError> {
        Ok(self
            .kindle
            .find_filler_folders()
            .map_err(io_error)?
            .into_iter()
            .filter(|f| f.name != self.name)
            .collect())
    }

    async fn ensure_dir(&mut self) -> Result<(), FillError> {
        self.kindle.ensure_fill_dir(self.name).map_err(io_error)?;
        Ok(())
    }

    async fn delete_filler(&mut self, filler: &engine::FillerFile<()>) -> Result<(), FillError> {
        fs::remove_file(self.dir().join(&filler.name)).map_err(io_error)
    }

    async fn write(
        &mut self,
        name: &str,
        bytes: u64,
        progress: &mut (dyn FnMut(u64) -> ControlFlow<()> + Send),
    ) -> Result<bool, FillError> {
        write_zeros(&self.dir().join(name), bytes, progress).map_err(io_error)
    }

    /// Implemented here too, though this transport is not the one that needed it.
    ///
    /// A filesystem write creates the file at its final path from the first byte, so a
    /// process killed mid-write leaves a short `fill_NNNN.bin` — a name `list_fillers`
    /// already recognises and `clean` already removes. There is no unnameable debris to
    /// reclaim. The marker is still written and cleared on this path so that a device
    /// used from both transports cannot end up with a stale marker that nothing ever
    /// takes off, and so that the two transports behave identically where they can.
    async fn mark_inflight(&mut self) -> Result<(), FillError> {
        let path = self.dir().join(engine::INFLIGHT_MARKER);
        if path.exists() {
            return Ok(());
        }
        fs::write(&path, b"kindlefill: a fill was underway in this folder\n").map_err(io_error)
    }

    async fn clear_inflight(&mut self) -> Result<(), FillError> {
        match fs::remove_file(self.dir().join(engine::INFLIGHT_MARKER)) {
            Ok(()) => Ok(()),
            // Already gone is the goal, not a failure.
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_error(e)),
        }
    }

    async fn take_interrupted(&mut self) -> Result<Vec<engine::FillerFile<()>>, FillError> {
        let dir = self.dir();
        if !dir.join(engine::INFLIGHT_MARKER).exists() {
            return Ok(Vec::new());
        }
        // The marker stays until `run_fill` has confirmed the debris went — same
        // contract as the MTP side, even though a filesystem delete does not lie.
        Ok(self
            .kindle
            .list_foreign(self.name)
            .map_err(io_error)?
            .into_iter()
            .filter(|e| !e.is_dir)
            .map(|e| engine::FillerFile {
                name: e.name,
                bytes: e.bytes,
                id: (),
            })
            .collect())
    }

    async fn names_in_dir(&mut self) -> Result<Vec<String>, FillError> {
        let dir = self.dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        Ok(self
            .kindle
            .entries(&dir)
            .map_err(io_error)?
            .into_iter()
            .map(|e| e.name)
            .collect())
    }
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
            // `is_file()`, not `!is_dir()`: the MTP purge classifies with `is_file`,
            // and a symlink or socket wearing a filler name is not a file we wrote.
            kind: if meta.is_file() && engine::filler_sequence(&name).is_some() {
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

/// Write `bytes` zeros to a fresh `path`, reporting every chunk through `progress`.
///
/// `progress` is called unthrottled — pacing and cancellation both live in
/// `run_fill`'s callback, the same as on the MTP transport. A `Break` from it (or an
/// error) removes the partial file: a leaked partial would consume space no `clean`
/// run could find. `Ok(false)` means cancelled.
fn write_zeros(
    path: &Path,
    bytes: u64,
    progress: &mut (dyn FnMut(u64) -> ControlFlow<()> + Send),
) -> io::Result<bool> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    // Heap, not `[0u8; CHUNK]` on the stack: 1 MiB is half of a worker thread's
    // default stack, and this function runs on whatever thread the app hands it.
    let zeros = vec![0u8; CHUNK];
    let mut written = 0;
    while written < bytes {
        if progress(written).is_break() {
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
    }
    // A `Break` here changes nothing — every byte is already written, so the file
    // is a complete filler either way; report it and finish.
    let _ = progress(written);
    file.sync_all()?;
    Ok(true)
}

/// The Win32 calls behind `MountedKindle`'s Windows arms, kept in one module so the
/// unsafe surface is a page rather than a scatter.
#[cfg(windows)]
mod windows_impl {
    use super::Space;
    use std::io;
    use std::path::{Path, PathBuf};
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
        GetVolumePathNameW,
    };

    /// `GetDriveTypeW` return value for removable media. Plain `u32` in the Win32
    /// metadata, so declared here rather than imported as a typed constant.
    const DRIVE_REMOVABLE: u32 = 2;
    /// `GetVolumeInformationW` filesystem flag: the volume rejects all writes.
    const FILE_READ_ONLY_VOLUME: u32 = 0x0008_0000;

    /// A root path as the volume APIs want it: wide, `\`-terminated, NUL-terminated.
    fn wide_root(root: &Path) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        let mut wide: Vec<u16> = root.as_os_str().encode_wide().collect();
        if wide.last() != Some(&u16::from(b'\\')) {
            wide.push(u16::from(b'\\'));
        }
        wide.push(0);
        wide
    }

    fn to_io(error: windows::core::Error) -> io::Error {
        // HRESULTs wrapping a Win32 error (facility 7) unwrap back to the OS code, so
        // callers classify `NotFound`/`PermissionDenied` exactly as they do on Unix.
        let code = error.code().0 as u32;
        if (code >> 16) & 0x7FF == 7 {
            io::Error::from_raw_os_error((code & 0xFFFF) as i32)
        } else {
            io::Error::other(error)
        }
    }

    /// Roots of removable drives whose volume label matches `label`.
    ///
    /// Removable only: a fixed partition someone labeled Kindle must not become a
    /// fill target, which is the same narrowness the macOS side gets from only ever
    /// looking under `/Volumes`. The label compare is case-insensitive because FAT
    /// stores labels uppercased as often as not, and Windows filesystems are
    /// case-insensitive everywhere else.
    pub(super) fn removable_drives_labeled(label: &str) -> Vec<PathBuf> {
        let mut out = Vec::new();
        // SAFETY: no pointers; returns a bitmask of present drive letters.
        let mask = unsafe { GetLogicalDrives() };
        for bit in 0..26u8 {
            if mask & (1 << bit) == 0 {
                continue;
            }
            let root = format!("{}:\\", char::from(b'A' + bit));
            let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
            // SAFETY: `wide` is a NUL-terminated root path that outlives both calls.
            if unsafe { GetDriveTypeW(PCWSTR(wide.as_ptr())) } != DRIVE_REMOVABLE {
                continue;
            }
            // 261: MAX_PATH + NUL, the documented upper bound for a volume name.
            let mut name = [0u16; 261];
            // SAFETY: `name` is a writable buffer; the API NUL-terminates into it.
            let ok = unsafe {
                GetVolumeInformationW(
                    PCWSTR(wide.as_ptr()),
                    Some(&mut name),
                    None,
                    None,
                    None,
                    None,
                )
            };
            if ok.is_err() {
                continue;
            }
            let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
            if String::from_utf16_lossy(&name[..len]).eq_ignore_ascii_case(label) {
                out.push(PathBuf::from(root));
            }
        }
        out
    }

    pub(super) fn space(root: &Path) -> io::Result<Space> {
        let wide = wide_root(root);
        let mut available = 0u64;
        let mut total = 0u64;
        // SAFETY: `wide` is NUL-terminated and the out-pointers reference live locals.
        unsafe {
            GetDiskFreeSpaceExW(
                PCWSTR(wide.as_ptr()),
                Some(&mut available),
                Some(&mut total),
                None,
            )
        }
        .map_err(to_io)?;
        Ok(Space {
            total,
            free: available,
        })
    }

    /// The mount point `path` sits on, NUL-terminated, as `GetVolumeInformationW`
    /// demands.
    ///
    /// That call takes a *root* and nothing else: `E:\` answers, `E:\somewhere` fails
    /// with `ERROR_DIR_NOT_ROOT`. `find` only ever produces drive roots, so reading the
    /// flags off the path directly worked there and nowhere else — `find_in` hands back
    /// a subdirectory, and every volume then looked unwritable. `GetVolumePathNameW`
    /// answers for any path, and for a volume mounted into a folder rather than a
    /// letter it returns that folder, which is the correct root to ask about anyway.
    fn volume_root(path: &Path) -> Option<Vec<u16>> {
        let wide = wide_root(path);
        let mut buf = [0u16; 261];
        // SAFETY: `wide` is NUL-terminated; the API NUL-terminates into `buf`.
        unsafe { GetVolumePathNameW(PCWSTR(wide.as_ptr()), &mut buf) }.ok()?;
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(
            buf[..len]
                .iter()
                .copied()
                .chain(std::iter::once(0))
                .collect(),
        )
    }

    pub(super) fn volume_is_writable(root: &Path) -> bool {
        let Some(wide) = volume_root(root) else {
            return false;
        };
        let mut flags = 0u32;
        // SAFETY: `wide` is NUL-terminated and `flags` is a live local.
        let ok = unsafe {
            GetVolumeInformationW(
                PCWSTR(wide.as_ptr()),
                None,
                None,
                None,
                Some(&mut flags),
                None,
            )
        };
        // An unanswerable volume is reported unwritable rather than writable — the
        // same pessimism `access` failing produces on the Unix side.
        ok.is_ok() && flags & FILE_READ_ONLY_VOLUME == 0
    }
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

    /// The mass transport reaches `engine::run_fill` through its adapter: a cancel
    /// raised before the first step must come back as `Cancelled` — the loop's
    /// answer, not an error — and must leave no filler behind. (The window bounds
    /// are irrelevant here; the tempdir sits on a volume with plenty of free space,
    /// so the loop reaches its cancel check rather than refusing below-window.)
    #[test]
    fn a_fill_cancelled_before_the_first_step_writes_nothing() {
        let (_tmp, kindle) = mounted();
        let cancel = AtomicBool::new(true);
        let window = Window::new(50 * 1024 * 1024, 90 * 1024 * 1024).unwrap();
        let outcome = kindle
            .fill_with_cancel(window, "fill_disk", &cancel, |_| {})
            .unwrap();
        assert!(matches!(outcome, Outcome::Cancelled { .. }));
        let dir = kindle.root.join("fill_disk");
        assert!(
            !dir.exists() || fs::read_dir(&dir).unwrap().next().is_none(),
            "a cancelled fill left files behind"
        );
    }
}
