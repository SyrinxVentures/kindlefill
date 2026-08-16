//! Desktop UI for kindlefill.
//!
//! Thin by design: every decision lives in `kindlefill-core`, and this crate only
//! opens the device, forwards engine events to the webview, and owns the cancel token
//! so the Stop button has something to pull.
//!
//! The device is opened per operation rather than held across commands. Opening is
//! cheap next to a multi-minute transfer, and a long-lived handle would go stale the
//! moment someone unplugs the cable — which they will.

// Release builds must not pop a console window behind the app on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use kindlefill_core::{
    engine, human_bytes, human_eta, is_disconnected, is_exclusive_access, is_permission_denied,
    is_timeout, mass_storage::MountedKindle, ptpcamerad, Event, Window,
};
use mtp_rs::{CancelToken, MtpDevice, Storage};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tauri::{AppHandle, Emitter, Manager, State};

/// Events the webview listens for.
const EV_PROGRESS: &str = "kindlefill://progress";
const EV_LOG: &str = "kindlefill://log";
const EV_DEVICE: &str = "kindlefill://device";

#[derive(Default)]
struct AppState {
    /// Set for as long as any device operation is running.
    ///
    /// The device is opened per operation, so two commands in flight means two
    /// [`open_device`] calls: the second fails for exclusive access, and the frontend
    /// unwinding *that* failure is what strands the first — its Stop button goes away
    /// while its transfer keeps running. The webview is supposed to prevent this, but
    /// it is a webview, and a seventeen-minute write that cannot be stopped is too
    /// expensive to leave resting on that alone.
    busy: AtomicBool,
    /// The running fill's cancel flag, held as the `Arc` the token wraps rather than
    /// as the token: a finishing fill has to prove the entry is its own before
    /// clearing it, [`CancelToken`] has no equality, and two tokens over one `Arc` are
    /// one flag — which [`Arc::ptr_eq`] can say.
    cancel: Mutex<Option<Arc<AtomicBool>>>,
}

/// Holds the single-operation slot until the operation ends, however it ends.
///
/// Each command has several `?` exits and releasing before every one of them is how a
/// release gets forgotten — after which the app refuses everything until it is
/// restarted, which is a worse bug than the one being fixed.
struct OpGuard<'a>(&'a AtomicBool);

impl Drop for OpGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Claim the slot, or refuse — in one indivisible step.
///
/// Compare-exchange rather than read-then-write: two commands can both read `false`
/// before either writes `true`, and an RAII guard doesn't help with that because it is
/// the *acquire* that has to be atomic.
fn claim(state: &AppState) -> Result<OpGuard<'_>, String> {
    state
        .busy
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .map(|_| OpGuard(&state.busy))
        .map_err(|_| "Another operation is already running.".to_string())
}

/// One object on the device, named for a person to recognize.
///
/// Names are carried to the UI so nothing is ever deleted from behind a count. "3
/// files" tells you nothing about whether one of them is yours; `fill_0002.bin` does.
#[derive(Serialize, Clone)]
struct NamedObject {
    name: String,
    bytes: u64,
    human: String,
}

impl From<&mtp_rs::ObjectInfo> for NamedObject {
    fn from(o: &mtp_rs::ObjectInfo) -> Self {
        NamedObject {
            name: o.filename.clone(),
            bytes: o.size,
            human: human_bytes(o.size),
        }
    }
}

#[derive(Serialize, Clone)]
struct DeviceSnapshot {
    /// What the device calls itself. Was the literal string "Kindle" for every MTP
    /// device regardless of what answered, which on Windows meant a flash drive could
    /// be introduced as a Kindle in the one place a user would look to notice it
    /// wasn't.
    model: String,
    /// Whether [`Identity`] recognised the device. `false` puts a warning and an
    /// opt-in tick in front of Fill; it does not disable anything else, because
    /// reading a device is not what needed guarding.
    is_kindle: bool,
    storage: String,
    total: u64,
    free: u64,
    total_human: String,
    free_human: String,
    writable: bool,
    /// The folder filler goes in, reported back so the UI can name it — someone who
    /// wants to undo this by hand needs to know what to look for.
    fill_dir: String,
    fill_dir_exists: bool,
    filler_files: usize,
    filler_bytes: u64,
    filler_human: String,
    /// Things in the filler folder that this tool did not write. `clean` leaves these
    /// alone, so their presence means the folder will survive removal — and it may
    /// mean the folder belongs to something else entirely.
    foreign: Vec<NamedObject>,
    /// Identifies the `foreign` set above. The webview hands this back with a fill
    /// that asks to overwrite, and the backend refuses if the folder no longer matches
    /// — which is what ties the tick the user gave to the files they were shown.
    /// `None` when nothing foreign is there and no confirmation is needed.
    overwrite_token: Option<String>,
    /// Staged OTA firmware images at the storage root.
    updates: Vec<NamedObject>,
    /// Filler found under a *different* folder name. Nothing persists the name
    /// between launches, so this is how a relaunch finds filler it wrote yesterday
    /// instead of reporting an empty default and offering to delete nothing.
    elsewhere: Vec<FillerFolderInfo>,
}

