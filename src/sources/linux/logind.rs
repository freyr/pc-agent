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

/// The last session list. `None` until the first read attempt is done; an
/// empty list while the system bus or logind is not reachable.
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
                                "cannot read the logind sessions, no session until a retry works: {e}"
                            );
                            warned = true;
                        }
                        store(Vec::new());
                        break;
                    }
                }
                tokio::time::sleep(REFRESH).await;
            },
            Err(e) => {
                if !warned {
                    warn!("system bus not reachable, no session until a retry works: {e}");
                    warned = true;
                }
                store(Vec::new());
            }
        }
        tokio::time::sleep(RETRY).await;
    }
}

/// All sessions of `ListSessions` with their properties. A session that goes
/// away during the read is left out.
async fn read(conn: &Connection) -> zbus::Result<Vec<Session>> {
    let manager = ManagerProxy::new(conn).await?;
    let mut list = Vec::new();
    for (_id, uid, user, _seat, path) in manager.list_sessions().await? {
        let Ok(session) = properties(conn, path).await else {
            continue;
        };
        let (class, kind, remote, active) = session;
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
