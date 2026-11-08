//! The rules of the Windows sources and of the service controls, on plain
//! Rust values, so that the tests run on each OS. The WTS calls, the power
//! notification and the control handler are in `windows::wts` and
//! `windows::service`.

use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;
use tracing::warn;

use crate::core::{SessionState, SystemEvent};

/// `WTSGetActiveConsoleSessionId` when no session is attached to the console.
pub const NO_CONSOLE: u32 = 0xFFFF_FFFF;
/// `SessionFlags` of `WTSINFOEX_LEVEL1_W`.
pub const WTS_SESSIONSTATE_LOCK: i32 = 0;
pub const WTS_SESSIONSTATE_UNLOCK: i32 = 1;
/// `WTS_CONNECTSTATE_CLASS` values.
pub const WTS_ACTIVE: i32 = 0;
pub const WTS_DISCONNECTED: i32 = 4;

/// The service control codes (`SERVICE_CONTROL_*`) that the handler acts on.
pub const SERVICE_CONTROL_STOP: u32 = 0x01;
pub const SERVICE_CONTROL_POWEREVENT: u32 = 0x0D;
pub const SERVICE_CONTROL_SESSIONCHANGE: u32 = 0x0E;
pub const SERVICE_CONTROL_PRESHUTDOWN: u32 = 0x0F;
/// The `WTS_*` reasons of `SERVICE_CONTROL_SESSIONCHANGE` that give a session event.
pub const WTS_SESSION_LOGON: u32 = 0x5;
pub const WTS_SESSION_LOGOFF: u32 = 0x6;
pub const WTS_SESSION_LOCK: u32 = 0x7;
pub const WTS_SESSION_UNLOCK: u32 = 0x8;
/// The `PBT_*` events of `SERVICE_CONTROL_POWEREVENT`.
pub const PBT_APMSUSPEND: u32 = 0x4;
pub const PBT_APMRESUMEAUTOMATIC: u32 = 0x12;

/// What the service does for one control.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ControlAction {
    /// Stop: publish `offline` and disconnect.
    Stop,
    /// Send `SystemEvent::Shutdown` and wait for the reply, then stop.
    ShutdownThenStop,
    /// Send `SystemEvent::Suspend` and block until the reply, at most the ack wait.
    Suspend,
    /// Send `SystemEvent::Resume`.
    Resume,
    /// Send `SystemEvent::SessionChange`: the core reads the session at once.
    SessionChange,
    /// Nothing to do.
    Ignore,
}

/// The action for the control `control` with its event type (the `WTS_*`
/// reason or the `PBT_*` event; 0 for the other controls).
pub fn control_action(control: u32, event_type: u32) -> ControlAction {
    match (control, event_type) {
        (SERVICE_CONTROL_STOP, _) => ControlAction::Stop,
        (SERVICE_CONTROL_PRESHUTDOWN, _) => ControlAction::ShutdownThenStop,
        (
            SERVICE_CONTROL_SESSIONCHANGE,
            WTS_SESSION_LOGON | WTS_SESSION_LOGOFF | WTS_SESSION_LOCK | WTS_SESSION_UNLOCK,
        ) => ControlAction::SessionChange,
        (SERVICE_CONTROL_POWEREVENT, PBT_APMSUSPEND) => ControlAction::Suspend,
        // A wake by a user adds PBT_APMRESUMESUSPEND after this event, so only
        // this one gives Resume: one Resume for each wake.
        (SERVICE_CONTROL_POWEREVENT, PBT_APMRESUMEAUTOMATIC) => ControlAction::Resume,
        _ => ControlAction::Ignore,
    }
}

/// Sends `Suspend` and waits for the reply of the core. The send and the
/// wait together take `limit` at most, because Windows goes on into sleep
/// when the handler returns.
pub async fn suspend(events: &mpsc::Sender<SystemEvent>, limit: Duration) {
    let (done, reply) = oneshot::channel();
    let sent_and_replied = timeout(limit, async {
        if events.send(SystemEvent::Suspend { done }).await.is_err() {
            warn!("cannot send Suspend: the core has stopped");
            return;
        }
        // A dropped reply also ends the wait: the core has stopped.
        let _ = reply.await;
    })
    .await;
    if sent_and_replied.is_err() {
        warn!("no reply to Suspend within {limit:?}");
    }
}

/// Sends `Shutdown` and waits for the reply of the core, which comes after
/// the ack of `offline` or the ack wait. The stop follows it.
pub async fn shutdown(events: &mpsc::Sender<SystemEvent>) {
    let (done, reply) = oneshot::channel();
    if events.send(SystemEvent::Shutdown { done }).await.is_err() {
        warn!("cannot send Shutdown: the core has stopped");
        return;
    }
    let _ = reply.await;
}

