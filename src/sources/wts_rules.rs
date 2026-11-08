//! The rules of the Windows session, user and display sources, on plain Rust
//! values, so that the tests run on each OS. The WTS calls and the power
//! notification are in `windows::wts` and `windows::service`.

use std::sync::atomic::{AtomicI32, Ordering};

use crate::core::SessionState;

/// `WTSGetActiveConsoleSessionId` when no session is attached to the console.
pub const NO_CONSOLE: u32 = 0xFFFF_FFFF;
/// `SessionFlags` of `WTSINFOEX_LEVEL1_W`.
pub const WTS_SESSIONSTATE_LOCK: i32 = 0;
pub const WTS_SESSIONSTATE_UNLOCK: i32 = 1;
/// `WTS_CONNECTSTATE_CLASS` values.
pub const WTS_ACTIVE: i32 = 0;
pub const WTS_DISCONNECTED: i32 = 4;

/// One WTS session: the fields of `WTSINFOEX_LEVEL1_W` that the rules use.
#[derive(Clone, Debug, PartialEq)]
pub struct WtsSession {
    pub id: u32,
    /// `SessionState`, a `WTS_CONNECTSTATE_CLASS` value.
    pub state: i32,
    /// `SessionFlags`: `WTS_SESSIONSTATE_LOCK`, `_UNLOCK` or `_UNKNOWN` (-1).
    pub flags: i32,
    pub user: String,
}

/// The session state of the console session `console_id`. `query` reads the
/// session; `None` from it (a failed read) gives no value, so the core keeps
/// the last one.
pub fn session_state(
    console_id: u32,
    query: impl FnOnce(u32) -> Option<WtsSession>,
) -> Option<SessionState> {
    if console_id == NO_CONSOLE {
        return Some(SessionState::NoSession);
    }
    let session = query(console_id)?;
    Some(if session.user.is_empty() {
        SessionState::NoSession
    } else {
        match session.flags {
            WTS_SESSIONSTATE_LOCK => SessionState::Locked,
            WTS_SESSIONSTATE_UNLOCK => SessionState::Unlocked,
            _ => SessionState::NotKnown,
        }
    })
}

/// The user names of the sessions in the state `WTSActive` or
/// `WTSDisconnected`, without session 0 and empty names.
pub fn logged_users(sessions: &[WtsSession]) -> Vec<String> {
    sessions
        .iter()
        .filter(|s| s.id != 0 && !s.user.is_empty())
        .filter(|s| s.state == WTS_ACTIVE || s.state == WTS_DISCONNECTED)
        .map(|s| s.user.clone())
        .collect()
}

/// The last `GUID_CONSOLE_DISPLAY_STATE` value of the power notification.
#[derive(Debug)]
pub struct DisplayCell(AtomicI32);

impl DisplayCell {
    pub const fn new() -> Self {
        DisplayCell(AtomicI32::new(i32::MIN))
    }

    /// Stores a notification value: 0 off, 1 on, 2 dimmed.
    pub fn record(&self, raw: i32) {
        self.0.store(raw, Ordering::Relaxed);
    }

    /// `true` when the display is on; `None` before the first notification.
    pub fn get(&self) -> Option<bool> {
        match self.0.load(Ordering::Relaxed) {
            1 => Some(true),
            0 | 2 => Some(false),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: u32, state: i32, flags: i32, user: &str) -> WtsSession {
        WtsSession {
            id,
            state,
            flags,
            user: user.to_string(),
        }
    }

    fn console(flags: i32, user: &str) -> impl FnOnce(u32) -> Option<WtsSession> {
        let user = user.to_string();
        move |id| Some(session(id, WTS_ACTIVE, flags, &user))
    }

    #[test]
    fn the_lock_flag_of_the_console_session_gives_locked_or_unlocked() {
        assert_eq!(
            session_state(1, console(WTS_SESSIONSTATE_LOCK, "jahu")),
            Some(SessionState::Locked)
        );
        assert_eq!(
            session_state(1, console(WTS_SESSIONSTATE_UNLOCK, "jahu")),
            Some(SessionState::Unlocked)
        );
    }

    #[test]
    fn no_console_session_or_no_user_gives_unknown() {
        let no_query = |_| -> Option<WtsSession> { panic!("no query expected") };
        assert_eq!(
            session_state(NO_CONSOLE, no_query),
            Some(SessionState::NoSession)
        );
        assert_eq!(
            session_state(1, console(WTS_SESSIONSTATE_UNLOCK, "")),
            Some(SessionState::NoSession)
        );
        assert_eq!(
            session_state(1, console(WTS_SESSIONSTATE_LOCK, "")),
            Some(SessionState::NoSession)
        );
    }

    #[test]
    fn the_unknown_flag_with_a_user_keeps_the_last_state() {
        assert_eq!(
            session_state(1, console(-1, "jahu")),
            Some(SessionState::NotKnown)
        );
    }

    #[test]
    fn a_failed_read_of_the_console_session_gives_no_value() {
        assert_eq!(session_state(1, |_| None), None);
    }

    #[test]
    fn the_console_session_id_is_the_one_queried() {
        let mut asked = None;
        session_state(7, |id| {
            asked = Some(id);
            None
        });
        assert_eq!(asked, Some(7));
    }

    #[test]
    fn logged_users_are_active_and_disconnected_sessions_with_a_name() {
        let sessions = [
            session(0, WTS_DISCONNECTED, -1, "SYSTEM"),
            session(1, WTS_ACTIVE, WTS_SESSIONSTATE_UNLOCK, "jahu"),
            session(2, WTS_DISCONNECTED, WTS_SESSIONSTATE_LOCK, "zoe"),
            session(3, 1, -1, "connected"), // WTSConnected: login screen
            session(4, 6, -1, "listen"),    // WTSListen
            session(5, WTS_ACTIVE, WTS_SESSIONSTATE_UNLOCK, ""),
            session(65536, WTS_DISCONNECTED, -1, "jahu"),
        ];
        assert_eq!(logged_users(&sessions), ["jahu", "zoe", "jahu"]);
        assert_eq!(logged_users(&[]), Vec::<String>::new());
    }

    #[test]
    fn the_display_has_no_value_before_the_first_notification() {
        assert_eq!(DisplayCell::new().get(), None);
    }

    #[test]
    fn display_1_is_on_and_0_and_2_are_off() {
        let cell = DisplayCell::new();
        cell.record(1);
        assert_eq!(cell.get(), Some(true));
        cell.record(0);
        assert_eq!(cell.get(), Some(false));
        cell.record(1);
        cell.record(2);
        assert_eq!(cell.get(), Some(false));
    }
}
