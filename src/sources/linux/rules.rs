//! The rules of the Linux sources, on plain Rust values. The logind reads are
//! in `logind`, the Hyprland socket and the DRM reads in `hyprland` and `drm`.

use serde::{Deserialize, Deserializer};

use std::time::{Duration, Instant};

use crate::core::{GpuSample, SessionState};

/// One logind session: the fields of `ListSessions` and the session
/// properties that the rules use.
#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    pub id: String,
    pub seat: String,
    pub uid: u32,
    pub user: String,
    /// `Class`: `user`, `greeter`, `lock-screen`, ...
    pub class: String,
    /// `Type`: `wayland`, `x11`, `tty`, `unspecified`, ...
    pub kind: String,
    pub remote: bool,
    pub active: bool,
}

/// A local graphical user session (step 1 of the spec, except `Active`).
fn is_desktop(s: &Session) -> bool {
    s.class == "user" && !s.remote && (s.kind == "wayland" || s.kind == "x11")
}

/// The active local graphical user session, if one exists.
pub fn active_session(sessions: &[Session]) -> Option<&Session> {
    sessions.iter().find(|s| is_desktop(s) && s.active)
}

/// The user names of the local graphical user sessions, active or not.
pub fn logged_users(sessions: &[Session]) -> Vec<String> {
    sessions
        .iter()
        .filter(|s| is_desktop(s))
        .map(|s| s.user.clone())
        .collect()
}

/// One monitor object of the Hyprland `j/monitors` reply.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct Monitor {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub disabled: bool,
    #[serde(rename = "dpmsStatus", default)]
    pub dpms_status: bool,
    /// `None` when the field is missing (an old Hyprland). Hyprland writes
    /// `null` when nothing blocks: that is an empty list.
    #[serde(
        rename = "solitaryBlockedBy",
        default,
        deserialize_with = "null_is_empty"
    )]
    pub solitary_blocked_by: Option<Vec<String>>,
}

fn null_is_empty<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<String>>, D::Error> {
    Ok(Some(
        Option::<Vec<String>>::deserialize(d)?.unwrap_or_default(),
    ))
}

/// The monitor list from the `j/monitors` reply, or `None` when the reply is
/// not a JSON array of monitors.
pub fn parse_monitors(reply: &str) -> Option<Vec<Monitor>> {
    serde_json::from_str(reply).ok()
}

/// The lock state that the monitor list shows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LockRead {
    Locked,
    Unlocked,
    /// No `LOCK`, but `WORKSPACE` (or no `solitaryBlockedBy` field).
    NotKnown,
}

/// Step 3 of the spec on the monitor list.
pub fn lock_read(monitors: &[Monitor]) -> LockRead {
    let has = |reason: &str| {
        monitors.iter().any(|m| {
            m.solitary_blocked_by
                .as_ref()
                .is_some_and(|list| list.iter().any(|r| r == reason))
        })
    };
    if has("LOCK") {
        LockRead::Locked
    } else if has("WORKSPACE") || monitors.iter().any(|m| m.solitary_blocked_by.is_none()) {
        LockRead::NotKnown
    } else {
        LockRead::Unlocked
    }
}

/// `true` when one or more monitors are enabled and have DPMS on.
pub fn display_from_monitors(monitors: &[Monitor]) -> bool {
    monitors.iter().any(|m| !m.disabled && m.dpms_status)
}

/// One DRM connector: the trimmed `status` and `dpms` files.
#[derive(Clone, Debug, PartialEq)]
pub struct Connector {
    /// The connector directory name, for example `card1-DP-1`.
    pub name: String,
    pub status: String,
    pub dpms: String,
}

/// The display state from the DRM connectors: `None` when no connector is
/// connected, else `true` when one or more connected connectors show `On`.
pub fn display_from_drm(connectors: &[Connector]) -> Option<bool> {
    let mut connected = connectors.iter().filter(|c| c.status == "connected");
    let first = connected.next()?;
    Some(first.dpms == "On" || connected.any(|c| c.dpms == "On"))
}

/// The session state of steps 2 to 4 of the spec. It keeps the last value
/// for the "not known" rule.
#[derive(Debug)]
pub struct LockTracker {
    /// `Locked` or `Unlocked`; `None` without an active session.
    last: Option<SessionState>,
}

impl LockTracker {
    pub const fn new() -> Self {
        LockTracker { last: None }
    }