#[derive(Serialize, Clone)]
struct FillerFolderInfo {
    name: String,
    files: usize,
    bytes: u64,
    human: String,
}

#[derive(Serialize, Clone)]
struct ProgressPayload {
    done: u64,
    total: u64,
    fraction: f64,
    percent: f64,
    done_human: String,
    total_human: String,
    /// Bytes per second, unformatted, so the pre-flight estimate can learn this
    /// device's real speed instead of quoting a constant measured on someone else's.
    rate: Option<f64>,
    rate_human: String,
    eta_human: String,
}

#[derive(Serialize, Clone)]
struct LogPayload {
    line: String,
}

/// A live correction to the device panel, sent as work happens.
///
/// The panel is otherwise painted from `detect`, which opens the device — and the
/// device is held for the length of a fill, so during the seventeen minutes that
/// matter most it cannot run. The result was a header confidently reading "not
/// present yet" and "none" while the log below it listed fourteen written files.
///
/// Every figure needed is already in the engine's event stream, so this carries it
/// rather than asking the device again: `free` is the value the engine just re-read,
/// and the deltas let the UI keep its own tally without knowing how many files were
/// there before it started.
#[derive(Serialize, Clone)]
struct DeviceUpdate {
    /// Absent for events that don't re-measure — `Deleted` reports bytes, not free
    /// space, and inventing a figure there would be worse than leaving the last one.
    free: Option<u64>,
    free_human: Option<String>,
    files_delta: i32,
    bytes_delta: i64,
}

/// Turn an MTP failure into something a person can act on.
///
/// The raw errors are useless to a user: "could not be opened for exclusive access"
/// gives no hint that the fix is to quit a background daemon. These two cases point in
/// opposite directions — close another program, versus grant this one permission — so
/// they're kept distinct rather than collapsed into one "device error".
fn explain(error: &mtp_rs::Error) -> String {
    // `cfg!` rather than `#[cfg]` arms: every platform's wording stays compiled and
    // type-checked from every platform, which matters in a repo whose Windows side
    // can only be cross-checked. The strings differ because the fixes do — naming
    // ptpcamerad to a Windows user sends them hunting for a daemon they don't have.
    if is_exclusive_access(error) {
        return if cfg!(windows) {
            // WPD is multi-client, so this is rare on Windows — it means something
            // claimed the device outside WPD (a WinUSB/libusb tool, a VM grabbing
            // the USB device), not the usual sync program.
            "Another program has claimed the Kindle's USB device. Close any \
             virtualization software or USB tools that may have captured it, then \
             unplug/replug the cable."
                .to_string()
        } else {
            "Another program is holding the Kindle. On macOS this is usually \
             ptpcamerad, Apple's camera daemon; Android File Transfer and OpenMTP \
             do it too. Quit those and unplug/replug the cable."
                .to_string()
        };
    }
    if is_permission_denied(error) {
        return if cfg!(windows) {
            format!(
                "Windows refused access to the device (nothing else is holding it). \
                 Unlock the Kindle's screen and replug the cable; if that doesn't \
                 clear it, check Device Manager — the Kindle should appear under \
                 Portable Devices. Underlying error: {error}"
            )
        } else {
            format!(
                "The system refused access to the device (nothing else is holding \
                 it). On Linux this usually means missing udev rules. Underlying \
                 error: {error}"
            )
        };
    }
    if is_timeout(error) {
        return "The Kindle stopped responding. Its MTP session can end up wedged \
                after an interrupted transfer — often it still answers reads while \
                refusing every write, so the device looks fine here but nothing can \
                be written. Unplug and replug the cable; if that doesn't clear it, \
                restart the Kindle (hold the power button, then Restart)."
            .to_string();
    }
    if is_disconnected(error) {
        return "The Kindle disconnected. Replug the cable and press Refresh. \
                Anything already written is intact — Fill again resumes from it."
            .to_string();
    }
    format!("{error}")
}

