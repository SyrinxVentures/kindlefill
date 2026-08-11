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

use kindlefill_core::{engine, human_bytes, human_duration, is_exclusive_access,
                      is_permission_denied, ptpcamerad, Event, Window};
use mtp_rs::{CancelToken, MtpDevice, Storage};
use serde::Serialize;
use std::sync::Mutex;
use std::time::Instant;
use tauri::{AppHandle, Emitter, Manager, State};

/// Events the webview listens for.
const EV_PROGRESS: &str = "kindlefill://progress";
const EV_LOG: &str = "kindlefill://log";

#[derive(Default)]
struct AppState {
    /// Present only while an operation is running.
    cancel: Mutex<Option<CancelToken>>,
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
    model: String,
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
    /// Staged OTA firmware images at the storage root.
    updates: Vec<NamedObject>,
}

#[derive(Serialize, Clone)]
struct ProgressPayload {
    done: u64,
    total: u64,
    fraction: f64,
    percent: f64,
    done_human: String,
    total_human: String,
    rate_human: String,
    eta_human: String,
}

#[derive(Serialize, Clone)]
struct LogPayload {
    line: String,
}

/// Turn an MTP failure into something a person can act on.
///
/// The raw errors are useless to a user: "could not be opened for exclusive access"
/// gives no hint that the fix is to quit a background daemon. These two cases point in
/// opposite directions — close another program, versus grant this one permission — so
/// they're kept distinct rather than collapsed into one "device error".
fn explain(error: &mtp_rs::Error) -> String {
    if is_exclusive_access(error) {
        return "Another program is holding the Kindle. On macOS this is usually \
                ptpcamerad, Apple's camera daemon; Android File Transfer and OpenMTP \
                do it too. Quit those and unplug/replug the cable."
            .to_string();
    }
    if is_permission_denied(error) {
        return format!(
            "The system refused access to the device (nothing else is holding it). \
             On Linux this usually means missing udev rules. Underlying error: {error}"
        );
    }
    format!("{error}")
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
        other => other.to_string(),
    }
}

/// An open Kindle, plus the things that have to stay alive alongside it.
///
/// The device handle must outlive the operation — dropping it closes the MTP session —
/// and so must the `ptpcamerad` tamer, which keeps Apple's camera daemon from
/// re-claiming the interface partway through a 17-minute transfer. Bundling them means
/// a caller can't hold one and drop the other.
struct Session {
    _device: MtpDevice,
    _tamer: ptpcamerad::Tamer,
    storage: Storage,
}

/// Open the Kindle and its writable storage.
///
/// The daemon is tamed for the whole session rather than only at open time: launchd
/// restarts `ptpcamerad` within a second of each kill and it re-claims the device when
/// it does, so a single kill before opening would not survive the transfer. This is the
/// same [`ptpcamerad::Tamer`] the CLI uses — the module used to live inside the CLI
/// crate, which meant the app could not do this at all despite the README saying the
/// tool did.
async fn open_device() -> Result<Session, String> {
    let tamer = ptpcamerad::Tamer::start();
    let device = MtpDevice::open_first().await.map_err(|e| explain(&e))?;
    let storages = device.storages().await.map_err(|e| explain(&e))?;
    let storage = storages
        .into_iter()
        .find(|s| s.info().is_writable)
        .ok_or_else(|| "The device has no writable storage.".to_string())?;
    Ok(Session {
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

/// Forward one engine event to the webview.
fn forward(app: &AppHandle, event: Event) {
    trace(&event);
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
                    rate_human: p.rate.map_or_else(
                        || "—".to_string(),
                        |r| format!("{:.1} MB/s", r / kindlefill_core::MIB as f64),
                    ),
                    eta_human: p.eta.map_or_else(|| "estimating…".to_string(), human_duration),
                },
            );
        }
        Event::Wrote { name, bytes, free } => log(
            app,
            format!(
                "Wrote {name} ({}) — {} free.",
                human_bytes(bytes),
                human_bytes(free)
            ),
        ),
        Event::Deleted { name, bytes } => {
            log(app, format!("Deleted {name} ({}).", human_bytes(bytes)))
        }
        Event::Finished { free } => log(app, format!("Finished at {} free.", human_bytes(free))),
    }
}