    /// `active` is `true` when an active session exists. `read` is the lock
    /// state of the Hyprland socket, or `None` when the socket did not answer.
    pub fn update(&mut self, active: bool, read: Option<LockRead>) -> SessionState {
        if !active {
            self.last = None;
            return SessionState::NoSession;
        }
        let state = match read {
            Some(LockRead::Locked) => SessionState::Locked,
            Some(LockRead::Unlocked) | None => SessionState::Unlocked,
            Some(LockRead::NotKnown) => self.last.unwrap_or(SessionState::Unlocked),
        };
        self.last = Some(state);
        state
    }
}

/// The event types of `input-event-codes.h` that the activity test uses.
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const EV_ABS: u16 = 0x03;

/// `BTN_JOYSTICK` (0x120) to the end of the `BTN_GAMEPAD` range; it holds
/// `BTN_SOUTH` (0x130).
const GAMEPAD_KEYS: std::ops::RangeInclusive<u16> = 0x120..=0x13f;

const ABS_RX: u16 = 0x03;
const ABS_RY: u16 = 0x04;
/// `ABS_X`, `ABS_Y`, `ABS_RX`, `ABS_RY`.
const STICK_AXES: [u16; 4] = [0x00, 0x01, ABS_RX, ABS_RY];
/// `ABS_HAT0X` to `ABS_HAT3Y`.
const HAT_AXES: std::ops::RangeInclusive<u16> = 0x10..=0x17;
/// `ABS_Z`, `ABS_RZ`: triggers, or the right stick on a gamepad with no
/// `ABS_RX` and no `ABS_RY`.
const Z_AXES: [u16; 2] = [0x02, 0x05];
/// `ABS_GAS`, `ABS_BRAKE`.
const PEDAL_AXES: [u16; 2] = [0x09, 0x0a];

/// The `absinfo` values of one absolute axis that the activity test uses.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AxisInfo {
    pub min: i32,
    pub max: i32,
    pub flat: i32,
    pub fuzz: i32,
}

/// The capabilities of one input device that the activity test uses.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InputCaps {
    /// The device has a key in the `BTN_JOYSTICK` or `BTN_GAMEPAD` range.
    pub gamepad: bool,
    /// The absinfo of each absolute axis of the device, by axis code.
    pub axes: std::collections::HashMap<u16, AxisInfo>,
    /// A gamepad with no `ABS_RX` and no `ABS_RY`: `ABS_Z` and `ABS_RZ` are
    /// its right stick.
    pub z_is_stick: bool,
}

impl InputCaps {
    /// The capabilities from the gamepad flag and the absinfo of the axes.
    pub fn new(gamepad: bool, axes: std::collections::HashMap<u16, AxisInfo>) -> Self {
        let z_is_stick = gamepad && !axes.contains_key(&ABS_RX) && !axes.contains_key(&ABS_RY);
        InputCaps {
            gamepad,
            axes,
            z_is_stick,
        }
    }
}

/// `true` when one of the key codes makes the device a gamepad.
pub fn is_gamepad(keys: impl IntoIterator<Item = u16>) -> bool {
    keys.into_iter().any(|k| GAMEPAD_KEYS.contains(&k))
}

/// The activity test of the spec for one event (type, code, value).
/// It reads the values and forgets them; nothing of the event is kept.
pub fn is_activity(caps: &InputCaps, kind: u16, code: u16, value: i32) -> bool {
    match kind {
        EV_KEY => value == 1,
        EV_REL => true,
        EV_ABS if !caps.gamepad => true,
        EV_ABS => {
            let Some(a) = caps.axes.get(&code) else {
                return false;
            };
            // In tenths and doubled where needed, so that 10 % of the range
            // and the center (min + max) / 2 stay integers.
            let (v, min, max, flat) = (
                i64::from(value),
                i64::from(a.min),
                i64::from(a.max),
                i64::from(a.flat),
            );
            let range = max - min;
            match axis_role(caps, code) {
                // |v - center| > max(flat, range / 10)
                AxisRole::Stick => 10 * (2 * v - (min + max)).abs() > 2 * (10 * flat).max(range),
                // v > min + range / 10
                AxisRole::Trigger => 10 * (v - min) > range,
                AxisRole::Ignored => false,
            }
        }
        _ => false,
    }
}

/// The role of an absolute axis of a gamepad in the activity test.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AxisRole {
    /// Counts outside the larger of `flat` and 10 % of the range.
    Stick,
    /// Counts above the minimum plus 10 % of the range.
    Trigger,
    /// Never counts.
    Ignored,
}