/// [`explain`] for the mass-storage transport, where failures are plain filesystem
/// errors rather than MTP ones. Same job, sibling function: the conditions worth
/// translating differ by transport (there is no ptpcamerad fight over a mounted
/// volume, but there is an ejected one), so the two keep separate vocabularies.
fn explain_io(error: &std::io::Error) -> String {
    // Same `cfg!` reasoning as `explain`: all wordings compile everywhere, and the
    // file-manager and permission vocabulary is the part that must match the OS.
    match error.kind() {
        std::io::ErrorKind::NotFound => if cfg!(windows) {
            "The Kindle's drive disappeared — it was ejected or unplugged. Replug \
             the cable, wait for it to appear in File Explorer, and press Refresh. \
             Anything already written is intact — Fill again resumes from it."
        } else {
            "The Kindle's volume disappeared — it was ejected or unplugged. Replug \
             the cable, wait for it to mount in Finder, and press Refresh. Anything \
             already written is intact — Fill again resumes from it."
        }
        .to_string(),
        std::io::ErrorKind::PermissionDenied => if cfg!(windows) {
            "Windows refused access to the Kindle's drive. Check that the drive \
             isn't write-protected and that nothing else has its files open, then \
             try again."
        } else {
            "macOS refused access to the Kindle's volume. If this is the first run, \
             grant the app access to removable volumes in System Settings → Privacy \
             & Security → Files and Folders, then try again."
        }
        .to_string(),
        _ => format!("{error}"),
    }
}

/// [`explain`] for the engine's own error type.
///
/// A fill or clean that fails because something else holds the device deserves the same
/// guidance as a failed open — it's the identical problem, and routing it through
/// `to_string()` instead would print the raw MTP error for one path and the helpful
/// text for the other.
fn explain_fill(error: &engine::FillError) -> String {
    match error {
        engine::FillError::Mtp(e) => explain(e),
        engine::FillError::Io(e) => explain_io(e),
        other => other.to_string(),
    }
}

/// An open Kindle, plus the things that have to stay alive alongside it.
///
/// The device handle must outlive the operation — dropping it closes the MTP session —
/// and so must the `ptpcamerad` tamer, which keeps Apple's camera daemon from
/// re-claiming the interface partway through a 17-minute transfer. Bundling them means
/// a caller can't hold one and drop the other.
enum Session {
    Mtp {
        _device: MtpDevice,
        _tamer: ptpcamerad::Tamer,
        storage: Storage,
    },
    Mass(MountedKindle),
}

/// What the open device says it is, and whether that reads as a Kindle.
///
/// One type derived in one place, because `detect` and `start_fill` open the device
/// separately and must not be able to reach different answers: the panel says "not a
/// recognised Kindle" and puts a checkbox in front of Fill, and the backend then
/// re-derives the same verdict before writing. A UI-only check would be no check at
/// all — the webview is where the tick comes from.
struct Identity {
    /// For the header. The device's own words when it has any, so a wrong device is
    /// recognisable *as* wrong rather than being labelled "Kindle" regardless.
    label: String,
    is_kindle: bool,
}

impl Session {
    fn identity(&self) -> Identity {
        match self {
            Session::Mtp { _device, .. } => {
                let info = _device.device_info();
                let (manufacturer, model) = (info.manufacturer.trim(), info.model.trim());
                // Both fields, trimmed and joined, minus whichever is empty: this Oasis
                // reports "Kindle" / "Internal Storage" and reads best as both, while a
                // device that fills only one field should not gain a stray space.
                let label = match (manufacturer.is_empty(), model.is_empty()) {
                    (true, true) => "Unknown device".to_string(),
                    (true, false) => model.to_string(),
                    (false, true) => manufacturer.to_string(),
                    (false, false) => format!("{manufacturer} {model}"),
                };
                Identity {
                    is_kindle: kindlefill_core::looks_like_kindle(manufacturer, model),
                    label,
                }
            }
            // Already established by the transport itself, which mounts nothing that
            // isn't a volume named `Kindle` holding a `documents` folder.
            Session::Mass(_) => Identity {
                label: "Kindle".to_string(),
                is_kindle: true,
            },
        }
    }
}

/// Open the Kindle and its writable storage.
///
/// The daemon is tamed for the whole session rather than only at open time: launchd
/// restarts `ptpcamerad` within a second of each kill and it re-claims the device when
/// it does, so a single kill before opening would not survive the transfer. This is the
/// same [`ptpcamerad::Tamer`] the CLI uses — the module used to live inside the CLI
/// crate, which meant the app could not do this at all despite the README saying the
/// tool did.
/// Is an MTP-capable device on the bus, by the same evidence `open_first` acts on?
///
/// Two probes because Windows needs both: `list_devices` enumerates over USB only,
/// and a WPD-bound Kindle is invisible to it — its vendor-class descriptors match
/// MTP only by an endpoint check that requires the open the WPD driver forbids. The
/// WPD side asks the same `GetDevices` question the WPD open path acts on, so this
/// answer and "would `open_first` find something" cannot disagree. One function
/// rather than two call sites repeating the pair: `device_present` painting the
/// header and `open_device` choosing a transport must make the identical decision,
/// or a mounted volume shadows a live Kindle for one of them.
fn mtp_on_bus() -> bool {
    MtpDevice::list_devices().is_ok_and(|d| !d.is_empty()) || kindlefill_core::wpd::device_present()
}

