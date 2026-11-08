//! The Windows sources and the service frame. The service reads the boot
//! time, the console session, the session list, the display state and the
//! GPU, and turns the session and power controls into events. It starts
//! `pc-agent session-helper` in each active user session; the helper of the
//! console session gives `lastactive` over the pipe, and `inputhelper` shows
//! that it is connected. `pc-agent session-helper --probe` is the one read
//! of the helper that `diagnose` starts.

mod diagnose;
mod helper;
mod install;
mod pipe;
mod service;
mod supervisor;
mod wts;

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use ::windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use ::windows::Win32::System::SystemInformation::GetTickCount64;
use tokio::sync::mpsc;

use super::helper_rules::{self, HelperMessage, InputState};
use super::wts_rules::DisplayCell;
use crate::core::{Snapshot, SystemEvent};
use crate::entities::{self, Entity};

pub use helper::{probe, run as session_helper};
pub use install::{install, uninstall};
pub use service::run_as_service;

/// The folder of the configuration and the log.
const DATA_DIR: &str = r"C:\ProgramData\pc-agent";

/// The name of the Windows service.
const SERVICE_NAME: &str = "pc-agent";

/// The entities that the Windows sources supply with the GPU on.
static SUPPLIED: [&Entity; 9] = [
    &entities::LASTBOOT,
    &entities::SESSIONSTATE,
    &entities::LOGGEDUSERS,
    &entities::DISPLAY,
    &entities::LASTACTIVE,
    &entities::GPULOAD,
    &entities::GPUPOWER,
    &entities::LASTSYSTEMSTATECHANGE,
    &entities::INPUTHELPER,
];

/// The entities with the GPU off (`[gpu] enabled = false`).
static SUPPLIED_NO_GPU: [&Entity; 7] = [
    &entities::LASTBOOT,
    &entities::SESSIONSTATE,
    &entities::LOGGEDUSERS,
    &entities::DISPLAY,
    &entities::LASTACTIVE,
    &entities::LASTSYSTEMSTATECHANGE,
    &entities::INPUTHELPER,
];

/// The last console display state: the service control handler records each
/// `GUID_CONSOLE_DISPLAY_STATE` notification, the poll reads it.
static DISPLAY: DisplayCell = DisplayCell::new();

/// The last input time and the connected helpers: the pipe server and the
/// presence notification write it, the poll reads it.
static INPUT: Mutex<InputState> = Mutex::new(InputState::new());

/// The receiver of the events of the service controls. The service puts it
/// here before the agent runs; `events()` takes it.
static EVENTS: Mutex<Option<mpsc::Receiver<SystemEvent>>> = Mutex::new(None);

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
/// notification (none before the first), the last input time, whether the
/// helper of the console session is connected and, with `gpu`, the NVML sample.
pub fn poll(gpu: bool) -> Snapshot {
    let (last_input, helper) = {
        let input = input();
        (input.last_input(), input.helper(console_session()))
    };
    Snapshot {
        session: wts::console_state(),
        users: wts::logged_users(),
        last_input,
        display: DISPLAY.get(),
        gpu: if gpu { super::nvml::sample() } else { None },
        helper: Some(helper),
    }
}

fn input() -> std::sync::MutexGuard<'static, InputState> {
    INPUT.lock().unwrap_or_else(|e| e.into_inner())
}

fn console_session() -> u32 {
    // SAFETY: WTSGetActiveConsoleSessionId has no arguments and no preconditions.
    unsafe { WTSGetActiveConsoleSessionId() }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Records a `GUID_GLOBAL_USER_PRESENCE` notification (the fallback of
/// `lastactive` with no helper of the console session).
fn record_presence(present: bool) {
    input().presence(present, console_session(), unix_now());
}

/// Starts the pipe server, the task that applies its events and the
/// supervisor of the helpers. The service calls it once, inside the runtime.
fn start_helpers() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let check = |pid| {
        let started_for = supervisor::SUPERVISOR
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .session_of(pid);
        helper_rules::accept_client(started_for, pipe::process_session(pid))
    };
    tokio::spawn(async move {
        let sddl = helper_rules::pipe_sddl(None);
        pipe::serve(helper_rules::PIPE_NAME, &sddl, check, tx).await;
    });
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            let mut input = input();
            match event {
                pipe::PipeEvent::Connected(session) => input.connected(session),
                pipe::PipeEvent::Disconnected(session) => input.disconnected(session),
                pipe::PipeEvent::Message(session, HelperMessage::LastInput(t)) => {
                    input.helper_input(session, t, console_session(), unix_now())
                }
            }
        }
    });
    supervisor::start();
}

/// `pc-agent diagnose [--config <path>]`: prints the reads of each source
/// once and publishes nothing.
pub async fn diagnose(config: Option<&std::path::Path>) {
    diagnose::run(config).await;
}

/// The receiver of the session, sleep, wake and shutdown events of the
/// service controls. In a console run no service sends an event: the sender
/// is dropped, and the run loop then never receives an event.
pub fn events() -> mpsc::Receiver<SystemEvent> {
    EVENTS
        .lock()
        .ok()
        .and_then(|mut events| events.take())
        .unwrap_or_else(|| mpsc::channel(1).1)
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
                "lastactive",
                "gpuload",
                "gpupower",
                "lastsystemstatechange",
                "inputhelper"
            ]
        );
        assert_eq!(
            names(false),
            [
                "lastboot",
                "sessionstate",
                "loggedusers",
                "display",
                "lastactive",
                "lastsystemstatechange",
                "inputhelper"
            ]
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