/// The role of the axis `code` on the gamepad `caps`.
pub fn axis_role(caps: &InputCaps, code: u16) -> AxisRole {
    if STICK_AXES.contains(&code)
        || HAT_AXES.contains(&code)
        || (caps.z_is_stick && Z_AXES.contains(&code))
    {
        AxisRole::Stick
    } else if Z_AXES.contains(&code) || PEDAL_AXES.contains(&code) {
        AxisRole::Trigger
    } else {
        AxisRole::Ignored
    }
}

/// The interval of the NVML query.
pub const GPU_QUERY: Duration = Duration::from_secs(30);
/// The delay before a new NVML init after a failure.
pub const GPU_RETRY: Duration = Duration::from_secs(300);

/// What the GPU source logs after one poll.
#[derive(Clone, Debug, PartialEq)]
pub enum GpuLog {
    /// The init failed (warning, once for each outage).
    InitFailed(String),
    /// The utilization query failed (warning, once for each outage).
    QueryFailed(String),
    /// Only the power query failed (warning, once until it works again).
    PowerFailed(String),
    /// A sample after an outage (info).
    Recovered,
}

/// The timing and failure rules of the GPU source. `H` is the NVML handle;
/// `init` and `query` are the NVML calls.
#[derive(Debug)]
pub struct GpuPoller<H> {
    handle: Option<H>,
    next_init: Option<Instant>,
    next_query: Option<Instant>,
    last: Option<GpuSample>,
    warned: bool,
    warned_power: bool,
}

impl<H> GpuPoller<H> {
    pub const fn new() -> Self {
        GpuPoller {
            handle: None,
            next_init: None,
            next_query: None,
            last: None,
            warned: false,
            warned_power: false,
        }
    }

