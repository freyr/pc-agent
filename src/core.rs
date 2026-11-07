//! The snapshot core. It compares the snapshots of the source and decides
//! which states to publish. It has no OS code: the sources meet it only
//! through `Snapshot`, `SystemEvent` and the list of supplied entities.

use std::time::{Duration, Instant};

use tokio::sync::oneshot;

use crate::entities::{self, Entity};
use crate::time::format_timestamp;

/// The interval of the republish of the "every 30 s" entities, and the
/// minimum interval between two `lastactive` publishes.
pub const REPUBLISH: Duration = Duration::from_secs(30);

/// An input after this many seconds without input publishes `lastactive` at once.
const IDLE_GAP_SECS: i64 = 60;

/// The entities that the core publishes again every 30 s.
const PERIODIC: [&str; 5] = [
    entities::SESSIONSTATE.name,
    entities::LOGGEDUSERS.name,
    entities::DISPLAY.name,
    entities::GPULOAD.name,
    entities::GPUPOWER.name,
];

/// One state to publish: the entity and its payload.
pub type State = (&'static Entity, String);

/// The session state that a source reads.
// The sources of later tasks construct the variants.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SessionState {
    Locked,
    Unlocked,
    /// No session: published as `Unknown`.
    NoSession,
    /// The source cannot tell: the last session state stays.
    NotKnown,
}

/// One GPU sample.
// The sources of later tasks construct the sample.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GpuSample {
    /// Utilization, integer percent.
    pub load: u32,
    /// Power in watts, when the OS has a source for it.
    pub power: Option<f64>,
}

/// What the source of the running OS reads at each poll. A field is `None`
/// when the OS has no source for it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub session: Option<SessionState>,
    /// The logged-in user names, in any order, duplicates allowed.
    pub users: Option<Vec<String>>,
    /// The time of the last counted input, seconds since the epoch. `None`
    /// until the source has counted one input.
    pub last_input: Option<i64>,
    /// `true` when the display is on.
    pub display: Option<bool>,
    /// The GPU sample of this poll. `None` (no sample) is not published, so
    /// the GPU entities expire.
    pub gpu: Option<GpuSample>,
}

/// The sleep and wake events that a source sends to the core.
// Linux sends no events yet (task 9).
#[allow(dead_code)]
#[derive(Debug)]
pub enum SystemEvent {
    /// The system goes to sleep. The core publishes the `Suspend` state and
    /// `offline`, then completes `done` when the broker has acknowledged
    /// `offline`, after 2 s without the ack, or at once when not connected.
    Suspend { done: oneshot::Sender<()> },
    /// The system woke up.
    Resume,
}

/// The current state of the supplied entities.
pub struct Core {
    supplied: &'static [&'static Entity],
    boot_time: Option<i64>,
    /// `Locked`, `Unlocked` or `NoSession`, never `NotKnown`.
    session: Option<SessionState>,
    /// Sorted and without duplicates.
    users: Option<Vec<String>>,
    last_input: Option<i64>,
    display: Option<bool>,
    gpu: Option<GpuSample>,
    last_event: Option<&'static str>,
    /// The last published `lastactive` input time and when it was published.
    lastactive_sent: Option<(i64, Instant)>,
    /// The time of the last full publish (connect or 30 s republish).
    last_republish: Option<Instant>,
}