async fn open_device() -> Result<Session, String> {
    // A device actually on the USB bus outranks a mounted volume: `/Volumes/Kindle`
    // with a `documents` folder is also the signature of a backup disk or mounted
    // disk image, and letting it shadow a real MTP Kindle would aim every operation —
    // including confirmed deletions — at the wrong storage. The volume is used only
    // when nothing answers on MTP.
    let mtp_present = mtp_on_bus();
    if !mtp_present {
        if let Some(kindle) = MountedKindle::find().map_err(|e| explain_io(&e))? {
            return Ok(Session::Mass(kindle));
        }
    }
    let tamer = ptpcamerad::Tamer::start();
    let device = MtpDevice::open_first().await.map_err(|e| explain(&e))?;
    let storages = device.storages().await.map_err(|e| explain(&e))?;
    let storage = storages
        .into_iter()
        .find(|s| s.info().is_writable)
        .ok_or_else(|| "The device has no writable storage.".to_string())?;
    Ok(Session::Mtp {
        _device: device,
        _tamer: tamer,
        storage,
    })
}

fn log(app: &AppHandle, line: impl Into<String>) {
    let _ = app.emit(EV_LOG, LogPayload { line: line.into() });
}

/// Mirror the event stream to stderr when `KINDLEFILL_TRACE` is set.
///
/// Everything the UI shows arrives as an [`Event`], so this is the only place from
/// which the whole stream — including the in-upload `Progress` cadence — can be
/// observed without watching the window. That makes "the bar doesn't freeze" and
/// "Stop responds in about a second" measurable rather than impressions: the
/// timestamps are the evidence.
fn trace(event: &Event) {
    use std::sync::OnceLock;
    static START: OnceLock<Instant> = OnceLock::new();
    if std::env::var_os("KINDLEFILL_TRACE").is_none() {
        return;
    }
    let t = START.get_or_init(Instant::now).elapsed().as_secs_f64();
    match event {
        Event::Progress(p) => eprintln!(
            "[{t:8.3}] Progress {:.1}% {}/{}",
            p.percent(),
            human_bytes(p.done),
            human_bytes(p.total)
        ),
        other => eprintln!("[{t:8.3}] {other:?}"),
    }
}

/// Push a live correction to the device panel.
fn device_update(app: &AppHandle, free: Option<u64>, files_delta: i32, bytes_delta: i64) {
    let _ = app.emit(
        EV_DEVICE,
        DeviceUpdate {
            free,
            free_human: free.map(human_bytes),
            files_delta,
            bytes_delta,
        },
    );
}

/// Forward one engine event to the webview.
fn forward(app: &AppHandle, event: Event) {
    trace(&event);
    match &event {
        Event::Started { free, .. } => device_update(app, Some(*free), 0, 0),
        Event::Wrote { bytes, free, .. } => device_update(app, Some(*free), 1, *bytes as i64),
        // Only filler moves the filler tally. A purge also removes the folder's other
        // contents and `delete_updates` removes a firmware image — charging those to
        // "existing filler" is how deleting a 1.5 GB update briefly read as the device
        // losing a filler file it never had.
        Event::Deleted {
            bytes,
            kind: engine::DeletedKind::Filler,
            ..
        } => device_update(app, None, -1, -(*bytes as i64)),
        Event::Deleted { .. } => {}
        // Nothing left the device, so nothing may move the tally. The whole point of
        // this event is that the optimistic accounting was wrong.
        Event::DeleteRefused { .. } => {}
        // Describes what is already counted, not a change to it.
        Event::Resuming { .. } => {}
        Event::Finished { free } => device_update(app, Some(*free), 0, 0),
        Event::Progress(_) => {}
    }
    match event {
        Event::Started { free, aim, total } => log(
            app,
            format!(
                "Starting at {} free, steering to {} — {} to write.",
                human_bytes(free),
                human_bytes(aim),
                human_bytes(total)
            ),
        ),
        Event::Progress(p) => {
            let _ = app.emit(
                EV_PROGRESS,
                ProgressPayload {
                    done: p.done,
                    total: p.total,
                    fraction: p.fraction(),
                    percent: p.percent(),
                    done_human: human_bytes(p.done),
                    total_human: human_bytes(p.total),
                    rate: p.rate,
                    rate_human: p.rate.map_or_else(
                        || "—".to_string(),
                        |r| format!("{:.1} MB/s", r / kindlefill_core::MIB as f64),
                    ),
                    eta_human: p.eta.map_or_else(|| "estimating…".to_string(), human_eta),
                },
            );
        }
        // Directly under "Starting at…", which is where the doubt lands: that line and
        // a bar leaving from zero otherwise look exactly like a fill that discarded
        // everything the previous run wrote.
        Event::Resuming { files, bytes } => log(
            app,
            format!(
                "Resuming — {} of filler is already on the device ({files} file{}), and \
                 is being kept. The bar below measures only what is left to write.",
                human_bytes(bytes),
                if files == 1 { "" } else { "s" }
            ),
        ),
        Event::Wrote { name, bytes, free } => log(
            app,
            format!(
                "Wrote {name} ({}) — {} free.",
                human_bytes(bytes),
                human_bytes(free)
            ),
        ),
        Event::Deleted { name, bytes, kind } => log(
            app,
            match kind {
                engine::DeletedKind::Interrupted => format!(
                    "Reclaimed {name} ({}) — left behind by a run that was killed \
                     mid-write.",
                    human_bytes(bytes)
                ),
                _ => format!("Deleted {name} ({}).", human_bytes(bytes)),
            },
        ),
        Event::DeleteRefused { name, bytes } => log(
            app,
            format!(
                "Could NOT delete {name} ({}) — the device refused it and the space was \
                 not reclaimed. Unplug and replug the Kindle, then try again.",
                human_bytes(bytes)
            ),
        ),
        Event::Finished { free } => log(app, format!("Finished at {} free.", human_bytes(free))),
    }
}

