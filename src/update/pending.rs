//! The rollback rules of a new build. The install writes
//! `update-pending.json`; each start of the service adds 1 to its start
//! count; the first ConnAck confirms the new build (the pending file and
//! `pc-agent.prev` are deleted). At the third start, or 10 min after the
//! first start, with no ConnAck, `pc-agent.prev` goes back to the binary
//! path, the new build is recorded as failed in `update-state.json`, and the
//! service exits so that the service manager starts the old binary.
//!
//! Only the service path reads and changes the start count (see `main`): a
//! CLI run never calls `on_start` or `Watch`.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// The folder of `update-pending.json`, `update-state.json` and the lock file.
#[cfg(target_os = "linux")]
pub const STATE_DIR: &str = "/var/lib/pc-agent";
#[cfg(target_os = "macos")]
pub const STATE_DIR: &str = "/Library/Application Support/pc-agent";
#[cfg(windows)]
pub const STATE_DIR: &str = r"C:\ProgramData\pc-agent";

const PENDING_FILE: &str = "update-pending.json";
const STATE_FILE: &str = "update-state.json";
const LOCK_FILE: &str = "update.lock";

/// The name of the kept old binary, next to the binary.
const PREV_NAME: &str = if cfg!(windows) {
    "pc-agent.prev.exe"
} else {
    "pc-agent.prev"
};

/// Windows only: the name of the failed exe after a rollback (a running
/// image cannot be replaced, only renamed). The next install deletes it.
const FAILED_NAME: &str = "pc-agent.failed.exe";

/// The start that reaches this count with no ConnAck rolls back.
pub const MAX_STARTS: u32 = 3;

/// The time after the first start in which the first ConnAck must come.
pub const CONFIRM_WITHIN: TimeDelta = TimeDelta::minutes(10);

/// The version and the commit of a build.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BuildId {
    pub version: String,
    pub commit: String,
}

/// `update-pending.json`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Pending {
    pub old: BuildId,
    pub new: BuildId,
    /// The starts of the new build so far.
    #[serde(default)]
    pub starts: u32,
    /// The time of the first start of the new build (UTC, RFC 3339).
    #[serde(default)]
    pub first_start: Option<String>,
}

impl Pending {
    /// The file that the install writes: no start yet.
    pub fn new(old: BuildId, new: BuildId) -> Self {
        Pending {
            old,
            new,
            starts: 0,
            first_start: None,
        }
    }
}

/// `update-state.json`: the builds that failed after an install. U4 adds
/// the times of the last check and of the last automatic install.
#[derive(Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct UpdateState {
    #[serde(default)]
    pub failed: Vec<BuildId>,
}

impl UpdateState {
    /// The commits of the failed builds, for `check::compare`.
    pub fn failed_commits(&self) -> Vec<String> {
        self.failed.iter().map(|b| b.commit.clone()).collect()
    }
}

/// The files of the update: the state folder and the binary.
#[derive(Clone, Debug)]
pub struct Paths {
    pub state_dir: PathBuf,
    /// The binary that runs (the service binary).
    pub exe: PathBuf,
}

impl Paths {
    /// The state folder of this OS and the path of the running binary.
    pub fn system() -> Result<Self> {
        Ok(Paths {
            state_dir: PathBuf::from(STATE_DIR),
            exe: std::env::current_exe().context("cannot find the path of pc-agent")?,
        })
    }

    pub fn pending(&self) -> PathBuf {
        self.state_dir.join(PENDING_FILE)
    }

    pub fn state(&self) -> PathBuf {
        self.state_dir.join(STATE_FILE)
    }

    pub fn lock(&self) -> PathBuf {
        self.state_dir.join(LOCK_FILE)
    }

    /// The folder of the binary.
    pub fn exe_dir(&self) -> &Path {
        self.exe.parent().unwrap_or(Path::new("."))
    }

    pub fn prev(&self) -> PathBuf {
        self.exe_dir().join(PREV_NAME)
    }

    pub fn failed_exe(&self) -> PathBuf {
        self.exe_dir().join(FAILED_NAME)
    }
}

