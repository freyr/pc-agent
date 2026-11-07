//! The rules of the Linux sources, on plain Rust values. The logind reads are
//! in `logind`, the Hyprland socket and the DRM reads in `hyprland` and `drm`.

use serde::{Deserialize, Deserializer};

use crate::core::SessionState;

/// One logind session: the fields of `ListSessions` and the session
/// properties that the rules use.
#[derive(Clone, Debug, PartialEq)]
pub struct Session {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn session(user: &str, class: &str, kind: &str, remote: bool, active: bool) -> Session {
        Session {
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
}