async fn detect_mtp(
    storage: &mut Storage,
    identity: Identity,
    dir_name: String,
) -> Result<DeviceSnapshot, String> {
    storage.refresh().await.map_err(|e| explain(&e))?;

    let (mut filler_files, mut filler_bytes) = (0usize, 0u64);
    let mut found_foreign = Vec::new();
    let existing = engine::find_fill_dir(storage, &dir_name)
        .await
        .map_err(|e| explain(&e))?;
    if let Some(dir) = &existing {
        let fillers = engine::list_fillers(storage, dir.handle)
            .await
            .map_err(|e| explain(&e))?;
        filler_files = fillers.len();
        filler_bytes = fillers.iter().map(|f| f.size).sum();
        found_foreign = engine::list_foreign(storage, dir.handle)
            .await
            .map_err(|e| explain(&e))?;
    }
    // Computed from the listing the UI is about to display, so the token stands for
    // exactly the set of names the user will see before they tick Overwrite.
    let overwrite_token = engine::overwrite_token(&dir_name, &found_foreign);
    let foreign: Vec<NamedObject> = found_foreign.iter().map(NamedObject::from).collect();

    // Only go looking when it isn't where we expected: scanning every root folder
    // costs a listing each, and on a device with a large documents folder that is not
    // free. When the configured folder has filler, there is nothing to look for.
    let mut elsewhere = Vec::new();
    if filler_files == 0 {
        for f in engine::find_filler_folders(storage)
            .await
            .map_err(|e| explain(&e))?
        {
            if f.name != dir_name {
                elsewhere.push(FillerFolderInfo {
                    name: f.name,
                    files: f.files,
                    bytes: f.bytes,
                    human: human_bytes(f.bytes),
                });
            }
        }
    }

    let updates = engine::list_staged_updates(storage)
        .await
        .map_err(|e| explain(&e))?
        .iter()
        .map(NamedObject::from)
        .collect();

    let info = storage.info();
    Ok(DeviceSnapshot {
        model: identity.label,
        is_kindle: identity.is_kindle,
        storage: info.description.clone(),
        total: info.total_capacity,
        free: info.free_space,
        total_human: human_bytes(info.total_capacity),
        free_human: human_bytes(info.free_space),
        writable: info.is_writable,
        fill_dir: dir_name,
        fill_dir_exists: existing.is_some(),
        filler_files,
        filler_bytes,
        filler_human: human_bytes(filler_bytes),
        foreign,
        overwrite_token,
        updates,
        elsewhere,
    })
}

fn named(entry: &kindlefill_core::mass_storage::Entry) -> NamedObject {
    NamedObject {
        name: entry.name.clone(),
        bytes: entry.bytes,
        human: human_bytes(entry.bytes),
    }
}

fn detect_mass(kindle: &MountedKindle, dir_name: String) -> Result<DeviceSnapshot, String> {
    let space = kindle.space().map_err(|e| explain_io(&e))?;
    let fillers = kindle.list_fillers(&dir_name).map_err(|e| explain_io(&e))?;
    let foreign = kindle.list_foreign(&dir_name).map_err(|e| explain_io(&e))?;
    let filler_bytes = fillers.iter().map(|f| f.bytes).sum();
    let elsewhere = if fillers.is_empty() {
        kindle
            .find_filler_folders()
            .map_err(|e| explain_io(&e))?
            .into_iter()
            .filter(|f| f.name != dir_name)
            .map(|f| FillerFolderInfo {
                name: f.name,
                files: f.files,
                bytes: f.bytes,
                human: human_bytes(f.bytes),
            })
            .collect()
    } else {
        Vec::new()
    };
    Ok(DeviceSnapshot {
        model: "Kindle (USB storage)".to_string(),
        // The transport is the check here: nothing mounts as this that isn't a volume
        // named `Kindle` with a `documents` folder in it.
        is_kindle: true,
        storage: kindle.root().display().to_string(),
        total: space.total,
        free: space.free,
        total_human: human_bytes(space.total),
        free_human: human_bytes(space.free),
        writable: kindle.is_writable(),
        fill_dir: dir_name.clone(),
        fill_dir_exists: kindle
            .find_fill_dir(&dir_name)
            .map_err(|e| explain_io(&e))?
            .is_some(),
        filler_files: fillers.len(),
        filler_bytes,
        filler_human: human_bytes(filler_bytes),
        // The token is derived from the same `foreign` listing rendered above — not
        // from `kindle.overwrite_token()`, which re-reads the directory and could
        // fingerprint a set the user never saw. Same rule as `detect_mtp`: the token
        // stands for exactly the names on screen.
        overwrite_token: engine::overwrite_token_for_entries(
            &dir_name,
            foreign.iter().map(|e| (e.name.as_str(), e.bytes)),
        ),
        foreign: foreign.iter().map(named).collect(),
        updates: kindle
            .list_staged_updates()
            .map_err(|e| explain_io(&e))?
            .iter()
            .map(named)
            .collect(),
        elsewhere,
    })
}

