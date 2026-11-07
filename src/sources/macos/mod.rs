//! The macOS sources.

mod iokit;
mod power;
mod rules;

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc;
use tracing::warn;

use crate::core::{SessionState, Snapshot, SystemEvent};
use crate::entities::{self, Entity};

use rules::InputTracker;

/// The entities that the macOS sources supply.
static SUPPLIED: [&Entity; 6] = [
    &entities::SESSIONSTATE,
    &entities::LASTACTIVE,
    &entities::LASTBOOT,
    &entities::LASTSYSTEMSTATECHANGE,
    &entities::GPULOAD,
    &entities::LOGGEDUSERS,
];

/// The entities with the GPU off (`[gpu] enabled = false`).
static SUPPLIED_NO_GPU: [&Entity; 5] = [
    &entities::SESSIONSTATE,
    &entities::LASTACTIVE,
    &entities::LASTBOOT,
    &entities::LASTSYSTEMSTATECHANGE,
    &entities::LOGGEDUSERS,
];

/// The source state of `lastactive`.
static INPUT: Mutex<InputTracker> = Mutex::new(InputTracker::new());

static WARNED_CONSOLE: AtomicBool = AtomicBool::new(false);
static WARNED_IDLE: AtomicBool = AtomicBool::new(false);
static WARNED_GPU: AtomicBool = AtomicBool::new(false);

/// Logs a failed read once, not at each poll.
fn warn_once(flag: &AtomicBool, message: &str) {
    if !flag.swap(true, Ordering::Relaxed) {
        warn!("{message}");
    }
}

pub fn supplied(gpu: bool) -> &'static [&'static Entity] {
    if gpu { &SUPPLIED } else { &SUPPLIED_NO_GPU }
}

/// The device model: `macOS <version>`.
pub fn model() -> String {
    match iokit::os_version() {
        Some(version) => format!("macOS {version}"),
        None => "macOS".to_string(),
    }
}

/// The boot time in seconds since the epoch.
pub fn boot_time() -> Option<i64> {
    let boot = iokit::boot_time();
    if boot.is_none() {
        warn!("no lastboot state: cannot read kern.boottime");
    }
    boot
}

/// One snapshot: the console list, the HID idle time and the GPU statistics.
pub fn poll(gpu: bool) -> Snapshot {
    let (session, users) = match iokit::console_sessions() {
        Some(list) => (
            Some(rules::session_state(&list)),
            Some(rules::logged_users(&list)),
        ),
        None => {
            warn_once(&WARNED_CONSOLE, "cannot read IOConsoleUsers");
            (Some(SessionState::NotKnown), None)
        }
    };

    let last_input = match iokit::hid_idle_ns() {
        Some(idle_ns) => {
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as i64);
            let mut input = INPUT.lock().unwrap_or_else(|e| e.into_inner());
            Some(input.update(now_ms, idle_ns))
        }
        None => {
            warn_once(&WARNED_IDLE, "cannot read HIDIdleTime of IOHIDSystem");
            None
        }
    };

    let gpu = if gpu {
        let sample = iokit::gpu_stats().and_then(|stats| rules::gpu_sample(&stats));
        if sample.is_none() {
            warn_once(&WARNED_GPU, "no GPU utilization from IOAccelerator");
        }
        sample
    } else {
        None
    };

    Snapshot {
        session,
        users,
        last_input,
        display: None,
        gpu,
    }
}

/// The receiver of the sleep and wake events. Starts the power thread, so
/// call it once.
pub fn events() -> mpsc::Receiver<SystemEvent> {
    let (tx, rx) = mpsc::channel(4);
    power::start(tx);
    rx
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::core::{Core, GpuSample};

    #[test]
    fn macos_publishes_six_entities() {
        let names: Vec<&str> = supplied(true).iter().map(|e| e.name).collect();
        assert_eq!(
            names,
            [
                "sessionstate",
                "lastactive",
                "lastboot",
                "lastsystemstatechange",
                "gpuload",
                "loggedusers"
            ]
        );

        let now = Instant::now();
        let mut core = Core::new(supplied(true), Some(1_787_892_895));
        core.update(
            Snapshot {
                session: Some(SessionState::Unlocked),
                users: Some(vec!["alice".into()]),
                last_input: Some(1_787_892_955),
                display: None,
                gpu: Some(GpuSample {
                    load: 27,
                    power: None,
                }),
            },
            now,
        );
        let states: Vec<(&str, String)> = core
            .connect(now)
            .into_iter()
            .map(|(e, v)| (e.name, v))
            .collect();
        assert_eq!(
            states,
            [
                ("sessionstate", "Unlocked".to_string()),
                ("lastactive", "2026-08-28 04:55:55Z".to_string()),
                ("lastboot", "2026-08-28 04:54:55Z".to_string()),
                ("lastsystemstatechange", "ApplicationStarted".to_string()),
                ("gpuload", "27".to_string()),
                ("loggedusers", r#"["alice"]"#.to_string()),
            ]
        );
    }

    #[test]
    fn macos_without_the_gpu_supplies_no_gpuload() {
        let names: Vec<&str> = supplied(false).iter().map(|e| e.name).collect();
        assert_eq!(
            names,
            [
                "sessionstate",
                "lastactive",
                "lastboot",
                "lastsystemstatechange",
                "loggedusers"
            ]
        );
    }
}
