//! Fill a Kindle's MTP storage down to a target free-space window, and undo it.
//!
//! Split so the risky part is testable without hardware:
//!
//! - [`plan`] is pure arithmetic — the convergence loop, fully unit-tested.
//! - [`zeros`] is the synthetic byte source fed to uploads.
//! - [`engine`] talks to a real [`mtp_rs::Storage`], and is exercised in
//!   `tests/virtual_device.rs` against `mtp-rs`'s virtual device.
//!
//! # macOS
//!
//! `ptpcamerad` claims MTP devices exclusively the moment they're plugged in, so
//! opening one usually fails until it's out of the way. That failure is detectable —
//! see [`is_exclusive_access`] — and should be surfaced as guidance, not as a generic
//! I/O error.

pub mod engine;
pub mod plan;
pub mod ptpcamerad;
pub mod rate;
pub mod zeros;

pub use engine::{
    clean, current_overwrite_token, delete_staged_updates, fill, fill_with_cancel, find_fill_dir,
    find_filler_folders, list_fillers, list_foreign, list_staged_updates, overwrite_token,
    purge_fill_dir, purge_fill_dir_confirmed, validate_dir_name, CleanReport, Event, FillError,
    FillerFolder, NameError, Outcome, DEFAULT_FILL_DIR,
};
pub use plan::{next_step, Step, Window, WindowError, GIB, KIB, MIB};
pub use rate::{human_duration, human_eta, FillProgress, RateEstimator};
pub use zeros::ZeroStream;

/// Whether this failure is another process holding the device open.
///
/// On macOS that is almost always `ptpcamerad`, Apple's PTP camera daemon, which
/// grabs any MTP device on connect and never lets go. Android File Transfer and
/// OpenMTP do the same if they're running. Worth special-casing because the raw
/// error ("could not be opened for exclusive access") gives a user no idea that the
/// fix is to kill a background daemon.
#[must_use]
pub fn is_exclusive_access(error: &mtp_rs::Error) -> bool {
    matches!(error, mtp_rs::Error::ExclusiveAccess)
}

/// Whether this failure is the OS refusing us access, with nothing else holding the
/// device.
///
/// Deliberately distinct from [`is_exclusive_access`]: the fixes point opposite ways.
/// Exclusive access means *close another program*; this means *grant this program
/// permission* (missing `udev` rules on Linux, or a sandboxed build without the USB
/// entitlement on macOS). Collapsing the two sends people chasing the wrong problem.
#[must_use]
pub fn is_permission_denied(error: &mtp_rs::Error) -> bool {
    matches!(error, mtp_rs::Error::PermissionDenied)
}

/// Whether the device stopped answering.
///
/// Worth its own case because of what it usually means in practice. A Kindle whose
/// MTP session was interrupted — an unplug mid-transfer, a process killed while it
/// held the device — can end up answering reads while refusing every write, which
/// looks like a broken tool rather than a device that needs reconnecting. The give-
/// away is `GetStorageInfo` succeeding while creating a folder times out.
#[must_use]
pub fn is_timeout(error: &mtp_rs::Error) -> bool {
    matches!(error, mtp_rs::Error::Timeout)
}

/// Whether the device went away mid-operation, or its session was reset under us.
#[must_use]
pub fn is_disconnected(error: &mtp_rs::Error) -> bool {
    matches!(
        error,
        mtp_rs::Error::Disconnected | mtp_rs::Error::NoDevice | mtp_rs::Error::DeviceReset
    )
}

/// Human-readable byte size, sized to how people talk about Kindle storage.
#[must_use]
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [(&str, u64); 3] = [("GB", GIB), ("MB", MIB), ("KB", KIB)];
    for (suffix, scale) in UNITS {
        if bytes >= scale {
            return format!("{:.2} {suffix}", bytes as f64 / scale as f64);
        }
    }
    format!("{bytes} B")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_picks_a_sensible_unit() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(70 * MIB), "70.00 MB");
        assert_eq!(human_bytes(13 * GIB), "13.00 GB");
    }
}
