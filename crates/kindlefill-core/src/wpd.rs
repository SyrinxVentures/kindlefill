//! Is any Windows Portable Device present? — the presence half of the WPD story.
//!
//! On Windows, `mtp_rs::MtpDevice::open_first()` tries the WPD backend before raw
//! USB, and WPD's open takes the first device `IPortableDeviceManager::GetDevices`
//! returns. But `MtpDevice::list_devices()` enumerates over nusb only — it has no
//! WPD side at all — and a Kindle is vendor-class, matched only by an endpoint-layout
//! check that requires opening the device, which the WPD driver binding forbids. So
//! on Windows the library can *open* a Kindle it can never *list*: a presence poll
//! built on `list_devices()` would paint "unplugged" over a live cable.
//!
//! This module asks the exact question the WPD open path asks — does `GetDevices`
//! return anything? — so presence and openability cannot disagree. It belongs
//! upstream as a WPD-aware `list_devices()` in mtp-rs (whose backend docstring still
//! calls WPD "planned" while shipping it); until that lands, this is the smallest
//! honest probe, deliberately not a vendored copy of the backend.

#[cfg(windows)]
mod imp {
    use windows::core::PWSTR;
    use windows::Win32::Devices::PortableDevices::PortableDeviceManager;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
    };

    /// Whether Windows currently sees at least one portable (WPD) device.
    ///
    /// Any WPD device, not just Kindles — because that is exactly what
    /// `open_first()` is willing to open, and a presence probe stricter than the
    /// open path recreates the disagreement it exists to prevent. Errors count as
    /// absent: this feeds a 2-second advisory poll, and WPD stays enumerable while
    /// a fill holds the device (WPD is multi-client), so a persistent COM failure
    /// here is a machine problem the next real `open` will surface with a message.
    pub fn device_present() -> bool {
        // SAFETY: COM init/uninit are balanced below; the manager interface is used
        // only within the initialized span; a null id buffer asks only for a count.
        unsafe {
            let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
            // S_OK or S_FALSE: this thread now holds a reference we must release.
            // RPC_E_CHANGED_MODE: the thread was already an STA — COM is usable,
            // but the reference is not ours to release.
            let must_uninit = hr.is_ok();
            let count = (|| {
                let manager: windows::Win32::Devices::PortableDevices::IPortableDeviceManager =
                    CoCreateInstance(&PortableDeviceManager, None, CLSCTX_ALL).ok()?;
                let mut count: u32 = 0;
                manager
                    .GetDevices(std::ptr::null_mut::<PWSTR>(), &mut count)
                    .ok()?;
                Some(count)
            })();
            if must_uninit {
                CoUninitialize();
            }
            count.unwrap_or(0) > 0
        }
    }
}

#[cfg(not(windows))]
mod imp {
    /// No WPD off Windows; exists so callers don't need `cfg` at every use site —
    /// the same shape as `ptpcamerad`'s non-macOS stub.
    pub fn device_present() -> bool {
        false
    }
}

pub use imp::device_present;