/// The result of the pending-file check at a service start.
#[derive(Debug, PartialEq)]
pub enum Start {
    /// No update is pending.
    Normal,
    /// A new build waits for its first ConnAck until `deadline`.
    Pending {
        starts: u32,
        deadline: DateTime<Utc>,
        new: BuildId,
    },
    /// The new build failed: the old binary is back at the binary path.
    /// The service must exit, so that the service manager starts it.
    RolledBack(BuildId),
    /// The rollback failed; the pending file stays, so the next start tries
    /// again. The service must exit.
    RollbackFailed(String),
    /// The pending file could not be read or written; the service runs.
    Error(String),
}

impl Start {
    /// The service must exit at once, so that the service manager starts
    /// the binary at the binary path (the old one after a rollback).
    pub fn must_exit(&self) -> bool {
        matches!(self, Start::RolledBack(_) | Start::RollbackFailed(_))
    }
}

/// Reads a JSON file; `None` when it does not exist.
fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .with_context(|| format!("{} is not valid", path.display())),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// Writes a JSON file through a temporary file and a rename, so a reader
/// never sees half a file.
fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(value)?;
    fs::write(&tmp, text).with_context(|| format!("cannot write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("cannot write {}", path.display()))
}

/// Deletes a file; a missing file is not an error.
pub(super) fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("cannot delete {}", path.display())),
    }
}

/// `update-pending.json`, or `None`.
pub fn read_pending(paths: &Paths) -> Result<Option<Pending>> {
    read_json(&paths.pending())
}

/// Writes `update-pending.json` (the install).
pub fn write_pending(paths: &Paths, pending: &Pending) -> Result<()> {
    write_json(&paths.pending(), pending)
}

/// `update-state.json`, or the empty state when it does not exist.
pub fn read_state(paths: &Paths) -> Result<UpdateState> {
    Ok(read_json(&paths.state())?.unwrap_or_default())
}

/// The pending-file check, the first step of the service path: adds 1 to
/// the start count, and rolls back at the third start or when 10 min have
/// passed since the first start.
pub fn on_start(paths: &Paths, now: DateTime<Utc>) -> Start {
    let path = paths.pending();
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == ErrorKind::NotFound => return Start::Normal,
        Err(e) => return Start::Error(format!("cannot read {}: {e}", path.display())),
    };
    let mut pending: Pending = match serde_json::from_slice(&bytes) {
        Ok(pending) => pending,
        Err(e) => {
            // A file that does not parse names no build to roll back to.
            let deleted = remove_if_present(&path);
            return Start::Error(match deleted {
                Ok(()) => format!("deleted the bad file {}: {e}", path.display()),
                Err(d) => format!("{} is not valid: {e}; {d:#}", path.display()),
            });
        }
    };
    pending.starts += 1;
    let first = pending
        .first_start
        .as_deref()
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc))
        // Whole seconds, as the file keeps it.
        .unwrap_or_else(|| DateTime::from_timestamp(now.timestamp(), 0).unwrap_or(now));
    pending.first_start = Some(first.format("%Y-%m-%dT%H:%M:%SZ").to_string());
    let deadline = first + CONFIRM_WITHIN;
    if pending.starts >= MAX_STARTS || now >= deadline {
        return match rollback_from(paths, &pending) {
            Ok(()) => Start::RolledBack(pending.new),
            Err(e) => Start::RollbackFailed(format!("{e:#}")),
        };
    }
    if let Err(e) = write_pending(paths, &pending) {
        return Start::Error(format!("cannot count the start of the new build: {e:#}"));
    }
    Start::Pending {
        starts: pending.starts,
        deadline,
        new: pending.new,
    }
}

/// The first ConnAck: deletes `update-pending.json`, then `pc-agent.prev`.
/// Returns the confirmed build, or `None` when no update was pending.
pub fn confirm(paths: &Paths) -> Result<Option<BuildId>> {
    let Some(pending) = read_pending(paths)? else {
        return Ok(None);
    };
    // The pending file first: a stale prev is harmless (the next install
    // deletes it), a pending file with no prev is not.
    remove_if_present(&paths.pending())?;
    remove_if_present(&paths.prev())?;
    Ok(Some(pending.new))
}

/// Moves `pc-agent.prev` back to the binary path, records the new build of
/// `update-pending.json` as failed and deletes the pending file. When the
/// pending file is gone (a confirm came first), does nothing and gives `None`.
pub fn rollback(paths: &Paths) -> Result<Option<BuildId>> {
    let Some(pending) = read_pending(paths)? else {
        return Ok(None);
    };
    rollback_from(paths, &pending)?;
    Ok(Some(pending.new))
}

