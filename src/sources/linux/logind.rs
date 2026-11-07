//! The logind session list. A task on the tokio runtime reads it every 2 s on
//! the system bus and keeps it in shared state; `poll()` reads that state.

use std::sync::Mutex;
use std::time::Duration;

use tracing::warn;
use zbus::{Connection, OwnedObjectPath};

use super::rules::Session;

const REFRESH: Duration = Duration::from_secs(2);
/// The delay before a new connect after the bus or logind failed.
const RETRY: Duration = Duration::from_secs(30);

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

/// Starts the logind task. Call it once, inside the tokio runtime.
pub fn start() {
    tokio::spawn(run());
}

async fn run() {
    // One warning until the next good read.
    let mut warned = false;
    loop {
        match Connection::system().await {
            Ok(conn) => loop {
                match read(&conn).await {
                    Ok(list) => {
                        warned = false;
                        store(list);
                    }
                    Err(e) => {
                        if !warned {
                            warn!(
                                "cannot read the logind sessions, the last list stays until a retry works: {e}"
                            );
                            warned = true;
                        }
                        store_failure();
                        break;
                    }
                }
                tokio::time::sleep(REFRESH).await;
            },
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
    for (_id, uid, user, _seat, path) in manager.list_sessions().await? {
        let (class, kind, remote, active) = match properties(conn, path).await {
            Ok(props) => props,
            Err(e) if session_gone(&e) => continue,
            Err(e) => return Err(e),
        };
        list.push(Session {
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

    fn alice() -> Session {
        Session {
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
