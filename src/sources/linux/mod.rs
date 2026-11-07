//! The Linux sources.

mod boot;
mod drm;
mod hyprland;
mod logind;
mod os;
mod rules;

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::mpsc;
use tracing::warn;

use crate::core::{Snapshot, SystemEvent};
use crate::entities::{self, Entity};

use rules::{LockTracker, Session};

/// The entities that the Linux sources supply.
static SUPPLIED: [&Entity; 5] = [
    &entities::LASTBOOT,
    &entities::SESSIONSTATE,
    &entities::LOGGEDUSERS,
    &entities::DISPLAY,
    &entities::LASTSYSTEMSTATECHANGE,
];

/// The source state of `sessionstate`: the last lock value.
static LOCK: Mutex<LockTracker> = Mutex::new(LockTracker::new());

static WARNED_SOCKET: AtomicBool = AtomicBool::new(false);

pub fn supplied() -> &'static [&'static Entity] {
    &SUPPLIED
}

/// The device model: the Omarchy version, or `Linux`.
pub fn model() -> String {
    os::read_model(Path::new(os::VERSION_PATH))
}

/// The boot time in seconds since the epoch.
pub fn boot_time() -> Option<i64> {
    match boot::read_btime() {
        Ok(btime) => Some(btime),
        Err(e) => {
            warn!("no lastboot state: {e:#}");
            None
        }
    }
}

/// One snapshot: the logind sessions, the Hyprland monitors of the active
/// session and, without them, the DRM connectors.
pub fn poll() -> Snapshot {
    snapshot(
        logind::sessions(),
        Path::new(hyprland::RUN_USER),
        Path::new(drm::DRM_DIR),
    )
}

/// The snapshot from the logind session list (`None` before the first read)
/// and the Hyprland and DRM directories.
fn snapshot(sessions: Option<Vec<Session>>, run_user: &Path, drm_dir: &Path) -> Snapshot {
    let active = sessions.as_deref().and_then(rules::active_session);
    let monitors = active.and_then(|s| hyprland::monitors(run_user, s.uid));
    if active.is_some() && socket_warning(&WARNED_SOCKET, monitors.is_some()) {
        warn!("no answer from the Hyprland socket of the active session: sessionstate is Unlocked");
    }
    let display = match &monitors {
        Some(list) => Some(rules::display_from_monitors(list)),
        None => rules::display_from_drm(&drm::connectors(drm_dir)),
    };
    let session = sessions.as_ref().map(|_| {
        let read = monitors.as_deref().map(rules::lock_read);
        LOCK.lock()
            .unwrap_or_else(|e| e.into_inner())
            .update(active.is_some(), read)
    });
    Snapshot {
        session,
        users: sessions.as_deref().map(rules::logged_users),
        display,
        ..Snapshot::default()
    }
}

/// `true` when the socket of the active session did not answer and the
/// warning of this outage is not logged yet. An answer ends the outage.
fn socket_warning(warned: &AtomicBool, answered: bool) -> bool {
    if answered {
        warned.store(false, Ordering::Relaxed);
        false
    } else {
        !warned.swap(true, Ordering::Relaxed)
    }
}

/// The receiver of the sleep and wake events. No source sends them yet.
/// Starts the logind task, so call it once, inside the tokio runtime.
pub fn events() -> mpsc::Receiver<SystemEvent> {
    logind::start();
    mpsc::channel(1).1
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::core::{Core, SessionState};

    #[test]
    fn linux_publishes_five_entities() {
        let names: Vec<&str> = supplied().iter().map(|e| e.name).collect();
        assert_eq!(
            names,
            [
                "lastboot",
                "sessionstate",
                "loggedusers",
                "display",
                "lastsystemstatechange"
            ]
        );

        let now = Instant::now();
        let mut core = Core::new(supplied(), Some(1_787_892_895));
        core.update(
            Snapshot {
                session: Some(SessionState::Locked),
                users: Some(vec!["bob".into(), "alice".into()]),
                display: Some(false),
                ..Snapshot::default()
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
                ("lastboot", "2026-08-28 04:54:55Z".to_string()),
                ("sessionstate", "Locked".to_string()),
                ("loggedusers", r#"["alice","bob"]"#.to_string()),
                ("display", "OFF".to_string()),
                ("lastsystemstatechange", "ApplicationStarted".to_string()),
            ]
        );
    }

    #[test]
    fn before_the_first_logind_read_no_session_state_is_given() {
        let none = Path::new("/nonexistent/pc-agent");
        assert_eq!(snapshot(None, none, none), Snapshot::default());
    }

    #[test]
    fn no_session_gives_unknown_no_users_and_no_display() {
        let none = Path::new("/nonexistent/pc-agent");
        assert_eq!(
            snapshot(Some(Vec::new()), none, none),
            Snapshot {
                session: Some(SessionState::NoSession),
                users: Some(Vec::new()),
                ..Snapshot::default()
            }
        );
    }

    #[test]
    fn each_socket_outage_warns_once() {
        let warned = AtomicBool::new(false);
        let calls: Vec<bool> = [false, false, true, false, false]
            .into_iter()
            .map(|answered| socket_warning(&warned, answered))
            .collect();
        assert_eq!(calls, [true, false, false, true, false]);
    }
}