/// The rollback of `pending`. With no `pc-agent.prev` (a crash after an
/// earlier rollback moved it back) the binary stays as it is.
fn rollback_from(paths: &Paths, pending: &Pending) -> Result<()> {
    let prev = paths.prev();
    if prev.exists() {
        // Windows cannot replace the running image, only rename it.
        if cfg!(windows) {
            fs::rename(&paths.exe, paths.failed_exe()).with_context(|| {
                format!("cannot rename {} to {}", paths.exe.display(), FAILED_NAME)
            })?;
        }
        fs::rename(&prev, &paths.exe).with_context(|| {
            format!(
                "cannot move {} back to {}",
                prev.display(),
                paths.exe.display()
            )
        })?;
    }
    let mut state = read_state(paths)?;
    if !state.failed.iter().any(|b| b.commit == pending.new.commit) {
        state.failed.push(pending.new.clone());
    }
    write_json(&paths.state(), &state)?;
    remove_if_present(&paths.pending())
}

/// The update watch of the run loop: the first ConnAck confirms a pending
/// update; with no ConnAck before the deadline, the run loop ends with a
/// restart and the caller rolls back.
#[derive(Debug, Default)]
pub struct Watch {
    paths: Option<Paths>,
    /// While a new build waits for its first ConnAck.
    deadline: Option<tokio::time::Instant>,
    /// The first ConnAck came.
    connected: bool,
}

impl Watch {
    /// No pending-file check (a console run, the tests).
    pub fn none() -> Self {
        Watch::default()
    }

    /// The watch of a service start after `on_start` gave `start` at `now`.
    pub fn new(paths: Paths, start: &Start, now: DateTime<Utc>) -> Self {
        let deadline = match start {
            Start::Pending { deadline, .. } => {
                let left = (*deadline - now).to_std().unwrap_or_default();
                Some(tokio::time::Instant::now() + left)
            }
            _ => None,
        };
        Watch {
            paths: Some(paths),
            deadline,
            connected: false,
        }
    }

    /// The time at which the run loop ends with a restart, for the rollback.
    pub fn deadline(&self) -> Option<tokio::time::Instant> {
        self.deadline
    }

    /// A ConnAck: the first one confirms a pending update.
    pub fn connected(&mut self) {
        if self.connected {
            return;
        }
        self.connected = true;
        self.deadline = None;
        let Some(paths) = &self.paths else { return };
        match confirm(paths) {
            Ok(Some(build)) => info!(
                version = %build.version,
                commit = %build.commit,
                "the first ConnAck confirms the update"
            ),
            Ok(None) => {}
            Err(e) => warn!("cannot confirm the update: {e:#}"),
        }
    }

