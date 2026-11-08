//! The Windows sources and the service frame. The source supplies `lastboot`
//! only; the later tasks add the session, power, display, GPU and helper
//! sources.

mod install;
mod service;

use std::time::{SystemTime, UNIX_EPOCH};

use ::windows::Win32::System::SystemInformation::GetTickCount64;
use tokio::sync::mpsc;

use crate::core::{Snapshot, SystemEvent};
use crate::entities::{self, Entity};

pub use install::{install, uninstall};
pub use service::run_as_service;

/// The folder of the configuration and the log.
const DATA_DIR: &str = r"C:\ProgramData\pc-agent";

/// The name of the Windows service.
const SERVICE_NAME: &str = "pc-agent";

/// The entities that the Windows sources supply.
static SUPPLIED: [&Entity; 1] = [&entities::LASTBOOT];

pub fn supplied(_gpu: bool) -> &'static [&'static Entity] {
    &SUPPLIED
}

/// The device model.
pub fn model() -> String {
    "Windows".to_string()
}

/// The boot time in seconds since the epoch: now minus `GetTickCount64`.
/// Correct because Fast Startup is off on the managed PCs (W-D1).
pub fn boot_time() -> Option<i64> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    // SAFETY: GetTickCount64 has no arguments and no preconditions.
    let up_ms = unsafe { GetTickCount64() };
    Some(now - (up_ms / 1000) as i64)
}

/// One snapshot: no Windows source reads a polled entity yet.
pub fn poll(_gpu: bool) -> Snapshot {
    Snapshot::default()
}

/// `pc-agent diagnose`: the checks are for Linux only.
pub async fn diagnose() {
    println!("diagnose is for Linux");
}

/// The receiver of the sleep and wake events. No Windows source sends one
/// yet: the sender is dropped, and the run loop then never receives an event.
pub fn events() -> mpsc::Receiver<SystemEvent> {
    let (_tx, rx) = mpsc::channel(4);
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_supplies_lastboot_only() {
        for gpu in [true, false] {
            let names: Vec<&str> = supplied(gpu).iter().map(|e| e.name).collect();
            assert_eq!(names, ["lastboot"]);
        }
    }

    #[test]
    fn the_boot_time_is_in_the_past() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let boot = boot_time().unwrap();
        assert!(boot <= now && boot > now - 365 * 86_400, "{boot} {now}");
    }
}
