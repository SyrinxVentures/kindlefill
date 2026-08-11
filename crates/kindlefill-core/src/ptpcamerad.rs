//! Keeping Apple's PTP camera daemon off the Kindle for the duration of a transfer.
//!
//! macOS ships `ptpcamerad`, which claims any PTP/MTP device the instant it's
//! plugged in and holds it exclusively. It only speaks PTP, so it can't actually
//! talk to a Kindle — it just prevents anything else from doing so. This is the
//! whole reason "you can't do MTP on a Mac" is folklore: the protocol is fine, the
//! device is fine, a daemon is sitting on it.
//!
//! Scoped rather than permanent: `launchctl disable system/com.apple.ptpcamerad`
//! fixes it forever but also breaks importing photos from a camera, and silently
//! degrading an unrelated part of someone's Mac is not ours to do. Instead the
//! daemon is killed repeatedly only while a transfer is running, and launchd is
//! left to restart it afterwards.

#[cfg(target_os = "macos")]
mod imp {
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    /// launchd restarts the daemon within a second or so of each kill, and it
    /// re-claims the device when it does. Re-killing faster than it can respawn is
    /// what keeps the interface free for the length of a multi-GB transfer.
    const KILL_INTERVAL: Duration = Duration::from_millis(500);

    fn is_running() -> bool {
        Command::new("/usr/bin/pgrep")
            .args(["-x", "ptpcamerad"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// One kill attempt. `Ok(true)` means a process was signalled, `Ok(false)` that
    /// none was running. `Err` means we were not permitted to signal it.
    pub fn kill_once() -> Result<bool, String> {
        let out = Command::new("/usr/bin/pkill")
            .args(["-9", "-x", "ptpcamerad"])
            .output()
            .map_err(|e| format!("could not run pkill: {e}"))?;
        match out.status.code() {
            Some(0) => Ok(true),
            // pkill's documented "no processes matched".
            Some(1) => Ok(false),
            _ => Err(String::from_utf8_lossy(&out.stderr).trim().to_string()),
        }
    }

    /// Can we signal the daemon without elevating?
    ///
    /// This decides whether a GUI build needs an admin prompt or can just work, so
    /// it is answered by trying, not assumed.
    pub fn probe_privileges() -> PrivilegeCheck {
        if !is_running() {
            return PrivilegeCheck::NotRunning;
        }
        match kill_once() {
            Ok(_) => PrivilegeCheck::KillableUnprivileged,
            Err(reason) => PrivilegeCheck::NeedsElevation { reason },
        }
    }

    #[derive(Debug)]
    pub enum PrivilegeCheck {
        NotRunning,
        KillableUnprivileged,
        NeedsElevation { reason: String },
    }

    /// Holds `ptpcamerad` down until dropped.
    pub struct Tamer {
        stop: Arc<AtomicBool>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl Tamer {
        pub fn start() -> Self {
            let stop = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&stop);
            let handle = thread::spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    let _ = kill_once();
                    thread::sleep(KILL_INTERVAL);
                }
            });
            Tamer {
                stop,
                handle: Some(handle),
            }
        }
    }

    impl Drop for Tamer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    /// Mirrors the macOS enum so call sites compile unchanged. Off macOS there is no
    /// `ptpcamerad`, so only `NotRunning` is ever produced — the other variants exist
    /// to keep the two shapes identical, not because they're reachable here.
    #[derive(Debug)]
    #[allow(dead_code)]
    pub enum PrivilegeCheck {
        NotRunning,
        KillableUnprivileged,
        NeedsElevation { reason: String },
    }

    pub fn probe_privileges() -> PrivilegeCheck {
        PrivilegeCheck::NotRunning
    }

    /// No-op off macOS; exists so callers don't need `cfg` at every use site.
    pub struct Tamer;

    impl Tamer {
        pub fn start() -> Self {
            Tamer
        }
    }
}

pub use imp::{probe_privileges, PrivilegeCheck, Tamer};
