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
                      is_permission_denied, Event, Window};
use mtp_rs::{CancelToken, MtpDevice, Storage};
use serde::Serialize;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, State};

/// Events the webview listens for.
const EV_PROGRESS: &str = "kindlefill://progress";
const EV_LOG: &str = "kindlefill://log";

#[derive(Default)]
struct AppState {
    /// Present only while an operation is running.
    cancel: Mutex<Option<CancelToken>>,
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
    filler_files: usize,
    filler_bytes: u64,
    filler_human: String,
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

/// Open the Kindle and its writable storage.
///
/// The device handle is returned alongside the storage and must be kept alive by the
/// caller for the length of the operation — dropping it closes the MTP session.
async fn open_device() -> Result<(MtpDevice, Storage), String> {
    let device = MtpDevice::open_first().await.map_err(|e| explain(&e))?;
    let storages = device.storages().await.map_err(|e| explain(&e))?;
    let storage = storages
        .into_iter()
        .find(|s| s.info().is_writable)
        .ok_or_else(|| "The device has no writable storage.".to_string())?;
    Ok((device, storage))
}

fn log(app: &AppHandle, line: impl Into<String>) {
    let _ = app.emit(EV_LOG, LogPayload { line: line.into() });
}

/// Forward one engine event to the webview.
fn forward(app: &AppHandle, event: Event) {
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
async fn detect() -> Result<DeviceSnapshot, String> {
    let (_device, mut storage) = open_device().await?;
    storage.refresh().await.map_err(|e| explain(&e))?;

    let (mut filler_files, mut filler_bytes) = (0usize, 0u64);
    if let Some(dir) = engine::find_fill_dir(&storage)
        .await
        .map_err(|e| explain(&e))?
    {
        let fillers = engine::list_fillers(&storage, dir.handle)
            .await
            .map_err(|e| explain(&e))?;
        filler_files = fillers.len();
        filler_bytes = fillers.iter().map(|f| f.size).sum();
    }

    let info = storage.info();
    Ok(DeviceSnapshot {
        model: "Kindle".to_string(),
        storage: info.description.clone(),
        total: info.total_capacity,
        free: info.free_space,
        total_human: human_bytes(info.total_capacity),
        free_human: human_bytes(info.free_space),
        writable: info.is_writable,
        filler_files,
        filler_bytes,
        filler_human: human_bytes(filler_bytes),
    })
}

#[tauri::command]
async fn start_fill(
    app: AppHandle,
    state: State<'_, AppState>,
    low: u64,
    high: u64,
) -> Result<String, String> {
    let window = Window::new(low, high).map_err(|e| e.to_string())?;

    let token = CancelToken::new();
    // Scoped so the guard is dropped before the first await — a MutexGuard held across
    // an await point would make this future non-Send and fail to compile.
    {
        *state.cancel.lock().unwrap() = Some(token.clone());
    }

    let (_device, mut storage) = open_device().await?;
    let handle = app.clone();
    let outcome = engine::fill_with_cancel(&mut storage, window, Some(&token), move |ev| {
        forward(&handle, ev)
    })
    .await;

    *state.cancel.lock().unwrap() = None;

    match outcome.map_err(|e| e.to_string())? {
        engine::Outcome::InWindow { free } => Ok(format!(
            "Done — {} free, inside the target window.",
            human_bytes(free)
        )),
        engine::Outcome::Overfilled { free, excess } => Ok(format!(
            "Overfilled: {} free, {} below target. Use Remove Filler to recover.",
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
async fn start_clean(app: AppHandle, state: State<'_, AppState>) -> Result<String, String> {
    let (_device, mut storage) = open_device().await?;
    let handle = app.clone();
    let free = engine::clean(&mut storage, move |ev| forward(&handle, ev))
        .await
        .map_err(|e| explain(&e))?;

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
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            detect,
            start_fill,
            start_clean,
            cancel_fill
        ])
        .run(tauri::generate_context!())
        .expect("failed to start KindleFill");
}
