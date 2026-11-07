//! The Hyprland request socket of the active user.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime};

use super::rules::{self, Monitor};

/// The parent of the runtime directories of the users.
pub const RUN_USER: &str = "/run/user";

const TIMEOUT: Duration = Duration::from_millis(500);
/// The longest wait of one poll for the socket search.
const LIMIT: Duration = Duration::from_secs(1);

/// `true` while a socket search runs on its helper thread.
static BUSY: AtomicBool = AtomicBool::new(false);

/// The request sockets of the Hyprland instances of `uid`, the newest
/// instance directory first.
fn sockets(run_user: &Path, uid: u32) -> Vec<PathBuf> {
    let dir = run_user.join(uid.to_string()).join("hypr");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut list: Vec<(SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            if !meta.is_dir() {
                return None;
            }
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            Some((mtime, e.path().join(".socket.sock")))
        })
        .collect();
    list.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    list.into_iter().map(|(_, path)| path).collect()
}

/// Sends `command` and reads the reply to EOF.
fn request(socket: &Path, command: &str) -> std::io::Result<String> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    stream.write_all(command.as_bytes())?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply)?;
    Ok(reply)
}

/// The `j/monitors` list of the newest Hyprland instance of `uid` that
/// answers, or `None` when no socket answers with a monitor list.
pub fn monitors(run_user: &Path, uid: u32) -> Option<Vec<Monitor>> {
    monitors_limited(run_user.to_path_buf(), uid, &BUSY, LIMIT)
}

/// Runs the socket search on a helper thread and waits for it at most
/// `limit`, because a hung Hyprland can block the connect without a limit.
/// While an earlier helper still runs (`busy`), no new one starts and the
/// socket counts as not answering.
fn monitors_limited(
    run_user: PathBuf,
    uid: u32,
    busy: &'static AtomicBool,
    limit: Duration,
) -> Option<Vec<Monitor>> {
    if busy.swap(true, Ordering::SeqCst) {
        return None;
    }
    let (tx, rx) = mpsc::channel();
    let spawned = thread::Builder::new()
        .name("hyprland-socket".into())
        .spawn(move || {
            let list = search(&run_user, uid);
            busy.store(false, Ordering::SeqCst);
            let _ = tx.send(list);
        });
    if spawned.is_err() {
        busy.store(false, Ordering::SeqCst);
        return None;
    }
    rx.recv_timeout(limit).ok().flatten()
}

fn search(run_user: &Path, uid: u32) -> Option<Vec<Monitor>> {
    sockets(run_user, uid).iter().find_map(|socket| {
        request(socket, "j/monitors")
            .ok()
            .and_then(|reply| rules::parse_monitors(&reply))
    })
}

#[cfg(test)]
mod tests {
    use std::fs::{File, FileTimes};
    use std::os::unix::net::UnixListener;
    use std::thread;

    use std::time::Instant;

    use super::*;

    const REPLY: &str =
        r#"[{"name":"DP-1","disabled":false,"dpmsStatus":true,"solitaryBlockedBy":["LOCK"]}]"#;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pc-agent-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn set_mtime(dir: &Path, secs_ago: u64) {
        let t = SystemTime::now() - Duration::from_secs(secs_ago);
        File::open(dir)
            .unwrap()
            .set_times(FileTimes::new().set_modified(t))
            .unwrap();
    }

    #[test]
    fn the_answering_socket_is_used_when_a_newer_one_is_stale() {
        let base = temp_dir("hypr");
        let live = base.join("1000/hypr/live_sig");
        let stale = base.join("1000/hypr/stale_sig");
        std::fs::create_dir_all(&live).unwrap();
        std::fs::create_dir_all(&stale).unwrap();

        // A socket file that nothing listens on: the connect is refused.
        drop(UnixListener::bind(stale.join(".socket.sock")).unwrap());
        let listener = UnixListener::bind(live.join(".socket.sock")).unwrap();
        set_mtime(&live, 600);
        set_mtime(&stale, 60);

        let server = thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut buf = [0u8; 64];
            let n = conn.read(&mut buf).unwrap();
            conn.write_all(REPLY.as_bytes()).unwrap();
            String::from_utf8_lossy(&buf[..n]).to_string()
        });

        assert_eq!(
            sockets(&base, 1000),
            [stale.join(".socket.sock"), live.join(".socket.sock")]
        );
        // An own busy flag: the other tests run in parallel.
        static BUSY_T: AtomicBool = AtomicBool::new(false);
        let list =
            monitors_limited(base.clone(), 1000, &BUSY_T, LIMIT).expect("the live socket answers");
        assert_eq!(rules::lock_read(&list), rules::LockRead::Locked);
        assert_eq!(server.join().unwrap(), "j/monitors");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A listener at `<base>/1000/hypr/sig/.socket.sock` that accepts one
    /// connection and holds it open without a reply for `hold`.
    fn silent_socket(base: &Path, hold: Duration) {
        let dir = base.join("1000/hypr/sig");
        std::fs::create_dir_all(&dir).unwrap();
        let listener = UnixListener::bind(dir.join(".socket.sock")).unwrap();
        thread::spawn(move || {
            let (_conn, _) = listener.accept().unwrap();
            thread::sleep(hold);
        });
    }

    #[test]
    fn a_socket_without_a_reply_gives_none_within_the_limit() {
        static BUSY_T: AtomicBool = AtomicBool::new(false);
        let base = temp_dir("hypr-silent");
        silent_socket(&base, Duration::from_secs(2));

        let start = Instant::now();
        let list = monitors_limited(base.clone(), 1000, &BUSY_T, Duration::from_millis(200));
        assert_eq!(list, None);
        assert!(
            start.elapsed() < Duration::from_millis(400),
            "{:?}",
            start.elapsed()
        );
        // The helper still waits for its read timeout: the flag is set.
        assert!(BUSY_T.load(Ordering::SeqCst));
        // After the read timeout the helper ends and clears the flag.
        thread::sleep(Duration::from_millis(700));
        assert!(!BUSY_T.load(Ordering::SeqCst));
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_busy_helper_gives_none_at_once() {
        static BUSY_T: AtomicBool = AtomicBool::new(true);
        let base = temp_dir("hypr-busy");
        let dir = base.join("1000/hypr/sig");
        std::fs::create_dir_all(&dir).unwrap();
        let listener = UnixListener::bind(dir.join(".socket.sock")).unwrap();
        thread::spawn(move || {
            if let Ok((mut conn, _)) = listener.accept() {
                let mut buf = [0u8; 64];
                let _ = conn.read(&mut buf);
                let _ = conn.write_all(REPLY.as_bytes());
            }
        });

        let start = Instant::now();
        assert_eq!(monitors_limited(base.clone(), 1000, &BUSY_T, LIMIT), None);
        assert!(start.elapsed() < Duration::from_millis(100));
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn no_runtime_directory_gives_none() {
        let base = temp_dir("hypr-none");
        assert_eq!(monitors(&base, 1000), None);
    }
}