#[tauri::command]
async fn detect(dir_name: String) -> Result<DeviceSnapshot, String> {
    engine::validate_dir_name(&dir_name).map_err(|e| e.to_string())?;
    let mut session = open_device().await?;
    let storage = &mut session.storage;
    storage.refresh().await.map_err(|e| explain(&e))?;

    let (mut filler_files, mut filler_bytes) = (0usize, 0u64);
    let mut foreign = Vec::new();
    let existing = engine::find_fill_dir(storage, &dir_name)
        .await
        .map_err(|e| explain(&e))?;
    if let Some(dir) = &existing {
        let fillers = engine::list_fillers(storage, dir.handle)
            .await
            .map_err(|e| explain(&e))?;
        filler_files = fillers.len();
        filler_bytes = fillers.iter().map(|f| f.size).sum();
        foreign = engine::list_foreign(storage, dir.handle)
            .await
            .map_err(|e| explain(&e))?
            .iter()
            .map(NamedObject::from)
            .collect();
    }

    let updates = engine::list_staged_updates(storage)
        .await
        .map_err(|e| explain(&e))?
        .iter()
        .map(NamedObject::from)
        .collect();

    let info = storage.info();
    Ok(DeviceSnapshot {
        model: "Kindle".to_string(),
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
        updates,
    })
}

/// Delete staged firmware images the user picked by name.
///
/// The names come from the webview, so they are treated as a request rather than an
/// instruction: [`engine::delete_staged_updates`] intersects them with what is
/// actually a root-level `update_*.bin` and ignores anything else. A name that isn't
/// one of those deletes nothing.
#[tauri::command]
async fn delete_updates(app: AppHandle, names: Vec<String>) -> Result<String, String> {
    if names.is_empty() {
        return Ok("Nothing selected.".to_string());
    }
    let mut session = open_device().await?;
    let storage = &mut session.storage;
    let handle = app.clone();
    let removed = engine::delete_staged_updates(storage, &names, move |ev| {
        forward(&handle, ev)
    })
    .await
    .map_err(|e| explain(&e))?;

    if removed.is_empty() {
        return Ok("Nothing was deleted — no staged update matched.".to_string());
    }
    let bytes: u64 = removed.iter().map(|o| o.size).sum();
    Ok(format!(
        "Deleted {} staged update{} ({}).",
        removed.len(),
        if removed.len() == 1 { "" } else { "s" },
        human_bytes(bytes)
    ))
}

#[tauri::command]
async fn start_fill(
    app: AppHandle,
    state: State<'_, AppState>,
    low: u64,
    high: u64,
    dir_name: String,
    overwrite: bool,
) -> Result<String, String> {
    let window = Window::new(low, high).map_err(|e| e.to_string())?;
    engine::validate_dir_name(&dir_name).map_err(|e| e.to_string())?;

    let token = CancelToken::new();
    // Scoped so the guard is dropped before the first await — a MutexGuard held across
    // an await point would make this future non-Send and fail to compile.
    {
        *state.cancel.lock().unwrap() = Some(token.clone());
    }

    let mut session = open_device().await?;
    let storage = &mut session.storage;

    // Taking the folder over is a separate, explicit act that happens before any
    // filling — so if it fails, nothing has been written yet, and if the caller didn't
    // ask for it, the destructive path isn't reached at all.
    if overwrite {
        let handle = app.clone();
        let removed = engine::purge_fill_dir(storage, &dir_name, move |ev| forward(&handle, ev))
            .await
            .map_err(|e| explain_fill(&e))?;
        log(
            &app,
            format!("Emptied {dir_name} — {removed} item(s) deleted."),
        );
    }

    let handle = app.clone();
    let outcome =
        engine::fill_with_cancel(storage, window, &dir_name, Some(&token), move |ev| {
            forward(&handle, ev)
        })
        .await;

    *state.cancel.lock().unwrap() = None;

    match outcome.map_err(|e| explain_fill(&e))? {
        engine::Outcome::InWindow { free } => Ok(format!(
            "Done — {} free, inside the target window.",
            human_bytes(free)
        )),
        engine::Outcome::Overfilled { free, excess } => Ok(format!(
            "Overfilled: {} free, {} below target. Use Remove Fill Folder & Fill \
             Content to recover.",
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
    engine::validate_dir_name(&dir_name).map_err(|e| e.to_string())?;
    let mut session = open_device().await?;
    let storage = &mut session.storage;
    let handle = app.clone();
    let free = engine::clean(storage, &dir_name, move |ev| forward(&handle, ev))
        .await
        .map_err(|e| explain_fill(&e))?;

    *state.cancel.lock().unwrap() = None;
    Ok(format!("Filler removed — {} free.", human_bytes(free)))
}

#[tauri::command]
fn cancel_fill(state: State<'_, AppState>) {
    if let Some(token) = state.cancel.lock().unwrap().as_ref() {
        token.cancel();
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
            cancel_fill
        ])
        .run(tauri::generate_context!())
        .expect("failed to start KindleFill");
}
