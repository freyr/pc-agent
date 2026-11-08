//! The logind session list. A task on the tokio runtime reads it every 2 s on
//! the system bus and keeps it in shared state; `poll()` reads that state.

use std::sync::Mutex;
use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};
use zbus::{Connection, OwnedFd, OwnedObjectPath};

use super::rules::Session;
use crate::core::SystemEvent;

const REFRESH: Duration = Duration::from_secs(2);
/// The delay before a new connect after the bus or logind failed.
const RETRY: Duration = Duration::from_secs(30);
/// The longest wait for the reply of the core before the lock is released.
/// The core replies within 2 s; logind waits 5 s at most for a delay lock.
const REPLY_GUARD: Duration = Duration::from_secs(3);

/// The last good session list. `None` until the first read attempt is done;
/// an empty list when the bus or logind failed before the first good read.
static SESSIONS: Mutex<Option<Vec<Session>>> = Mutex::new(None);

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait Manager {
    fn list_sessions(&self) -> zbus::Result<Vec<(String, u32, String, String, OwnedObjectPath)>>;

    fn inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> zbus::Result<OwnedFd>;

    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;

    #[zbus(signal)]
    fn prepare_for_shutdown(&self, start: bool) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.freedesktop.login1.Session",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1/session/auto"
)]
trait LoginSession {
    #[zbus(property)]
    fn class(&self) -> zbus::Result<String>;
    #[zbus(property, name = "Type")]
    fn kind(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn remote(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn active(&self) -> zbus::Result<bool>;
}

/// The last session list, or `None` before the first read attempt.
pub fn sessions() -> Option<Vec<Session>> {
    SESSIONS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn store(list: Vec<Session>) {
    *SESSIONS.lock().unwrap_or_else(|e| e.into_inner()) = Some(list);
}

fn store_failure() {
    failed(&mut SESSIONS.lock().unwrap_or_else(|e| e.into_inner()));
}

/// A failed read or a bus loss: the last good list stays; without one, the
/// list is empty (no session).
fn failed(state: &mut Option<Vec<Session>>) {
    if state.is_none() {
        *state = Some(Vec::new());
    }
}

/// Starts the logind task. Call it once, inside the tokio runtime. The task
/// sends the sleep and shutdown events to `events`.
pub fn start(events: mpsc::Sender<SystemEvent>) {
    tokio::spawn(run(events));
}

async fn run(events: mpsc::Sender<SystemEvent>) {
    // One warning until the next good read.
    let mut warned = false;
    loop {
        match Connection::system().await {
            Ok(conn) => {
                if let Err(e) = serve(&conn, &events, &mut warned).await
                    && !warned
                {
                    warn!(
                        "cannot read the logind sessions, the last list stays until a retry works: {e}"
                    );
                    warned = true;
                }
                store_failure();
            }
            Err(e) => {
                if !warned {
                    warn!(
                        "system bus not reachable, the last session list stays until a retry works: {e}"
                    );
                    warned = true;
                }
                store_failure();
            }
        }
        tokio::time::sleep(RETRY).await;
    }
}

/// On one bus connection: holds the delay lock, handles the sleep and
/// shutdown signals and reads the sessions every 2 s, until a read fails.
async fn serve(
    conn: &Connection,
    events: &mpsc::Sender<SystemEvent>,
    warned: &mut bool,
) -> zbus::Result<()> {
    let manager = ManagerProxy::new(conn).await?;
    let mut sleep_signals = manager.receive_prepare_for_sleep().await?;
    let mut shutdown_signals = manager.receive_prepare_for_shutdown().await?;
    let mut lock = take_lock(&manager).await;
    let mut tick = tokio::time::interval(REFRESH);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let (signal, start) = tokio::select! {
            _ = tick.tick() => {
                store(read(conn).await?);
                *warned = false;
                continue;
            }
            Some(s) = sleep_signals.next() => (Signal::PrepareForSleep, *s.args()?.start()),
            Some(s) = shutdown_signals.next() => (Signal::PrepareForShutdown, *s.args()?.start()),
        };
        let act = action(signal, start);
        info!(?act, "logind signal");
        match act {
            Action::Suspend => {
                notify(events, |done| SystemEvent::Suspend { done }).await;
                lock = None;
            }
            Action::Shutdown => {
                notify(events, |done| SystemEvent::Shutdown { done }).await;
                lock = None;
            }
            Action::Resume | Action::CancelShutdown => {
                if lock.is_none() {
                    lock = take_lock(&manager).await;
                }
                let event = if act == Action::Resume {
                    SystemEvent::Resume
                } else {
                    SystemEvent::ShutdownCancelled
                };
                let _ = events.send(event).await;
            }
        }
    }
}

/// Sends an event with a reply and waits for the reply, 3 s at most.
async fn notify(
    events: &mpsc::Sender<SystemEvent>,
    event: impl FnOnce(oneshot::Sender<()>) -> SystemEvent,
) {
    let (done, reply) = oneshot::channel();
    if events.send(event(done)).await.is_ok()
        && tokio::time::timeout(REPLY_GUARD, reply).await.is_err()
    {
        warn!("no reply from the core within {} s", REPLY_GUARD.as_secs());
    }
}

/// Takes the delay lock `sleep:shutdown`; `None` (with a warning) when
/// logind refuses it.
async fn take_lock(manager: &ManagerProxy<'_>) -> Option<OwnedFd> {
    match inhibit(manager).await {
        Ok(fd) => Some(fd),
        Err(e) => {
            warn!("cannot take the logind delay lock: {e}");
            None
        }
    }
}

async fn inhibit(manager: &ManagerProxy<'_>) -> zbus::Result<OwnedFd> {
    manager
        .inhibit(
            "sleep:shutdown",
            "pc-agent",
            "Publish the sleep or shutdown state to Home Assistant",
            "delay",
        )
        .await
}

/// For `diagnose`: takes the delay lock and releases it at once.
pub async fn check_inhibit() -> Result<(), String> {
    let conn = Connection::system().await.map_err(|e| e.to_string())?;
    let manager = ManagerProxy::new(&conn).await.map_err(|e| e.to_string())?;
    inhibit(&manager).await.map(drop).map_err(|e| e.to_string())
}

/// For `diagnose`: all sessions, read once.
pub async fn read_once() -> Result<Vec<Session>, String> {
    let conn = Connection::system().await.map_err(|e| e.to_string())?;
    read(&conn).await.map_err(|e| e.to_string())
}

/// The logind signals of the delay lock.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Signal {
    PrepareForSleep,
    PrepareForShutdown,
}

/// What the logind task does for one signal.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    /// Send `Suspend`, wait for the reply, release the lock.
    Suspend,
    /// Take a new lock, send `Resume`.
    Resume,
    /// Send `Shutdown`, wait for the reply, release the lock.
    Shutdown,
    /// Take a new lock, send `ShutdownCancelled`.
    CancelShutdown,
}