    /// The rollback after the deadline (the caller exits after it).
    pub fn rollback(&self) {
        let Some(paths) = &self.paths else { return };
        match rollback(paths) {
            Ok(Some(failed)) => warn!(
                version = %failed.version,
                commit = %failed.commit,
                "no ConnAck within {} min after the update: rolled back",
                CONFIRM_WITHIN.num_minutes()
            ),
            Ok(None) => info!("no update to roll back"),
            Err(e) => warn!("the rollback failed: {e:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: &str = "1111111111111111111111111111111111111111";
    const NEW: &str = "2222222222222222222222222222222222222222";

    /// A temporary state folder and a binary folder with `pc-agent` (the new
    /// build) and `pc-agent.prev` (the old one).
    fn setup(name: &str) -> Paths {
        let root =
            std::env::temp_dir().join(format!("pc-agent-pending-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let state_dir = root.join("state");
        let bin = root.join("bin");
        fs::create_dir_all(&state_dir).unwrap();
        fs::create_dir_all(&bin).unwrap();
        let exe = bin.join(if cfg!(windows) {
            "pc-agent.exe"
        } else {
            "pc-agent"
        });
        let paths = Paths { state_dir, exe };
        fs::write(&paths.exe, "new build").unwrap();
        fs::write(paths.prev(), "old build").unwrap();
        paths
    }

    fn id(version: &str, commit: &str) -> BuildId {
        BuildId {
            version: version.to_string(),
            commit: commit.to_string(),
        }
    }

    fn pending_installed(paths: &Paths) {
        write_pending(paths, &Pending::new(id("0.3.0", OLD), id("0.3.0", NEW))).unwrap();
    }

    fn at(minutes: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-08T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
            + TimeDelta::minutes(minutes)
    }

    fn assert_rolled_back(paths: &Paths, start: Start) {
        assert_eq!(start, Start::RolledBack(id("0.3.0", NEW)));
        assert_eq!(fs::read_to_string(&paths.exe).unwrap(), "old build");
        assert!(!paths.prev().exists());
        assert!(!paths.pending().exists());
        assert_eq!(read_state(paths).unwrap().failed, vec![id("0.3.0", NEW)]);
        assert_eq!(read_state(paths).unwrap().failed_commits(), vec![NEW]);
    }

    #[test]
    fn no_pending_file_is_a_normal_start() {
        let paths = setup("none");
        assert_eq!(on_start(&paths, at(0)), Start::Normal);
        assert!(!paths.pending().exists());
        assert_eq!(fs::read_to_string(&paths.exe).unwrap(), "new build");
        // No state folder at all (a console run as a user): also normal.
        let mut missing = paths.clone();
        missing.state_dir = paths.state_dir.join("no-such-dir");
        assert_eq!(on_start(&missing, at(0)), Start::Normal);
    }

    #[test]
    fn each_start_adds_one_to_the_count_and_the_first_sets_the_deadline() {
        let paths = setup("count");
        pending_installed(&paths);
        assert_eq!(
            on_start(&paths, at(0)),
            Start::Pending {
                starts: 1,
                deadline: at(10),
                new: id("0.3.0", NEW)
            }
        );
        let p = read_pending(&paths).unwrap().unwrap();
        assert_eq!(p.starts, 1);
        assert_eq!(p.first_start.as_deref(), Some("2026-11-08T12:00:00Z"));
        // The deadline stays 10 min after the first start.
        assert_eq!(
            on_start(&paths, at(3)),
            Start::Pending {
                starts: 2,
                deadline: at(10),
                new: id("0.3.0", NEW)
            }
        );
        assert_eq!(read_pending(&paths).unwrap().unwrap().starts, 2);
        assert_eq!(fs::read_to_string(&paths.exe).unwrap(), "new build");
    }

    #[test]
    fn the_third_start_with_no_connack_rolls_back() {
        let paths = setup("third");
        pending_installed(&paths);
        assert!(matches!(on_start(&paths, at(0)), Start::Pending { .. }));
        assert!(matches!(on_start(&paths, at(1)), Start::Pending { .. }));
        let start = on_start(&paths, at(2));
        assert_rolled_back(&paths, start);
        // The old binary starts next: no pending file, a normal start.
        assert_eq!(on_start(&paths, at(3)), Start::Normal);
    }

    #[test]
    fn a_start_10_min_after_the_first_rolls_back() {
        let paths = setup("late");
        pending_installed(&paths);
        assert!(matches!(on_start(&paths, at(0)), Start::Pending { .. }));
        let start = on_start(&paths, at(10));
        assert_rolled_back(&paths, start);
    }

    #[tokio::test(start_paused = true)]
    async fn the_watch_rolls_back_10_min_after_the_first_start() {
        let paths = setup("watch");
        pending_installed(&paths);
        let start = on_start(&paths, at(0));
        // The service starts 4 min after the first start: 6 min are left.
        let watch = Watch::new(paths.clone(), &start, at(4));
        let left = watch.deadline().unwrap() - tokio::time::Instant::now();
        assert_eq!(left, std::time::Duration::from_secs(6 * 60));
        watch.rollback();
        assert_rolled_back(&paths, Start::RolledBack(id("0.3.0", NEW)));
        // A watch past its deadline ends the run loop at once.
        pending_installed(&paths);
        let start = on_start(&paths, at(0));
        let watch = Watch::new(paths.clone(), &start, at(11));
        assert_eq!(watch.deadline(), Some(tokio::time::Instant::now()));
    }

    #[test]
    fn the_first_connack_confirms() {
        let paths = setup("confirm");
        pending_installed(&paths);
        let start = on_start(&paths, at(0));
        let mut watch = Watch::new(paths.clone(), &start, at(0));
        assert!(watch.deadline().is_some());
        watch.connected();
        assert!(
            watch.deadline().is_none(),
            "a confirmed update has no deadline"
        );
        assert!(!paths.pending().exists());
        assert!(!paths.prev().exists());
        assert_eq!(fs::read_to_string(&paths.exe).unwrap(), "new build");
        assert!(read_state(&paths).unwrap().failed.is_empty());
        // A rollback after the confirm does nothing.
        watch.rollback();
        assert_eq!(fs::read_to_string(&paths.exe).unwrap(), "new build");
        assert!(read_state(&paths).unwrap().failed.is_empty());
    }

    #[test]
    fn confirm_gives_the_build_once_and_keeps_prev_with_no_pending_file() {
        let paths = setup("confirm-once");
        // No pending file: a kept prev is not this start's business.
        assert_eq!(confirm(&paths).unwrap(), None);
        assert!(paths.prev().exists());
        pending_installed(&paths);
        assert_eq!(confirm(&paths).unwrap(), Some(id("0.3.0", NEW)));
        assert!(!paths.prev().exists());
        assert_eq!(confirm(&paths).unwrap(), None);
    }

    #[test]
    fn a_rollback_with_no_prev_still_records_the_failed_build() {
        let paths = setup("no-prev");
        pending_installed(&paths);
        fs::remove_file(paths.prev()).unwrap();
        assert_eq!(rollback(&paths).unwrap(), Some(id("0.3.0", NEW)));
        assert!(!paths.pending().exists());
        assert_eq!(read_state(&paths).unwrap().failed_commits(), vec![NEW]);
    }

    #[test]
    fn a_second_failed_build_is_added_and_a_known_one_is_not_repeated() {
        let paths = setup("two-failed");
        pending_installed(&paths);
        rollback(&paths).unwrap();
        fs::write(paths.prev(), "old build").unwrap();
        pending_installed(&paths);
        rollback(&paths).unwrap();
        fs::write(paths.prev(), "old build").unwrap();
        write_pending(&paths, &Pending::new(id("0.3.0", OLD), id("0.3.1", OLD))).unwrap();
        rollback(&paths).unwrap();
        assert_eq!(
            read_state(&paths).unwrap().failed,
            vec![id("0.3.0", NEW), id("0.3.1", OLD)]
        );
    }

    #[test]
    fn a_bad_pending_file_is_deleted_and_the_start_is_normal() {
        let paths = setup("bad");
        fs::write(paths.pending(), "{ not json").unwrap();
        assert!(matches!(on_start(&paths, at(0)), Start::Error(_)));
        assert!(!paths.pending().exists());
        assert_eq!(fs::read_to_string(&paths.exe).unwrap(), "new build");
    }

    #[test]
    fn a_pending_file_that_cannot_be_read_stays() {
        let paths = setup("unreadable");
        // A folder in place of the file: the read fails, the parse never runs.
        fs::create_dir_all(paths.pending().join("x")).unwrap();
        assert!(matches!(on_start(&paths, at(0)), Start::Error(_)));
        assert!(paths.pending().join("x").exists());
        // A valid file with no read access: an I/O error, not a bad file.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let paths = setup("no-access");
            pending_installed(&paths);
            fs::set_permissions(paths.pending(), fs::Permissions::from_mode(0o000)).unwrap();
            // Root reads it anyway: nothing to test then.
            if fs::read(paths.pending()).is_err() {
                assert!(matches!(on_start(&paths, at(0)), Start::Error(_)));
                assert!(
                    paths.pending().exists(),
                    "a file that could not be read was deleted"
                );
            }
        }
    }

    #[test]
    fn the_paths_of_the_update_files() {
        let paths = setup("names");
        assert!(paths.pending().ends_with("update-pending.json"));
        assert!(paths.state().ends_with("update-state.json"));
        assert!(paths.lock().ends_with("update.lock"));
        assert_eq!(paths.prev().parent(), paths.exe.parent());
        #[cfg(windows)]
        assert!(paths.prev().ends_with("pc-agent.prev.exe"));
        #[cfg(not(windows))]
        assert!(paths.prev().ends_with("pc-agent.prev"));
    }
}