#[tauri::command]
async fn detect(state: State<'_, AppState>, dir_name: String) -> Result<DeviceSnapshot, String> {
    let _op = claim(&state)?;
    engine::validate_dir_name(&dir_name).map_err(|e| e.to_string())?;
    let session = open_device().await?;
    // Read before the match consumes the session, so the panel names whatever actually
    // answered rather than what we hoped would.
    let identity = session.identity();
    match session {
        Session::Mtp { mut storage, .. } => detect_mtp(&mut storage, identity, dir_name).await,
        Session::Mass(kindle) => detect_mass(&kindle, dir_name),
    }
}

/// Delete staged firmware images the user picked by name.
///
/// The names come from the webview, so they are treated as a request rather than an
/// instruction: [`engine::delete_staged_updates`] intersects them with what is
/// actually a root-level `update_*.bin` and ignores anything else. A name that isn't
/// one of those deletes nothing.
#[tauri::command]
async fn delete_updates(
    app: AppHandle,
    state: State<'_, AppState>,
    names: Vec<String>,
) -> Result<String, String> {
    let _op = claim(&state)?;
    if names.is_empty() {
        return Ok("Nothing selected.".to_string());
    }
    let handle = app.clone();
    let removed: Vec<NamedObject> = match open_device().await? {
        Session::Mtp { mut storage, .. } => {
            engine::delete_staged_updates(&mut storage, &names, move |ev| forward(&handle, ev))
                .await
                .map_err(|e| explain(&e))?
                .iter()
                .map(NamedObject::from)
                .collect()
        }
        Session::Mass(kindle) => kindle
            .delete_staged_updates(&names, move |ev| forward(&handle, ev))
            .map_err(|e| explain_io(&e))?
            .iter()
            .map(named)
            .collect(),
    };

    if removed.is_empty() {
        return Ok("Nothing was deleted — no staged update matched.".to_string());
    }
    let bytes: u64 = removed.iter().map(|o| o.bytes).sum();
    Ok(format!(
        "Deleted {} staged update{} ({}).",
        removed.len(),
        if removed.len() == 1 { "" } else { "s" },
        human_bytes(bytes)
    ))
}

