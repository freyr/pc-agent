//! The Windows sources and the service frame. The service reads the boot
//! time, the console session, the session list, the display state and the
//! GPU; the later tasks add the power events and the helper sources.

mod install;
mod service;
mod wts;

use std::time::{SystemTime, UNIX_EPOCH};

use ::windows::Win32::System::SystemInformation::GetTickCount64;
use tokio::sync::mpsc;

use super::wts_rules::DisplayCell;
use crate::core::{Snapshot, SystemEvent};
use crate::entities::{self, Entity};

pub use install::{install, uninstall};
pub use service::run_as_service;

/// The folder of the configuration and the log.
const DATA_DIR: &str = r"C:\ProgramData\pc-agent";

/// The name of the Windows service.
const SERVICE_NAME: &str = "pc-agent";

/// The entities that the Windows sources supply with the GPU on.
static SUPPLIED: [&Entity; 6] = [
    &entities::LASTBOOT,
    &entities::SESSIONSTATE,
    &entities::LOGGEDUSERS,
    &entities::DISPLAY,
    &entities::GPULOAD,
    &entities::GPUPOWER,
];

/// The entities with the GPU off (`[gpu] enabled = false`).
static SUPPLIED_NO_GPU: [&Entity; 4] = [
    &entities::LASTBOOT,
    &entities::SESSIONSTATE,
    &entities::LOGGEDUSERS,
    &entities::DISPLAY,
];

/// The last console display state: the service control handler records each
/// `GUID_CONSOLE_DISPLAY_STATE` notification, the poll reads it.
static DISPLAY: DisplayCell = DisplayCell::new();

pub fn supplied(gpu: bool) -> &'static [&'static Entity] {
    if gpu { &SUPPLIED } else { &SUPPLIED_NO_GPU }
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

/// One snapshot: the console session, the session list, the last display
/// notification (none before the first) and, with `gpu`, the NVML sample.
pub fn poll(gpu: bool) -> Snapshot {
    Snapshot {
        session: wts::console_state(),
        users: wts::logged_users(),
        display: DISPLAY.get(),
        gpu: if gpu { super::nvml::sample() } else { None },
        ..Snapshot::default()
    }
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

    fn names(gpu: bool) -> Vec<&'static str> {
        supplied(gpu).iter().map(|e| e.name).collect()
    }

    #[test]
    fn windows_supplies_the_service_entities() {
        assert_eq!(
            names(true),
            [
                "lastboot",
                "sessionstate",
                "loggedusers",
                "display",
                "gpuload",
                "gpupower"
            ]
        );
        assert_eq!(
            names(false),
            ["lastboot", "sessionstate", "loggedusers", "display"]
        );
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