/// The warning of `pc-agent install` for the `HiberbootEnabled` value.
pub fn fast_startup_warning(hiberboot: Option<u32>) -> Option<&'static str> {
    (hiberboot == Some(1)).then_some(
        "warning: Fast Startup is on (HiberbootEnabled = 1). A shutdown then \
         hibernates the kernel: lastboot is wrong and no SystemShutdown event \
         is sent. Turn it off with: powercfg /h off",
    )
}

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

    /// The wake by a user, after `PBT_APMRESUMEAUTOMATIC`.
    const PBT_APMRESUMESUSPEND: u32 = 0x7;

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
    fn stop_and_preshutdown_stop_the_agent_and_preshutdown_announces_the_shutdown() {
        assert_eq!(control_action(SERVICE_CONTROL_STOP, 0), ControlAction::Stop);
        assert_eq!(
            control_action(SERVICE_CONTROL_PRESHUTDOWN, 0),
            ControlAction::ShutdownThenStop
        );
    }

    #[test]
    fn the_four_session_reasons_give_a_session_change_and_the_others_nothing() {
        for reason in [
            WTS_SESSION_LOGON,
            WTS_SESSION_LOGOFF,
            WTS_SESSION_LOCK,
            WTS_SESSION_UNLOCK,
        ] {
            assert_eq!(
                control_action(SERVICE_CONTROL_SESSIONCHANGE, reason),
                ControlAction::SessionChange,
                "{reason}"
            );
        }
        // Console and remote connect and disconnect, remote control, create, terminate.
        for reason in [0x1, 0x2, 0x3, 0x4, 0x9, 0xA, 0xB] {
            assert_eq!(
                control_action(SERVICE_CONTROL_SESSIONCHANGE, reason),
                ControlAction::Ignore,
                "{reason}"
            );
        }
    }

    #[test]
    fn suspend_and_the_automatic_resume_give_suspend_and_resume_once() {
        assert_eq!(
            control_action(SERVICE_CONTROL_POWEREVENT, PBT_APMSUSPEND),
            ControlAction::Suspend
        );
        assert_eq!(
            control_action(SERVICE_CONTROL_POWEREVENT, PBT_APMRESUMEAUTOMATIC),
            ControlAction::Resume
        );
        // A wake by a user sends PBT_APMRESUMESUSPEND after the automatic
        // resume: it must not give a second Resume.
        assert_eq!(
            control_action(SERVICE_CONTROL_POWEREVENT, PBT_APMRESUMESUSPEND),
            ControlAction::Ignore
        );
        // PBT_POWERSETTINGCHANGE (the display state), PBT_APMPOWERSTATUSCHANGE.
        for event in [0x8013, 0xA] {
            assert_eq!(
                control_action(SERVICE_CONTROL_POWEREVENT, event),
                ControlAction::Ignore,
                "{event}"
            );
        }
    }

    #[test]
    fn the_event_type_of_one_control_does_not_select_an_action_of_another() {
        // 4 is PBT_APMSUSPEND and WTS_REMOTE_DISCONNECT; 7 is WTS_SESSION_LOCK
        // and PBT_APMRESUMESUSPEND.
        assert_eq!(
            control_action(SERVICE_CONTROL_SESSIONCHANGE, PBT_APMSUSPEND),
            ControlAction::Ignore
        );
        assert_eq!(
            control_action(SERVICE_CONTROL_POWEREVENT, WTS_SESSION_LOCK),
            ControlAction::Ignore
        );
        // SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_SHUTDOWN.
        assert_eq!(control_action(0x4, 0), ControlAction::Ignore);
        assert_eq!(control_action(0x5, 0), ControlAction::Ignore);
    }

    const WINDOWS_WAIT: Duration = Duration::from_millis(1500);

    #[tokio::test(start_paused = true)]
    async fn the_suspend_handler_returns_after_1_5_s_without_a_broker_ack() {
        let (tx, mut rx) = mpsc::channel(4);
        // The fake core keeps the reply: no ack comes.
        let core = tokio::spawn(async move {
            let event = rx.recv().await;
            std::future::pending::<()>().await;
            drop(event);
        });
        let start = tokio::time::Instant::now();
        suspend(&tx, WINDOWS_WAIT).await;
        assert_eq!(start.elapsed(), WINDOWS_WAIT);
        core.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn the_suspend_handler_sends_suspend_and_returns_at_the_reply() {
        let (tx, mut rx) = mpsc::channel(4);
        let core = tokio::spawn(async move {
            match rx.recv().await {
                Some(SystemEvent::Suspend { done }) => {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    let _ = done.send(());
                }
                other => panic!("expected Suspend, got {other:?}"),
            }
        });
        let start = tokio::time::Instant::now();
        suspend(&tx, WINDOWS_WAIT).await;
        assert_eq!(start.elapsed(), Duration::from_millis(300));
        core.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn the_suspend_handler_returns_within_the_limit_when_the_channel_is_full() {
        let (tx, _rx) = mpsc::channel(1);
        tx.try_send(SystemEvent::Resume).unwrap();
        let start = tokio::time::Instant::now();
        suspend(&tx, WINDOWS_WAIT).await;
        assert_eq!(start.elapsed(), WINDOWS_WAIT);
    }

    #[tokio::test(start_paused = true)]
    async fn preshutdown_sends_shutdown_and_stops_only_after_the_reply() {
        let (tx, mut rx) = mpsc::channel(4);
        let stop = tokio::spawn(async move { shutdown(&tx).await });
        let done = match rx.recv().await {
            Some(SystemEvent::Shutdown { done }) => done,
            other => panic!("expected Shutdown, got {other:?}"),
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!stop.is_finished(), "the stop must wait for the reply");
        done.send(()).unwrap();
        stop.await.unwrap();
    }

    #[test]
    fn hiberboot_1_gives_the_fast_startup_warning_and_0_or_none_no_warning() {
        let warning = fast_startup_warning(Some(1)).expect("a warning for 1");
        assert!(warning.contains("Fast Startup"), "{warning}");
        assert!(warning.contains("powercfg /h off"), "{warning}");
        assert_eq!(fast_startup_warning(Some(0)), None);
        assert_eq!(fast_startup_warning(None), None);
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
