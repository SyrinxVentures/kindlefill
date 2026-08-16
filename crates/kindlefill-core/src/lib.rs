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
pub mod mass_storage;
pub mod plan;
pub mod ptpcamerad;
pub mod rate;
pub mod wpd;
pub mod zeros;

pub use engine::{
    clean, current_overwrite_token, delete_staged_updates, fill, fill_with_cancel, find_fill_dir,
    find_filler_folders, list_fillers, list_foreign, list_staged_updates, overwrite_token,
    overwrite_token_for_entries, purge_fill_dir, purge_fill_dir_confirmed, validate_dir_name,
    CleanReport, DeletedKind, Event, FillError, FillerFolder, NameError, Outcome, DEFAULT_FILL_DIR,
};
pub use plan::{next_removal, next_step, Removal, Step, Window, WindowError, GIB, KIB, MIB};
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

/// Does this device's own reported identity say "Kindle"?
///
/// Asked because opening a device and *being on the right device* are different
/// questions on Windows, and only one of them was being asked. `open_first` takes the
/// first device the WPD manager enumerates, with no filter — and Windows publishes a
/// WPD entry for every USB mass-storage volume, not just for media players. A 10th-gen
/// Oasis proves the mechanism: it appears as `SWD\WPDBUSENUM\_??_USBSTOR#DISK&VEN_
/// KINDLE&PROD_INTERNAL_STORAGE`, a generic shim over a USBSTOR disk whose only
/// Kindle-specific part is the vendor string inside it. A USB stick or SD card in the
/// same machine gets the same treatment and can be enumerated first, so without this
/// the tool would happily report a flash drive's capacity and write filler onto it.
///
/// Matched on the strings the device volunteers, because that is the evidence both
/// transports actually have: this Oasis reports manufacturer `Kindle` and model
/// `Internal Storage`, while a native-MTP Kindle reports `Amazon`. Substring rather
/// than equality — the two real devices already disagree on which field carries the
/// word, and neither spelling is promised by any specification.
///
/// Deliberately a *heuristic with an escape hatch* rather than a hard gate. It is used
/// to decide whether the user has to opt in before filling, not to decide whether the
/// device may be used at all: a false negative on some unseen model must cost a
/// checkbox, not the whole tool. The mass-storage transport does not need this — a
/// volume named `Kindle` holding a `documents` folder has already answered it.
#[must_use]
pub fn looks_like_kindle(manufacturer: &str, model: &str) -> bool {
    let haystack = format!("{manufacturer} {model}").to_lowercase();
    haystack.contains("kindle") || haystack.contains("amazon")
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
    fn the_two_devices_this_has_actually_run_against_are_recognised() {
        // Exactly as each reports itself: the Oasis carries the word in the
        // manufacturer field with a model that says nothing, which is why neither
        // field alone can be the one that's checked.
        assert!(looks_like_kindle("Kindle", "Internal Storage"));
        assert!(looks_like_kindle("Amazon", "Kindle"));
    }

    #[test]
    fn a_usb_stick_enumerated_first_is_not_mistaken_for_a_kindle() {
        // The failure this exists to stop: WPD publishes an entry for any mass-storage
        // volume, and whichever Windows returns first is what `open_first` opens.
        assert!(!looks_like_kindle("SanDisk", "Cruzer Blade"));
        assert!(!looks_like_kindle("Generic", "USB Flash Disk"));
        assert!(!looks_like_kindle("", ""));
    }

    #[test]
    fn recognition_does_not_depend_on_how_the_device_capitalises_itself() {
        assert!(looks_like_kindle("AMAZON", "KINDLE PAPERWHITE"));
        assert!(looks_like_kindle("amazon", "kindle"));
    }

    #[test]
    fn human_bytes_picks_a_sensible_unit() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(70 * MIB), "70.00 MB");
        assert_eq!(human_bytes(13 * GIB), "13.00 GB");
    }
}