impl Core {
    /// `supplied` is the list of the source module; `boot_time` is the boot
    /// time in seconds since the epoch, when the OS has a source for it.
    pub fn new(supplied: &'static [&'static Entity], boot_time: Option<i64>) -> Self {
        Core {
            supplied,
            boot_time,
            session: None,
            users: None,
            last_input: None,
            display: None,
            gpu: None,
            last_event: Some("ApplicationStarted"),
            lastactive_sent: None,
            last_republish: None,
        }
    }

    pub fn supplied(&self) -> &'static [&'static Entity] {
        self.supplied
    }

    /// All known states of the supplied entities, in the order of the list.
    /// Called at each connect.
    pub fn connect(&mut self, now: Instant) -> Vec<State> {
        self.last_republish = Some(now);
        if let Some(t) = self.last_input {
            self.lastactive_sent = Some((t, now));
        }
        self.states(|_| true)
    }

    /// Takes the snapshot of one poll and gives the states to publish now:
    /// the changed states, the "every 30 s" states when 30 s have passed,
    /// `lastactive` by its rate rule, then the session events.
    pub fn update(&mut self, snap: Snapshot, now: Instant) -> Vec<State> {
        let mut changed: Vec<&str> = Vec::new();
        let mut events: Vec<&'static str> = Vec::new();

        if let Some(mut users) = snap.users {
            users.sort();
            users.dedup();
            if self.users.as_ref() != Some(&users) {
                if let Some(old) = &self.users {
                    let gone = old.iter().filter(|u| !users.contains(u)).count();
                    let new = users.iter().filter(|u| !old.contains(u)).count();
                    events.extend(std::iter::repeat_n("SessionLogoff", gone));
                    events.extend(std::iter::repeat_n("SessionLogon", new));
                }
                self.users = Some(users);
                changed.push(entities::LOGGEDUSERS.name);
            }
        }

        if let Some(read) = snap.session {
            let new = match read {
                SessionState::NotKnown => self.session.unwrap_or(SessionState::Unlocked),
                other => other,
            };
            if self.session != Some(new) {
                match (self.session, new) {
                    (Some(SessionState::Unlocked), SessionState::Locked) => {
                        events.push("SessionLock")
                    }
                    (Some(SessionState::Locked), SessionState::Unlocked) => {
                        events.push("SessionUnlock")
                    }
                    _ => {}
                }
                self.session = Some(new);
                changed.push(entities::SESSIONSTATE.name);
            }
        }

        if let Some(on) = snap.display {
            if self.display != Some(on) {
                changed.push(entities::DISPLAY.name);
            }
            self.display = Some(on);
        }

        self.gpu = snap.gpu;

        let mut idle_gap = false;
        if let Some(t) = snap.last_input {
            idle_gap = self.last_input.is_none_or(|prev| t - prev >= IDLE_GAP_SECS);
            self.last_input = Some(t);
        }
        let mut lastactive = false;
        if let Some(t) = self.last_input {
            let pending = self.lastactive_sent.is_none_or(|(sent, _)| sent != t);
            let rate_ok = self
                .lastactive_sent
                .is_none_or(|(_, at)| now.duration_since(at) >= REPUBLISH);
            if pending && (idle_gap || rate_ok) {
                lastactive = true;
                self.lastactive_sent = Some((t, now));
            }
        }

        let republish = self
            .last_republish
            .is_some_and(|at| now.duration_since(at) >= REPUBLISH);
        if republish {
            self.last_republish = Some(now);
        }

        let mut out = self.states(|e| {
            changed.contains(&e.name)
                || (republish && PERIODIC.contains(&e.name))
                || (lastactive && e.name == entities::LASTACTIVE.name)
        });
        for name in events {
            out.extend(self.event(name));
        }
        out
    }

    /// Records a system event (`Suspend`, `Resume`) and gives its
    /// `lastsystemstatechange` state when the entity is supplied.
    pub fn event(&mut self, name: &'static str) -> Vec<State> {
        self.last_event = Some(name);
        self.states(|e| e.name == entities::LASTSYSTEMSTATECHANGE.name)
    }

    /// The known states of the supplied entities that `include` selects, in
    /// the order of the list.
    fn states(&self, include: impl Fn(&Entity) -> bool) -> Vec<State> {
        self.supplied
            .iter()
            .filter(|e| include(e))
            .filter_map(|e| self.value(e).map(|v| (*e, v)))
            .collect()
    }

    /// The current payload of an entity, or `None` when it is not known.
    fn value(&self, entity: &Entity) -> Option<String> {
        let name = entity.name;
        if name == entities::SESSIONSTATE.name {
            self.session.map(|s| {
                match s {
                    SessionState::Locked => "Locked",
                    SessionState::Unlocked => "Unlocked",
                    SessionState::NoSession | SessionState::NotKnown => "Unknown",
                }
                .to_string()
            })
        } else if name == entities::LASTACTIVE.name {
            self.last_input.map(format_timestamp)
        } else if name == entities::LASTBOOT.name {
            self.boot_time.map(format_timestamp)
        } else if name == entities::LASTSYSTEMSTATECHANGE.name {
            self.last_event.map(str::to_string)
        } else if name == entities::GPULOAD.name {
            self.gpu.map(|g| g.load.to_string())
        } else if name == entities::GPUPOWER.name {
            self.gpu.and_then(|g| g.power).map(|w| format!("{w:.1}"))
        } else if name == entities::LOGGEDUSERS.name {
            self.users
                .as_ref()
                .map(|u| serde_json::Value::from(u.clone()).to_string())
        } else if name == entities::DISPLAY.name {
            self.display
                .map(|on| if on { "ON" } else { "OFF" }.to_string())
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fake source of the tests: it supplies every entity of the table.
    static ALL: [&Entity; 8] = [
        &entities::SESSIONSTATE,
        &entities::LASTACTIVE,
        &entities::LASTBOOT,
        &entities::LASTSYSTEMSTATECHANGE,
        &entities::GPULOAD,
        &entities::GPUPOWER,
        &entities::LOGGEDUSERS,
        &entities::DISPLAY,
    ];

    /// 2026-08-28 04:54:55Z, computed outside this code.
    const BOOT: i64 = 1_787_892_895;

    fn names(states: &[State]) -> Vec<(&str, &str)> {
        states.iter().map(|(e, v)| (e.name, v.as_str())).collect()
    }

    fn snap(session: SessionState, users: &[&str]) -> Snapshot {
        Snapshot {
            session: Some(session),
            users: Some(users.iter().map(|u| u.to_string()).collect()),
            ..Snapshot::default()
        }
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// A core that took one snapshot and connected at `t0`.
    fn connected(supplied: &'static [&'static Entity], first: Snapshot, t0: Instant) -> Core {
        let mut core = Core::new(supplied, Some(BOOT));
        core.update(first, t0);
        core.connect(t0);
        core
    }

    #[test]
    fn connect_gives_all_known_states_in_supplied_order() {
        let t0 = Instant::now();
        let mut core = Core::new(&ALL, Some(BOOT));
        core.update(
            Snapshot {
                session: Some(SessionState::Unlocked),
                users: Some(vec!["zoe".into(), "jahu".into(), "jahu".into()]),
                last_input: Some(BOOT + 60),
                display: Some(true),
                gpu: Some(GpuSample {
                    load: 37,
                    power: Some(85.26),
                }),
            },
            t0,
        );
        assert_eq!(
            names(&core.connect(t0)),
            [
                ("sessionstate", "Unlocked"),
                ("lastactive", "2026-08-28 04:55:55Z"),
                ("lastboot", "2026-08-28 04:54:55Z"),
                ("lastsystemstatechange", "ApplicationStarted"),
                ("gpuload", "37"),
                ("gpupower", "85.3"),
                ("loggedusers", r#"["jahu","zoe"]"#),
                ("display", "ON"),
            ]
        );
    }

    #[test]
    fn linux_today_connects_with_lastboot_and_unknown_session() {
        static LINUX: [&Entity; 2] = [&entities::LASTBOOT, &entities::SESSIONSTATE];
        let t0 = Instant::now();
        let mut core = Core::new(&LINUX, Some(BOOT));
        core.update(
            Snapshot {
                session: Some(SessionState::NoSession),
                ..Snapshot::default()
            },
            t0,
        );
        assert_eq!(
            names(&core.connect(t0)),
            [
                ("lastboot", "2026-08-28 04:54:55Z"),
                ("sessionstate", "Unknown")
            ]
        );
    }

    #[test]
    fn a_lock_gives_locked_and_session_lock_at_once() {
        let t0 = Instant::now();
        let mut core = connected(&ALL, snap(SessionState::Unlocked, &["jahu"]), t0);
        let out = core.update(snap(SessionState::Locked, &["jahu"]), t0 + secs(2));
        assert_eq!(
            names(&out),
            [
                ("sessionstate", "Locked"),
                ("lastsystemstatechange", "SessionLock")
            ]
        );
        let out = core.update(snap(SessionState::Unlocked, &["jahu"]), t0 + secs(4));
        assert_eq!(
            names(&out),
            [
                ("sessionstate", "Unlocked"),
                ("lastsystemstatechange", "SessionUnlock")
            ]
        );
    }

    #[test]
    fn a_change_from_or_to_no_session_gives_no_lock_event() {
        let t0 = Instant::now();
        let mut core = connected(&ALL, snap(SessionState::NoSession, &[]), t0);
        let out = core.update(snap(SessionState::Locked, &[]), t0 + secs(2));
        assert_eq!(names(&out), [("sessionstate", "Locked")]);
        let out = core.update(snap(SessionState::NoSession, &[]), t0 + secs(4));
        assert_eq!(names(&out), [("sessionstate", "Unknown")]);
    }

    #[test]
    fn a_new_user_gives_session_logon_and_a_gone_user_session_logoff() {
        let t0 = Instant::now();
        let mut core = connected(&ALL, snap(SessionState::Unlocked, &["jahu"]), t0);
        let out = core.update(snap(SessionState::Unlocked, &["zoe", "jahu"]), t0 + secs(2));
        assert_eq!(
            names(&out),
            [
                ("loggedusers", r#"["jahu","zoe"]"#),
                ("lastsystemstatechange", "SessionLogon")
            ]
        );
        let out = core.update(snap(SessionState::Unlocked, &[]), t0 + secs(4));
        assert_eq!(
            names(&out),
            [
                ("loggedusers", "[]"),
                ("lastsystemstatechange", "SessionLogoff"),
                ("lastsystemstatechange", "SessionLogoff")
            ]
        );
    }

    #[test]
    fn the_first_snapshot_gives_no_session_event() {
        let t0 = Instant::now();
        let mut core = Core::new(&ALL, Some(BOOT));
        core.update(snap(SessionState::Locked, &["jahu"]), t0);
        let states = core.connect(t0);
        assert!(
            names(&states).contains(&("lastsystemstatechange", "ApplicationStarted")),
            "{:?}",
            names(&states)
        );
    }

    #[test]
    fn an_unchanged_state_is_sent_again_after_30_s_and_not_before() {
        let t0 = Instant::now();
        let first = Snapshot {
            display: Some(false),
            gpu: Some(GpuSample {
                load: 5,
                power: None,
            }),
            ..snap(SessionState::Unlocked, &["jahu"])
        };
        let mut core = connected(&ALL, first.clone(), t0);
        for s in (2..30).step_by(2) {
            assert!(core.update(first.clone(), t0 + secs(s)).is_empty(), "{s} s");
        }
        assert_eq!(
            names(&core.update(first.clone(), t0 + secs(30))),
            [
                ("sessionstate", "Unlocked"),
                ("gpuload", "5"),
                ("loggedusers", r#"["jahu"]"#),
                ("display", "OFF"),
            ]
        );
        assert!(core.update(first.clone(), t0 + secs(32)).is_empty());
        assert_eq!(core.update(first, t0 + secs(60)).len(), 4);
    }

    #[test]
    fn not_known_keeps_the_last_session_state() {
        let t0 = Instant::now();
        let mut core = connected(&ALL, snap(SessionState::Locked, &[]), t0);
        assert!(
            core.update(snap(SessionState::NotKnown, &[]), t0 + secs(2))
                .is_empty()
        );
        let out = core.update(snap(SessionState::NotKnown, &[]), t0 + secs(30));
        assert_eq!(names(&out)[0], ("sessionstate", "Locked"));
    }

    #[test]
    fn not_known_without_a_last_state_gives_unlocked() {
        let t0 = Instant::now();
        let core_states = connected(&ALL, snap(SessionState::NotKnown, &[]), t0).connect(t0);
        assert_eq!(names(&core_states)[0], ("sessionstate", "Unlocked"));
    }

    fn input(t: i64) -> Snapshot {
        Snapshot {
            last_input: Some(t),
            ..Snapshot::default()
        }
    }

    #[test]
    fn lastactive_is_not_published_before_the_first_counted_input() {
        let t0 = Instant::now();
        let mut core = connected(&ALL, Snapshot::default(), t0);
        let states = core.connect(t0);
        assert!(
            !names(&states).iter().any(|(n, _)| *n == "lastactive"),
            "{:?}",
            names(&states)
        );
        let out = core.update(input(BOOT + 100), t0 + secs(2));
        assert_eq!(names(&out), [("lastactive", "2026-08-28 04:56:35Z")]);
    }

    #[test]
    fn lastactive_follows_the_rate_rule() {
        let t0 = Instant::now();
        let mut core = connected(&ALL, input(BOOT), t0);
        // Input goes on: a change is published at most every 30 s.
        assert!(core.update(input(BOOT + 2), t0 + secs(2)).is_empty());
        assert!(core.update(input(BOOT + 28), t0 + secs(28)).is_empty());
        let out = core.update(input(BOOT + 30), t0 + secs(30));
        assert!(
            names(&out).contains(&("lastactive", "2026-08-28 04:55:25Z")),
            "{:?}",
            names(&out)
        );
        // A pending change goes out once 30 s have passed, with no new input.
        assert!(core.update(input(BOOT + 40), t0 + secs(40)).is_empty());
        let out = core.update(input(BOOT + 40), t0 + secs(60));
        assert!(
            names(&out).contains(&("lastactive", "2026-08-28 04:55:35Z")),
            "{:?}",
            names(&out)
        );
        // No change, no lastactive.
        let out = core.update(input(BOOT + 40), t0 + secs(90));
        assert!(
            !names(&out).iter().any(|(n, _)| *n == "lastactive"),
            "{:?}",
            names(&out)
        );
        // Input after 60 s or more without input: at once.
        let out = core.update(input(BOOT + 100), t0 + secs(100));
        assert_eq!(names(&out), [("lastactive", "2026-08-28 04:56:35Z")]);
        // Input after less than 60 s without input, within 30 s: waits.
        assert!(core.update(input(BOOT + 159), t0 + secs(110)).is_empty());
    }

    #[test]
    fn only_supplied_entities_are_published() {
        static SOME: [&Entity; 1] = [&entities::SESSIONSTATE];
        let t0 = Instant::now();
        let full = Snapshot {
            last_input: Some(BOOT),
            display: Some(true),
            ..snap(SessionState::Unlocked, &["jahu"])
        };
        let mut core = connected(&SOME, full, t0);
        assert_eq!(names(&core.connect(t0)), [("sessionstate", "Unlocked")]);
        let out = core.update(
            Snapshot {
                last_input: Some(BOOT + 100),
                display: Some(false),
                ..snap(SessionState::Locked, &["zoe"])
            },
            t0 + secs(2),
        );
        assert_eq!(names(&out), [("sessionstate", "Locked")]);
        assert!(core.event("Suspend").is_empty());
    }

    #[test]
    fn a_system_event_sets_lastsystemstatechange() {
        let t0 = Instant::now();
        let mut core = connected(&ALL, Snapshot::default(), t0);
        assert_eq!(
            names(&core.event("Suspend")),
            [("lastsystemstatechange", "Suspend")]
        );
        assert!(names(&core.connect(t0)).contains(&("lastsystemstatechange", "Suspend")));
    }

    #[test]
    fn gpupower_is_absent_without_a_power_source_and_gpu_goes_without_a_sample() {
        let t0 = Instant::now();
        let with_gpu = Snapshot {
            gpu: Some(GpuSample {
                load: 12,
                power: None,
            }),
            ..Snapshot::default()
        };
        let mut core = connected(&ALL, with_gpu, t0);
        let states = core.connect(t0);
        assert!(names(&states).contains(&("gpuload", "12")));
        assert!(!names(&states).iter().any(|(n, _)| *n == "gpupower"));
        core.update(Snapshot::default(), t0 + secs(2));
        assert!(
            !names(&core.connect(t0 + secs(2)))
                .iter()
                .any(|(n, _)| *n == "gpuload")
        );
    }
}