/// Fill to the window, optionally taking the folder over first.
///
/// `overwrite` is the token from the [`DeviceSnapshot`] whose foreign listing the user
/// confirmed, or `None` to leave the folder alone. Deliberately not a `bool`: a flag
/// says the user agreed to *something*, a token says to what.
///
/// `allow_non_kindle` is the tick from the panel, and it is re-checked here against the
/// device that is open *now* rather than trusted as permission already granted. The
/// webview cannot be the only thing standing between "the first WPD device Windows
/// enumerated" and a multi-gigabyte write, and the device can change between the detect
/// that raised the question and the Fill that acts on it.
#[tauri::command]
async fn start_fill(
    app: AppHandle,
    state: State<'_, AppState>,
    low: u64,
    high: u64,
    dir_name: String,
    overwrite: Option<String>,
    allow_non_kindle: bool,
) -> Result<String, String> {
    let _op = claim(&state)?;
    let window = Window::new(low, high).map_err(|e| e.to_string())?;
    engine::validate_dir_name(&dir_name).map_err(|e| e.to_string())?;

    let session = open_device().await?;

    let identity = session.identity();
    if !identity.is_kindle && !allow_non_kindle {
        return Err(format!(
            "This device reports itself as \"{}\", which is not a Kindle. Filling it \
             would write gigabytes to whatever it actually is. If you know what this \
             device is and want to fill it anyway, tick the box above Fill.",
            identity.label
        ));
    }
    if !identity.is_kindle {
        log(
            &app,
            format!(
                "Filling \"{}\", which is not a recognised Kindle — you opted in. This \
                 has not been tested on non-Kindle devices.",
                identity.label
            ),
        );
    }

    // Published only once the device is open, not before: a token published ahead of a
    // failed open outlives the command that made it, and Stop then pulls on a token
    // nothing is watching.
    let flag = Arc::new(AtomicBool::new(false));
    let token = CancelToken::from_arc(flag.clone());
    // Scoped so the guard is dropped before the next await — a MutexGuard held across
    // an await point would make this future non-Send and fail to compile.
    {
        *state.cancel.lock().unwrap() = Some(flag.clone());
    }

    // Taking the folder over is a separate, explicit act that happens before any
    // filling — so if it fails, nothing has been written yet, and if the caller didn't
    // ask for it, the destructive path isn't reached at all.
    //
    // The confirmation arrives as the token `detect` computed over the very files the
    // user was shown, not as a bare `true`, and the engine re-derives it from the
    // device before deleting anything. A tick given for one folder therefore cannot be
    // spent on another's contents, and it can't be spent at all on a folder that has
    // changed since it was listed.
    // Held as a `Result` rather than `?`-propagated: an early return here would skip
    // the clearing block below and leave the published flag pointing at a fill that
    // no longer exists — the Stop button then pulls on a token nothing is watching,
    // which is the exact failure the publish ordering above exists to prevent.
    let result: Result<engine::Outcome, String> = match session {
        Session::Mtp { mut storage, .. } => {
            let purge = app.clone();
            let fill = app.clone();
            async {
                if let Some(confirmed) = &overwrite {
                    let handle = purge.clone();
                    let removed = engine::purge_fill_dir_confirmed(
                        &mut storage,
                        &dir_name,
                        confirmed,
                        move |ev| forward(&handle, ev),
                    )
                    .await
                    .map_err(|e| explain_fill(&e))?;
                    log(
                        &purge,
                        format!("Emptied {dir_name} — {removed} item(s) deleted."),
                    );
                }
                let handle = fill.clone();
                engine::fill_with_cancel(&mut storage, window, &dir_name, Some(&token), move |ev| {
                    forward(&handle, ev)
                })
                .await
                .map_err(|e| explain_fill(&e))
            }
            .await
        }
        Session::Mass(kindle) => {
            // The mass path is blocking filesystem I/O; a multi-gigabyte fill run
            // inline would pin one of the runtime's worker threads for its whole
            // duration. `spawn_blocking` gives it a thread that's allowed to block —
            // the MTP arm above needs none of this because its transfers await.
            let purge_app = app.clone();
            let fill_app = app.clone();
            let dir = dir_name.clone();
            let overwrite = overwrite.clone();
            let fill_flag = flag.clone();
            tauri::async_runtime::spawn_blocking(move || {
                if let Some(confirmed) = &overwrite {
                    let handle = purge_app.clone();
                    let removed = kindle
                        .purge_fill_dir_confirmed(&dir, confirmed, move |ev| forward(&handle, ev))
                        .map_err(|e| explain_fill(&e))?;
                    log(
                        &purge_app,
                        format!("Emptied {dir} — {removed} item(s) deleted."),
                    );
                }
                let handle = fill_app.clone();
                kindle
                    .fill_with_cancel(window, &dir, &fill_flag, move |ev| forward(&handle, ev))
                    .map_err(|e| explain_fill(&e))
            })
            .await
            .unwrap_or_else(|e| Err(format!("the fill task failed: {e}")))
        }
    };

    // Ours and only ours. Clearing whatever happens to be in the slot is how one
    // command takes away another's Stop button.
    {
        let mut published = state.cancel.lock().unwrap();
        if published.as_ref().is_some_and(|f| Arc::ptr_eq(f, &flag)) {
            *published = None;
        }
    }

    match result? {
        engine::Outcome::InWindow { free } => Ok(format!(
            "Done — {} free, inside the target window.",
            human_bytes(free)
        )),
        // Names the button as it is labelled on screen. Renaming the control without
        // this left the one message that tells someone how to recover pointing at a
        // button that no longer existed.
        engine::Outcome::Overfilled { free, excess } => Ok(format!(
            "Overfilled: {} free, {} below target. Use Remove filler & folder to \
             recover.",
            human_bytes(free),
            human_bytes(excess)
        )),
        engine::Outcome::Cancelled { free } => Ok(format!(
            "Stopped at {} free. What's written is intact — Fill again to resume.",
            human_bytes(free)
        )),
    }
}