    /// The GPU sample at `now` and the lines to log. `query` gives the
    /// utilization percent and the power in watts (or the power error).
    pub fn poll(
        &mut self,
        now: Instant,
        init: impl FnOnce() -> Result<H, String>,
        query: impl FnOnce(&H) -> Result<(u32, Result<f64, String>), String>,
    ) -> (Option<GpuSample>, Vec<GpuLog>) {
        let mut logs = Vec::new();
        if self.handle.is_none() {
            if self.next_init.is_some_and(|t| now < t) {
                return (None, logs);
            }
            match init() {
                Ok(handle) => {
                    self.handle = Some(handle);
                    self.next_init = None;
                    self.next_query = None;
                }
                Err(e) => {
                    self.next_init = Some(now + GPU_RETRY);
                    if !self.warned {
                        self.warned = true;
                        logs.push(GpuLog::InitFailed(e));
                    }
                    return (None, logs);
                }
            }
        }
        if self.next_query.is_some_and(|t| now < t) {
            return (self.last, logs);
        }
        let Some(handle) = &self.handle else {
            return (None, logs);
        };
        match query(handle) {
            Ok((load, power)) => {
                if self.warned {
                    self.warned = false;
                    logs.push(GpuLog::Recovered);
                }
                let power = match power {
                    Ok(watts) => {
                        self.warned_power = false;
                        Some(watts)
                    }
                    Err(e) => {
                        if !self.warned_power {
                            self.warned_power = true;
                            logs.push(GpuLog::PowerFailed(e));
                        }
                        None
                    }
                };
                self.last = Some(GpuSample { load, power });
                self.next_query = Some(now + GPU_QUERY);
            }
            Err(e) => {
                self.handle = None;
                self.last = None;
                self.next_query = None;
                self.next_init = Some(now + GPU_RETRY);
                if !self.warned {
                    self.warned = true;
                    logs.push(GpuLog::QueryFailed(e));
                }
            }
        }
        (self.last, logs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(user: &str, class: &str, kind: &str, remote: bool, active: bool) -> Session {
        Session {
            id: "2".to_string(),
            seat: "seat0".to_string(),
            uid: 1000,
            user: user.to_string(),
            class: class.to_string(),
            kind: kind.to_string(),
            remote,
            active,
        }
    }

    #[test]
    fn a_wayland_user_session_and_the_greeter() {
        let list = [
            session("sddm", "greeter", "wayland", false, false),
            session("alice", "user", "wayland", false, true),
        ];
        assert_eq!(
            active_session(&list).map(|s| s.user.as_str()),
            Some("alice")
        );
        assert_eq!(logged_users(&list), ["alice"]);
    }

    #[test]
    fn an_active_greeter_is_not_the_session() {
        let list = [session("sddm", "greeter", "wayland", false, true)];
        assert_eq!(active_session(&list), None);
        assert!(logged_users(&list).is_empty());
    }

    #[test]
    fn a_tty_session_only_gives_no_session() {
        let list = [session("alice", "user", "tty", false, true)];
        assert_eq!(active_session(&list), None);
        assert!(logged_users(&list).is_empty());
    }

    #[test]
    fn a_remote_session_only_gives_no_session() {
        let list = [session("alice", "user", "wayland", true, true)];
        assert_eq!(active_session(&list), None);
        assert!(logged_users(&list).is_empty());
    }

    #[test]
    fn two_user_sessions_the_active_one_decides() {
        let list = [
            Session {
                uid: 1001,
                ..session("bob", "user", "wayland", false, false)
            },
            session("alice", "user", "wayland", false, true),
        ];
        assert_eq!(active_session(&list).map(|s| s.uid), Some(1000));
        assert_eq!(logged_users(&list), ["bob", "alice"]);
    }

    #[test]
    fn an_x11_session_works() {
        let list = [session("alice", "user", "x11", false, true)];
        assert_eq!(
            active_session(&list).map(|s| s.user.as_str()),
            Some("alice")
        );
        assert_eq!(logged_users(&list), ["alice"]);
    }

    #[test]
    fn no_sessions_give_no_session() {
        assert_eq!(active_session(&[]), None);
        assert!(logged_users(&[]).is_empty());
    }

    fn monitors(json: &str) -> Vec<Monitor> {
        parse_monitors(json).expect("fixture parses")
    }

    #[test]
    fn no_lock_and_no_workspace_is_unlocked() {
        let m = monitors(
            r#"[{"name":"DP-1","disabled":false,"dpmsStatus":true,"solitaryBlockedBy":["CANDIDATE","WINDOWED"]}]"#,
        );
        assert_eq!(lock_read(&m), LockRead::Unlocked);
    }

    #[test]
    fn a_null_list_is_unlocked() {
        let m = monitors(
            r#"[{"name":"DP-1","disabled":false,"dpmsStatus":true,"solitaryBlockedBy":null}]"#,
        );
        assert_eq!(lock_read(&m), LockRead::Unlocked);
    }

    #[test]
    fn lock_on_one_of_two_monitors_is_locked() {
        let m = monitors(
            r#"[{"name":"DP-1","disabled":false,"dpmsStatus":true,"solitaryBlockedBy":["CANDIDATE"]},
                {"name":"DP-2","disabled":false,"dpmsStatus":true,"solitaryBlockedBy":["LOCK"]}]"#,
        );
        assert_eq!(lock_read(&m), LockRead::Locked);
    }

    #[test]
    fn workspace_only_is_not_known() {
        let m = monitors(
            r#"[{"name":"DP-1","disabled":false,"dpmsStatus":true,"solitaryBlockedBy":["WORKSPACE"]}]"#,
        );
        assert_eq!(lock_read(&m), LockRead::NotKnown);
    }

    #[test]
    fn a_missing_field_is_not_known() {
        let m = monitors(r#"[{"name":"DP-1","disabled":false,"dpmsStatus":true}]"#);
        assert_eq!(lock_read(&m), LockRead::NotKnown);
    }

    #[test]
    fn a_reply_that_is_not_a_monitor_list_gives_none() {
        assert_eq!(parse_monitors("unknown request"), None);
        assert_eq!(parse_monitors(r#"{"name":"DP-1"}"#), None);
    }

    #[test]
    fn one_monitor_with_dpms_on_is_on() {
        let m = monitors(
            r#"[{"name":"DP-1","disabled":false,"dpmsStatus":false},
                {"name":"DP-2","disabled":false,"dpmsStatus":true}]"#,
        );
        assert!(display_from_monitors(&m));
    }

    #[test]
    fn all_dpms_off_is_off() {
        let m = monitors(
            r#"[{"name":"DP-1","disabled":false,"dpmsStatus":false},
                {"name":"DP-2","disabled":false,"dpmsStatus":false}]"#,
        );
        assert!(!display_from_monitors(&m));
    }

    #[test]
    fn disabled_with_dpms_on_is_off() {
        let m = monitors(r#"[{"name":"DP-1","disabled":true,"dpmsStatus":true}]"#);
        assert!(!display_from_monitors(&m));
    }

    fn connector(status: &str, dpms: &str) -> Connector {
        Connector {
            name: "card1-DP-1".to_string(),
            status: status.to_string(),
            dpms: dpms.to_string(),
        }
    }

    #[test]
    fn drm_connected_and_on_is_on() {
        assert_eq!(
            display_from_drm(&[connector("connected", "On")]),
            Some(true)
        );
    }

    #[test]
    fn drm_connected_off_and_disconnected_on_is_off() {
        let list = [
            connector("connected", "Off"),
            connector("disconnected", "On"),
        ];
        assert_eq!(display_from_drm(&list), Some(false));
    }

    #[test]
    fn drm_without_a_connected_connector_gives_none() {
        assert_eq!(display_from_drm(&[]), None);
        assert_eq!(display_from_drm(&[connector("disconnected", "On")]), None);
    }

    #[test]
    fn no_active_session_gives_no_session() {
        let mut t = LockTracker::new();
        assert_eq!(t.update(false, None), SessionState::NoSession);
    }

    #[test]
    fn the_lock_read_gives_the_state() {
        let mut t = LockTracker::new();
        assert_eq!(t.update(true, Some(LockRead::Locked)), SessionState::Locked);
        assert_eq!(
            t.update(true, Some(LockRead::Unlocked)),
            SessionState::Unlocked
        );
    }

    #[test]
    fn not_known_without_a_last_value_is_unlocked() {
        let mut t = LockTracker::new();
        assert_eq!(
            t.update(true, Some(LockRead::NotKnown)),
            SessionState::Unlocked
        );
    }

    #[test]
    fn not_known_keeps_the_last_value() {
        let mut t = LockTracker::new();
        t.update(true, Some(LockRead::Locked));
        assert_eq!(
            t.update(true, Some(LockRead::NotKnown)),
            SessionState::Locked
        );
        t.update(true, Some(LockRead::Unlocked));
        assert_eq!(
            t.update(true, Some(LockRead::NotKnown)),
            SessionState::Unlocked
        );
    }

    #[test]
    fn no_session_clears_the_last_value() {
        let mut t = LockTracker::new();
        t.update(true, Some(LockRead::Locked));
        t.update(false, None);
        assert_eq!(
            t.update(true, Some(LockRead::NotKnown)),
            SessionState::Unlocked
        );
    }

    #[test]
    fn a_socket_without_an_answer_is_unlocked() {
        let mut t = LockTracker::new();
        t.update(true, Some(LockRead::Locked));
        assert_eq!(t.update(true, None), SessionState::Unlocked);
    }

    // The activity test. Fixtures: an Xbox-style pad and a DualSense.

    const ABS_X: u16 = 0x00;
    const ABS_RX: u16 = 0x03;
    const ABS_Z: u16 = 0x02;
    const ABS_RZ: u16 = 0x05;
    const ABS_HAT0X: u16 = 0x10;
    const ABS_HAT0Y: u16 = 0x11;
    const ABS_MISC: u16 = 0x28;
    const EV_SYN: u16 = 0x00;
    const EV_MSC: u16 = 0x04;
    const KEY_A: u16 = 30;
    const REL_X: u16 = 0x00;

    fn axis(min: i32, max: i32, flat: i32) -> AxisInfo {
        AxisInfo {
            min,
            max,
            flat,
            fuzz: 0,
        }
    }

    fn pad(axes: &[(u16, AxisInfo)]) -> InputCaps {
        InputCaps::new(true, axes.iter().copied().collect())
    }

    #[test]
    fn z_is_stick_is_set_at_open() {
        assert!(dinput().z_is_stick);
        assert!(!xbox().z_is_stick);
        assert!(!dualsense().z_is_stick);
        // Only a gamepad has its right stick on Z and RZ.
        let tablet = InputCaps::new(false, [(ABS_Z, axis(0, 255, 0))].into_iter().collect());
        assert!(!tablet.z_is_stick);
    }

    /// Sticks -32768..32767 flat 128 (left) and 4095 (right), triggers
    /// 0..1023, hats -1..1, and an `ABS_MISC` axis.
    fn xbox() -> InputCaps {
        pad(&[
            (ABS_X, axis(-32768, 32767, 128)),
            (ABS_RX, axis(-32768, 32767, 4095)),
            (ABS_Z, axis(0, 1023, 0)),
            (ABS_RZ, axis(0, 1023, 0)),
            (ABS_HAT0X, axis(-1, 1, 0)),
            (ABS_HAT0Y, axis(-1, 1, 0)),
            (ABS_MISC, axis(0, 255, 0)),
        ])
    }

    /// Sticks X, Y, RX, RY 0..255 flat 0 (center 127.5), triggers Z, RZ
    /// 0..255 (the hid-playstation layout).
    fn dualsense() -> InputCaps {
        pad(&[
            (ABS_X, axis(0, 255, 0)),
            (0x01, axis(0, 255, 0)),
            (ABS_RX, axis(0, 255, 0)),
            (0x04, axis(0, 255, 0)),
            (ABS_Z, axis(0, 255, 0)),
            (ABS_RZ, axis(0, 255, 0)),
        ])
    }

    fn keyboard() -> InputCaps {
        InputCaps::default()
    }

    #[test]
    fn gamepad_keys() {
        assert!(is_gamepad([KEY_A, 0x130]));
        assert!(is_gamepad([0x120]));
        assert!(is_gamepad([0x12f]));
        // A pad with only BTN_EAST, and the end of the BTN_GAMEPAD range.
        assert!(is_gamepad([0x131]));
        assert!(is_gamepad([0x13f]));
        assert!(!is_gamepad([0x140]));
        assert!(!is_gamepad([KEY_A, 0x110, 0x14a]));
        assert!(!is_gamepad([]));
    }

    #[test]
    fn a_key_press_counts() {
        assert!(is_activity(&keyboard(), EV_KEY, KEY_A, 1));
    }

    #[test]
    fn a_key_release_and_autorepeat_do_not_count() {
        assert!(!is_activity(&keyboard(), EV_KEY, KEY_A, 0));
        assert!(!is_activity(&keyboard(), EV_KEY, KEY_A, 2));
    }

    #[test]
    fn rel_counts() {
        assert!(is_activity(&keyboard(), EV_REL, REL_X, -1));
        assert!(is_activity(&keyboard(), EV_REL, REL_X, 0));
    }

    #[test]
    fn syn_and_msc_do_not_count() {
        assert!(!is_activity(&keyboard(), EV_SYN, 0, 0));
        assert!(!is_activity(&keyboard(), EV_MSC, 4, 458756));
    }

    #[test]
    fn a_stick_inside_the_dead_zone_does_not_count() {
        // Left: flat 128 < 10 % of 65535 (6553.5), so 6553.5 decides.
        assert!(!is_activity(&xbox(), EV_ABS, ABS_X, 0));
        assert!(!is_activity(&xbox(), EV_ABS, ABS_X, 3000));
        assert!(!is_activity(&xbox(), EV_ABS, ABS_X, -6000));
        // Right: flat 4095 is smaller than 6553.5 too.
        assert!(!is_activity(&xbox(), EV_ABS, ABS_RX, 4500));
        // DualSense: 127.5 +- 25.5.
        assert!(!is_activity(&dualsense(), EV_ABS, ABS_X, 128));
        assert!(!is_activity(&dualsense(), EV_ABS, ABS_X, 150));
    }

    #[test]
    fn a_stick_at_50_percent_counts() {
        assert!(is_activity(&xbox(), EV_ABS, ABS_X, 16384));
        assert!(is_activity(&xbox(), EV_ABS, ABS_RX, -16384));
        assert!(is_activity(&dualsense(), EV_ABS, ABS_X, 255));
        assert!(is_activity(&dualsense(), EV_ABS, ABS_X, 0));
    }

    #[test]
    fn a_stick_exactly_at_the_threshold_does_not_count() {
        // "Farther than": strict. 10 % of 200 = 20 decides here.
        let caps = pad(&[(ABS_X, axis(-100, 100, 0))]);
        assert!(!is_activity(&caps, EV_ABS, ABS_X, 20));
        assert!(!is_activity(&caps, EV_ABS, ABS_X, -20));
        assert!(is_activity(&caps, EV_ABS, ABS_X, 21));
        // flat 300 > 10 % of 2000 = 200: flat decides.
        let caps = pad(&[(ABS_X, axis(-1000, 1000, 300))]);
        assert!(!is_activity(&caps, EV_ABS, ABS_X, 300));
        assert!(!is_activity(&caps, EV_ABS, ABS_X, 250));
        assert!(is_activity(&caps, EV_ABS, ABS_X, 301));
    }

    #[test]
    fn a_trigger_counts_only_above_min_plus_10_percent() {
        // 0..1023: 10 % = 102.3.
        assert!(!is_activity(&xbox(), EV_ABS, ABS_Z, 0));
        assert!(!is_activity(&xbox(), EV_ABS, ABS_RZ, 51));
        assert!(is_activity(&xbox(), EV_ABS, ABS_RZ, 205));
        // 0..255: 10 % = 25.5.
        assert!(!is_activity(&dualsense(), EV_ABS, ABS_Z, 0));
        assert!(!is_activity(&dualsense(), EV_ABS, ABS_Z, 13));
        assert!(is_activity(&dualsense(), EV_ABS, ABS_Z, 51));
    }

    const ABS_Y: u16 = 0x01;
    const ABS_GAS: u16 = 0x09;
    const ABS_BRAKE: u16 = 0x0a;

    /// A DInput-style pad: sticks on X, Y, Z, RZ (0..255), no RX and no RY,
    /// triggers on GAS and BRAKE (0..255).
    fn dinput() -> InputCaps {
        pad(&[
            (ABS_X, axis(0, 255, 0)),
            (ABS_Y, axis(0, 255, 0)),
            (ABS_Z, axis(0, 255, 0)),
            (ABS_RZ, axis(0, 255, 0)),
            (ABS_GAS, axis(0, 255, 0)),
            (ABS_BRAKE, axis(0, 255, 0)),
        ])
    }

    #[test]
    fn z_and_rz_are_a_stick_without_rx_and_ry() {
        // Center 127.5, threshold 25.5.
        assert!(!is_activity(&dinput(), EV_ABS, ABS_Z, 130));
        assert!(!is_activity(&dinput(), EV_ABS, ABS_RZ, 128));
        assert!(is_activity(&dinput(), EV_ABS, ABS_Z, 250));
        assert!(is_activity(&dinput(), EV_ABS, ABS_RZ, 5));
    }

    #[test]
    fn gas_and_brake_are_triggers() {
        assert!(!is_activity(&dinput(), EV_ABS, ABS_GAS, 5));
        assert!(is_activity(&dinput(), EV_ABS, ABS_GAS, 200));
        assert!(!is_activity(&dinput(), EV_ABS, ABS_BRAKE, 0));
        assert!(is_activity(&dinput(), EV_ABS, ABS_BRAKE, 200));
    }

    #[test]
    fn z_and_rz_stay_triggers_with_rx_and_ry() {
        // An xpad-style pad: X, Y, RX, RY sticks; Z and RZ triggers 0..1023.
        let xpad = pad(&[
            (ABS_X, axis(-32768, 32767, 128)),
            (ABS_Y, axis(-32768, 32767, 128)),
            (ABS_RX, axis(-32768, 32767, 128)),
            (0x04, axis(-32768, 32767, 128)),
            (ABS_Z, axis(0, 1023, 0)),
            (ABS_RZ, axis(0, 1023, 0)),
        ]);
        // 400 is near the stick center (511.5) but far above the trigger minimum.
        assert!(is_activity(&xpad, EV_ABS, ABS_Z, 400));
        assert!(!is_activity(&xpad, EV_ABS, ABS_RZ, 51));
        assert!(is_activity(&xpad, EV_ABS, ABS_RZ, 205));
    }

    #[test]
    fn a_hat_away_from_zero_counts() {
        assert!(is_activity(&xbox(), EV_ABS, ABS_HAT0X, -1));
        assert!(is_activity(&xbox(), EV_ABS, ABS_HAT0Y, 1));
        assert!(!is_activity(&xbox(), EV_ABS, ABS_HAT0X, 0));
    }

    #[test]
    fn abs_misc_on_a_gamepad_does_not_count() {
        assert!(!is_activity(&xbox(), EV_ABS, ABS_MISC, 200));
    }

    #[test]
    fn a_gamepad_axis_without_absinfo_does_not_count() {
        assert!(!is_activity(&pad(&[]), EV_ABS, ABS_X, 30000));
    }

    #[test]
    fn abs_on_a_touchpad_counts() {
        let touchpad = InputCaps::new(false, [(ABS_X, axis(0, 1216, 0))].into_iter().collect());
        assert!(is_activity(&touchpad, EV_ABS, ABS_X, 600));
        assert!(is_activity(&touchpad, EV_ABS, ABS_X, 0));
    }

    // The GPU source: fake init and query, injected clock.

    type Query = Result<(u32, Result<f64, String>), String>;

    fn ok(load: u32, watts: f64) -> Query {
        Ok((load, Ok(watts)))
    }

    fn sample(load: u32, power: Option<f64>) -> Option<GpuSample> {
        Some(GpuSample { load, power })
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn no_init() -> Result<(), String> {
        panic!("no init expected")
    }

    fn no_query(_: &()) -> Query {
        panic!("no query expected")
    }

    #[test]
    fn an_init_error_warns_once_and_retries_after_5_min() {
        let t0 = Instant::now();
        let mut p = GpuPoller::<()>::new();
        let lib = "a library is not found".to_string();

        let (s, logs) = p.poll(t0, || Err(lib.clone()), no_query);
        assert_eq!(s, None);
        assert_eq!(logs, [GpuLog::InitFailed(lib.clone())]);

        // No new init before 5 min.
        for t in [2, 30, 298] {
            assert_eq!(p.poll(t0 + secs(t), no_init, no_query), (None, vec![]));
        }

        // The retry at 5 min fails again: no second warning.
        let (s, logs) = p.poll(t0 + secs(300), || Err(lib.clone()), no_query);
        assert_eq!((s, logs), (None, vec![]));
        assert_eq!(p.poll(t0 + secs(598), no_init, no_query), (None, vec![]));

        // The retry at 10 min works: one info line and a sample.
        let (s, logs) = p.poll(t0 + secs(600), || Ok(()), |_| ok(37, 85.3));
        assert_eq!(s, sample(37, Some(85.3)));
        assert_eq!(logs, [GpuLog::Recovered]);
    }

    #[test]
    fn a_working_gpu_is_queried_every_30_s_and_cached_between() {
        let t0 = Instant::now();
        let mut p = GpuPoller::<()>::new();

        let (s, logs) = p.poll(t0, || Ok(()), |_| ok(37, 85.3));
        assert_eq!((s, logs), (sample(37, Some(85.3)), vec![]));

        // Between the queries: the cached sample, no init, no query.
        for t in [2, 16, 28] {
            assert_eq!(
                p.poll(t0 + secs(t), no_init, no_query),
                (sample(37, Some(85.3)), vec![])
            );
        }

        // At 30 s: a new query on the same handle.
        let (s, _) = p.poll(t0 + secs(30), no_init, |_| ok(90, 300.0));
        assert_eq!(s, sample(90, Some(300.0)));
        assert_eq!(
            p.poll(t0 + secs(58), no_init, no_query).0,
            sample(90, Some(300.0))
        );
    }

    #[test]
    fn a_utilization_error_gives_no_sample_and_a_new_init_after_5_min() {
        let t0 = Instant::now();
        let mut p = GpuPoller::<()>::new();
        p.poll(t0, || Ok(()), |_| ok(37, 85.3));

        let err = "GPU is lost".to_string();
        let (s, logs) = p.poll(t0 + secs(30), no_init, |_| Err(err.clone()));
        assert_eq!(s, None);
        assert_eq!(logs, [GpuLog::QueryFailed(err)]);

        // No sample, no init and no query before 5 min.
        assert_eq!(p.poll(t0 + secs(60), no_init, no_query), (None, vec![]));
        assert_eq!(p.poll(t0 + secs(328), no_init, no_query), (None, vec![]));

        let (s, logs) = p.poll(t0 + secs(330), || Ok(()), |_| ok(5, 20.0));
        assert_eq!(s, sample(5, Some(20.0)));
        assert_eq!(logs, [GpuLog::Recovered]);
    }

    #[test]
    fn a_power_error_alone_gives_the_load_without_power() {
        let t0 = Instant::now();
        let mut p = GpuPoller::<()>::new();
        let err = "Not Supported".to_string();

        let (s, logs) = p.poll(t0, || Ok(()), |_| Ok((37, Err(err.clone()))));
        assert_eq!(s, sample(37, None));
        assert_eq!(logs, [GpuLog::PowerFailed(err.clone())]);

        // The next query has the same error: no second warning, same handle.
        let (s, logs) = p.poll(t0 + secs(30), no_init, |_| Ok((40, Err(err.clone()))));
        assert_eq!((s, logs), (sample(40, None), vec![]));

        // Power works again.
        let (s, _) = p.poll(t0 + secs(60), no_init, |_| ok(41, 99.5));
        assert_eq!(s, sample(41, Some(99.5)));
    }
}
