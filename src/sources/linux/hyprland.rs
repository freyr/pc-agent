//! The Hyprland request socket of the active user.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::rules::{self, Monitor};

/// The parent of the runtime directories of the users.
pub const RUN_USER: &str = "/run/user";

const TIMEOUT: Duration = Duration::from_millis(500);

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
        let list = monitors(&base, 1000).expect("the live socket answers");
        assert_eq!(rules::lock_read(&list), rules::LockRead::Locked);
        assert_eq!(server.join().unwrap(), "j/monitors");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn no_runtime_directory_gives_none() {
        let base = temp_dir("hypr-none");
        assert_eq!(monitors(&base, 1000), None);
    }
}