#[tauri::command]
async fn start_clean(
    app: AppHandle,
    state: State<'_, AppState>,
    dir_name: String,
) -> Result<String, String> {
    let _op = claim(&state)?;
    engine::validate_dir_name(&dir_name).map_err(|e| e.to_string())?;
    let mut session = open_device().await?;
    let handle = app.clone();
    let report = match &mut session {
        Session::Mtp { storage, .. } => {
            engine::clean(storage, &dir_name, move |ev| forward(&handle, ev))
                .await
                .map_err(|e| explain_fill(&e))?
        }
        Session::Mass(kindle) => kindle
            .clean(&dir_name, move |ev| forward(&handle, ev))
            .map_err(|e| explain_fill(&e))?,
    };

    if report.removed > 0 {
        return Ok(format!(
            "Removed {} filler file{} ({}) from {dir_name} — {} free.",
            report.removed,
            if report.removed == 1 { "" } else { "s" },
            human_bytes(report.bytes),
            human_bytes(report.free)
        ));
    }
    // Deleting nothing and reporting success is how someone concludes their device is
    // clean while gigabytes of filler sit in a folder under another name. Say where.
    // Scanned only on that empty-handed path: the sweep costs a listing per root
    // folder, and a failure in it must not turn a clean that already succeeded into
    // a reported error.
    let elsewhere = match &session {
        Session::Mtp { storage, .. } => engine::find_filler_folders(storage)
            .await
            .map_err(|e| explain(&e))?,
        Session::Mass(kindle) => kindle.find_filler_folders().map_err(|e| explain_io(&e))?,
    };
    match elsewhere.iter().find(|f| f.name != dir_name) {
        Some(f) => Ok(format!(
            "Nothing to remove in {dir_name}, but {} of filler is in {} — switch to \
             that folder and try again.",
            human_bytes(f.bytes),
            f.name
        )),
        None => Ok(format!(
            "No filler to remove — {} free.",
            human_bytes(report.free)
        )),
    }
}

/// Is a device on the bus at all?
///
/// Deliberately not `detect`: this only enumerates — USB descriptors, the WPD device
/// list, mounted volumes — so it opens no MTP session, starts no `ptpcamerad` tamer,
/// and is safe to call on a timer. ("Enumerates" with one asterisk: the USB scan's
/// vendor-class fallback may open the USB device for a descriptor read on macOS —
/// still no session, and WPD stays multi-client during a fill on Windows.) Opening
/// the device properly to answer "is the cable still in?" would be both wasteful and
/// impossible during a fill, which is exactly when the answer matters.
/// What this build is, for the window to show.
///
/// Both halves earn their place: the version is what a user reports, and the commit is
/// what makes two builds of that version distinguishable — which matters most in
/// exactly the situation this tool keeps finding itself in, a fix in hand and a device
/// on the cable, asking whether the binary in front of you contains it.
#[tauri::command]
fn app_version() -> String {
    format!(
        "v{} · {}",
        env!("CARGO_PKG_VERSION"),
        env!("KINDLEFILL_BUILD")
    )
}

#[tauri::command]
fn device_present() -> bool {
    MountedKindle::find().is_ok_and(|kindle| kindle.is_some()) || mtp_on_bus()
}

#[tauri::command]
fn cancel_fill(state: State<'_, AppState>) {
    if let Some(flag) = state.cancel.lock().unwrap().as_ref() {
        // Back through a token rather than storing `true` into the flag directly:
        // `CancelToken::cancel` is where the memory ordering that makes a cancellation
        // visible to the engine is decided, and setting the bit here would be a second
        // copy of that decision, free to drift from the first.
        CancelToken::from_arc(flag.clone()).cancel();
    }
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            app.manage(AppState::default());
            // The webview swallows JS errors — a frontend fault reaches neither stdout
            // nor the window, it just leaves the UI inert. So there has to be *some*
            // way to see the console. Opt-in rather than automatic in debug: an
            // inspector pane docked under the app is not what someone running
            // `cargo run` to use the tool wants to look at.
            #[cfg(debug_assertions)]
            if std::env::var_os("KINDLEFILL_DEVTOOLS").is_some() {
                if let Some(w) = app.get_webview_window("main") {
                    w.open_devtools();
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            detect,
            start_fill,
            start_clean,
            delete_updates,
            device_present,
            cancel_fill,
            app_version
        ])
        .run(tauri::generate_context!())
        .expect("failed to start KindleFill");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_operation_is_refused_while_one_is_running() {
        let state = AppState::default();
        let first = claim(&state).expect("the first command takes the slot");
        assert!(claim(&state).is_err(), "a second command must be refused");
        drop(first);
        assert!(
            claim(&state).is_ok(),
            "the slot has to come back when the operation ends, or the app wedges"
        );
    }

    /// The refusal has to be inert. A second Fill that clears the cancel slot on its
    /// way out is the failure this whole wave exists to stop: the running transfer
    /// keeps going with nothing left for Stop to pull on.
    #[test]
    fn a_refused_command_leaves_the_running_fills_cancel_token_alone() {
        let state = AppState::default();
        let _op = claim(&state).expect("the fill takes the slot");
        let flag = Arc::new(AtomicBool::new(false));
        *state.cancel.lock().unwrap() = Some(flag.clone());

        assert!(claim(&state).is_err());

        let published = state.cancel.lock().unwrap();
        assert!(
            published.as_ref().is_some_and(|f| Arc::ptr_eq(f, &flag)),
            "the refused command disturbed the running fill's cancel token"
        );
    }
}