/// The action for a signal and its boolean argument.
pub fn action(signal: Signal, start: bool) -> Action {
    match (signal, start) {
        (Signal::PrepareForSleep, true) => Action::Suspend,
        (Signal::PrepareForSleep, false) => Action::Resume,
        (Signal::PrepareForShutdown, true) => Action::Shutdown,
        (Signal::PrepareForShutdown, false) => Action::CancelShutdown,
    }
}

/// `true` when the error says that the session object is gone: the session
/// ended between `ListSessions` and the property read.
fn session_gone(e: &zbus::Error) -> bool {
    match e {
        zbus::Error::FDO(fdo) => matches!(**fdo, zbus::fdo::Error::UnknownObject(_)),
        zbus::Error::MethodError(name, _, _) => {
            name.as_str() == "org.freedesktop.DBus.Error.UnknownObject"
        }
        _ => false,
    }
}

/// All sessions of `ListSessions` with their properties. A session that goes
/// away during the read is left out.
async fn read(conn: &Connection) -> zbus::Result<Vec<Session>> {
    let manager = ManagerProxy::new(conn).await?;
    let mut list = Vec::new();
    for (id, uid, user, seat, path) in manager.list_sessions().await? {
        let (class, kind, remote, active) = match properties(conn, path).await {
            Ok(props) => props,
            Err(e) if session_gone(&e) => continue,
            Err(e) => return Err(e),
        };
        list.push(Session {
            id,
            seat,
            uid,
            user,
            class,
            kind,
            remote,
            active,
        });
    }
    Ok(list)
}

async fn properties(
    conn: &Connection,
    path: OwnedObjectPath,
) -> zbus::Result<(String, String, bool, bool)> {
    let proxy = LoginSessionProxy::builder(conn)
        .path(path)
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await?;
    Ok((
        proxy.class().await?,
        proxy.kind().await?,
        proxy.remote().await?,
        proxy.active().await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signals_map_to_their_actions() {
        assert_eq!(action(Signal::PrepareForSleep, true), Action::Suspend);
        assert_eq!(action(Signal::PrepareForSleep, false), Action::Resume);
        assert_eq!(action(Signal::PrepareForShutdown, true), Action::Shutdown);
        assert_eq!(
            action(Signal::PrepareForShutdown, false),
            Action::CancelShutdown
        );
    }

    fn alice() -> Session {
        Session {
            id: "2".into(),
            seat: "seat0".into(),
            uid: 1000,
            user: "alice".into(),
            class: "user".into(),
            kind: "wayland".into(),
            remote: false,
            active: true,
        }
    }

    #[test]
    fn only_an_unknown_object_error_is_a_gone_session() {
        let gone = zbus::Error::FDO(Box::new(zbus::fdo::Error::UnknownObject(
            "Unknown object '/org/freedesktop/login1/session/_32'.".into(),
        )));
        assert!(session_gone(&gone));

        let failed = zbus::Error::FDO(Box::new(zbus::fdo::Error::Failed("busy".into())));
        assert!(!session_gone(&failed));
        let timeout = zbus::Error::FDO(Box::new(zbus::fdo::Error::NoReply("timeout".into())));
        assert!(!session_gone(&timeout));
        let io = zbus::Error::InputOutput(std::sync::Arc::new(std::io::Error::other("reset")));
        assert!(!session_gone(&io));
    }

    #[test]
    fn a_failure_after_a_good_read_keeps_the_list() {
        let mut state = Some(vec![alice()]);
        failed(&mut state);
        assert_eq!(state, Some(vec![alice()]));
    }

    #[test]
    fn a_failure_without_a_good_read_gives_no_session() {
        let mut state = None;
        failed(&mut state);
        assert_eq!(state, Some(Vec::new()));
    }
}
