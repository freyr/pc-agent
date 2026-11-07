//! The Linux sources.

mod boot;
mod os;

use std::path::Path;

use tokio::sync::mpsc;
use tracing::warn;

use crate::core::{SessionState, Snapshot, SystemEvent};
use crate::entities::{self, Entity};

/// The entities that the Linux sources supply.
static SUPPLIED: [&Entity; 2] = [&entities::LASTBOOT, &entities::SESSIONSTATE];

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

/// One snapshot. The session source comes in a later task; until then there
/// is no session, so `sessionstate` is `Unknown`.
pub fn poll() -> Snapshot {
    Snapshot {
        session: Some(SessionState::NoSession),
        ..Snapshot::default()
    }
}

/// The receiver of the sleep and wake events. No source sends them yet.
pub fn events() -> mpsc::Receiver<SystemEvent> {
    mpsc::channel(1).1
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::core::Core;

    #[test]
    fn linux_publishes_lastboot_and_an_unknown_session() {
        let names: Vec<&str> = supplied().iter().map(|e| e.name).collect();
        assert_eq!(names, ["lastboot", "sessionstate"]);

        let now = Instant::now();
        let mut core = Core::new(supplied(), Some(1_787_892_895));
        core.update(poll(), now);
        let states: Vec<(&str, String)> = core
            .connect(now)
            .into_iter()
            .map(|(e, v)| (e.name, v))
            .collect();
        assert_eq!(
            states,
            [
                ("lastboot", "2026-08-28 04:54:55Z".to_string()),
                ("sessionstate", "Unknown".to_string())
            ]
        );
    }
}
