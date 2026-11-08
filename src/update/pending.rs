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

use std::fs::{self, File, TryLockError};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
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

/// The wait before a confirm tries the install lock again.
pub const CONFIRM_RETRY: std::time::Duration = std::time::Duration::from_secs(5);

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
    /// The pending file is left from an install that stopped before the
    /// swap (the running build is its old build, the start count is 0): it
    /// was deleted, and the service runs.
    Stale(BuildId),
    /// The old build runs after a rollback that stopped before it recorded
    /// the failed build (the new build had started): it is recorded now,
    /// the pending file is deleted, and the service runs.
    RollbackFinished(BuildId),
    /// The pending file could not be read or written; the service runs.
    Error(String),
}

/// The result of a confirm.
#[derive(Debug, PartialEq)]
pub enum Confirm {
    /// No update was pending.
    NotPending,
    /// The update of this build is confirmed.
    Confirmed(BuildId),
    /// An install holds the lock: try again later.
    Busy,
}

/// The lock of an install and of a confirm; the OS releases it when the
/// process ends.
#[derive(Debug)]
pub struct Lock {
    _file: File,
}

/// Takes the install lock in the state folder (created when missing).
/// `None` when another process holds it.
pub fn try_lock(paths: &Paths) -> Result<Option<Lock>> {
    fs::create_dir_all(&paths.state_dir).with_context(|| {
        format!(
            "cannot create {} (run as root or admin)",
            paths.state_dir.display()
        )
    })?;
    let path = paths.lock();
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("cannot open {} (run as root or admin)", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(Lock { _file: file })),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("cannot lock {}", path.display()))
        }
    }
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

/// Writes a JSON file through a temporary file in the same folder, synced
/// to the disk before the rename (and the folder after it on Unix), so a
/// reader never sees half a file and a power loss keeps the file.
fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(value)?;
    let mut file = File::create(&tmp).with_context(|| format!("cannot write {}", tmp.display()))?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .with_context(|| format!("cannot write {}", tmp.display()))?;
    drop(file);
    fs::rename(&tmp, path).with_context(|| format!("cannot write {}", path.display()))?;
    sync_dir(path.parent().unwrap_or(Path::new(".")))
}

/// Unix: syncs a folder, so a rename in it is on the disk. Windows has no
/// folder sync (NTFS logs the rename).
pub(super) fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(dir)
        .and_then(|d| d.sync_all())
        .with_context(|| format!("cannot sync {}", dir.display()))?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
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
pub fn on_start(paths: &Paths, running: &BuildId, now: DateTime<Utc>) -> Start {
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
    // The old build runs with a pending file. The count, read before any
    // increment, tells why. 0: an install that stopped after the pending
    // write and before the swap (the new build never ran): stale. 1 or
    // more: a rollback that moved prev back and stopped before it recorded
    // the failed build: finish it.
    if running == &pending.old && running != &pending.new {
        if pending.starts == 0 {
            return match remove_if_present(&path) {
                Ok(()) => Start::Stale(pending.new),
                Err(e) => Start::Error(format!("{e:#}")),
            };
        }
        return match record_failed(paths, &pending) {
            Ok(()) => Start::RollbackFinished(pending.new),
            Err(e) => Start::Error(format!("cannot finish the rollback: {e:#}")),
        };
    }
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

/// The first ConnAck: deletes `update-pending.json`, then `pc-agent.prev`,
/// under the install lock; `Busy` when an install holds it.
pub fn confirm(paths: &Paths) -> Result<Confirm> {
    if read_pending(paths)?.is_none() {
        return Ok(Confirm::NotPending);
    }
    // An install between the two deletes would lose its new prev.
    let Some(_lock) = try_lock(paths)? else {
        return Ok(Confirm::Busy);
    };
    let Some(pending) = read_pending(paths)? else {
        return Ok(Confirm::NotPending);
    };
    // The pending file first: a stale prev is harmless (the next install
    // deletes it), a pending file with no prev is not.
    remove_if_present(&paths.pending())?;
    remove_if_present(&paths.prev())?;
    Ok(Confirm::Confirmed(pending.new))
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
    if paths.prev().exists() {
        // Windows cannot replace the running image, only rename it.
        restore_prev(paths, cfg!(windows), |from, to| fs::rename(from, to))?;
    }
    record_failed(paths, pending)
}

/// Records the new build of `pending` as failed in `update-state.json` (by
/// commit, once), then deletes the pending file.
fn record_failed(paths: &Paths, pending: &Pending) -> Result<()> {
    let mut state = read_state(paths)?;
    if !state.failed.iter().any(|b| b.commit == pending.new.commit) {
        state.failed.push(pending.new.clone());
    }
    write_json(&paths.state(), &state)?;
    remove_if_present(&paths.pending())
}

/// Moves `pc-agent.prev` to the binary path. With `rename_running` (Windows)
/// the running binary moves to `pc-agent.failed.exe` first. `rename` is
/// `fs::rename` (a test passes one that fails).
fn restore_prev(
    paths: &Paths,
    rename_running: bool,
    mut rename: impl FnMut(&Path, &Path) -> io::Result<()>,
) -> Result<()> {
    let prev = paths.prev();
    if rename_running {
        rename(&paths.exe, &paths.failed_exe())
            .with_context(|| format!("cannot rename {} to {}", paths.exe.display(), FAILED_NAME))?;
    }
    let moved = rename(&prev, &paths.exe).with_context(|| {
        format!(
            "cannot move {} back to {}",
            prev.display(),
            paths.exe.display()
        )
    });
    if let Err(e) = moved {
        if rename_running {
            // Never leave the binary path empty: put the running binary back.
            if let Err(undo) = rename(&paths.failed_exe(), &paths.exe) {
                return Err(anyhow!(
                    "{e:#}; cannot rename {} back to {}: {undo}",
                    FAILED_NAME,
                    paths.exe.display()
                ));
            }
        }
        return Err(e);
    }
    Ok(())
}

/// The update watch of the run loop: the first ConnAck confirms a pending
/// update; with no ConnAck before the deadline, the run loop ends with a
/// restart and the caller rolls back.
#[derive(Debug, Default)]
pub struct Watch {
    paths: Option<Paths>,
    /// While a new build waits for its first ConnAck.
    deadline: Option<tokio::time::Instant>,
    /// The next try of a confirm that found the install lock held.
    retry: Option<tokio::time::Instant>,
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
            retry: None,
            connected: false,
        }
    }

    /// The time at which the run loop calls `on_deadline`: the rollback
    /// deadline before the first ConnAck, or the next try of a confirm.
    pub fn deadline(&self) -> Option<tokio::time::Instant> {
        self.retry.or(self.deadline)
    }

    /// A ConnAck: the first one confirms a pending update.
    pub fn connected(&mut self) {
        if self.connected {
            return;
        }
        self.connected = true;
        self.deadline = None;
        self.try_confirm();
    }

    fn try_confirm(&mut self) {
        self.retry = None;
        let Some(paths) = &self.paths else { return };
        match confirm(paths) {
            Ok(Confirm::Confirmed(build)) => info!(
                version = %build.version,
                commit = %build.commit,
                "the first ConnAck confirms the update"
            ),
            Ok(Confirm::NotPending) => {}
            Ok(Confirm::Busy) => {
                info!(
                    "an install holds the lock: the confirm tries again in {} s",
                    CONFIRM_RETRY.as_secs()
                );
                self.retry = Some(tokio::time::Instant::now() + CONFIRM_RETRY);
            }
            Err(e) => warn!("cannot confirm the update: {e:#}"),
        }
    }

    /// The time of `deadline()` passed. Tries a busy confirm again, or
    /// rolls back; gives `true` when the run loop must end with a restart.
    pub fn on_deadline(&mut self) -> bool {
        if self.retry.is_some() {
            self.try_confirm();
            return false;
        }
        self.rollback();
        true
    }

    /// The rollback after the deadline (the caller exits after it).
    fn rollback(&self) {
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
    const THIRD: &str = "3333333333333333333333333333333333333333";

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
        assert_eq!(on_start(&paths, &id("0.3.0", NEW), at(0)), Start::Normal);
        assert!(!paths.pending().exists());
        assert_eq!(fs::read_to_string(&paths.exe).unwrap(), "new build");
        // No state folder at all (a console run as a user): also normal.
        let mut missing = paths.clone();
        missing.state_dir = paths.state_dir.join("no-such-dir");
        assert_eq!(on_start(&missing, &id("0.3.0", NEW), at(0)), Start::Normal);
    }

    #[test]
    fn each_start_adds_one_to_the_count_and_the_first_sets_the_deadline() {
        let paths = setup("count");
        pending_installed(&paths);
        assert_eq!(
            on_start(&paths, &id("0.3.0", NEW), at(0)),
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
            on_start(&paths, &id("0.3.0", NEW), at(3)),
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
        assert!(matches!(
            on_start(&paths, &id("0.3.0", NEW), at(0)),
            Start::Pending { .. }
        ));
        assert!(matches!(
            on_start(&paths, &id("0.3.0", NEW), at(1)),
            Start::Pending { .. }
        ));
        let start = on_start(&paths, &id("0.3.0", NEW), at(2));
        assert_rolled_back(&paths, start);
        // The old binary starts next: no pending file, a normal start.
        assert_eq!(on_start(&paths, &id("0.3.0", NEW), at(3)), Start::Normal);
    }

    #[test]
    fn a_start_10_min_after_the_first_rolls_back() {
        let paths = setup("late");
        pending_installed(&paths);
        assert!(matches!(
            on_start(&paths, &id("0.3.0", NEW), at(0)),
            Start::Pending { .. }
        ));
        let start = on_start(&paths, &id("0.3.0", NEW), at(10));
        assert_rolled_back(&paths, start);
    }

    #[tokio::test(start_paused = true)]
    async fn the_watch_rolls_back_10_min_after_the_first_start() {
        let paths = setup("watch");
        pending_installed(&paths);
        let start = on_start(&paths, &id("0.3.0", NEW), at(0));
        // The service starts 4 min after the first start: 6 min are left.
        let mut watch = Watch::new(paths.clone(), &start, at(4));
        let left = watch.deadline().unwrap() - tokio::time::Instant::now();
        assert_eq!(left, std::time::Duration::from_secs(6 * 60));
        assert!(watch.on_deadline());
        assert_rolled_back(&paths, Start::RolledBack(id("0.3.0", NEW)));
        // A watch past its deadline ends the run loop at once.
        pending_installed(&paths);
        let start = on_start(&paths, &id("0.3.0", NEW), at(0));
        let watch = Watch::new(paths.clone(), &start, at(11));
        assert_eq!(watch.deadline(), Some(tokio::time::Instant::now()));
    }

    #[test]
    fn the_first_connack_confirms() {
        let paths = setup("confirm");
        pending_installed(&paths);
        let start = on_start(&paths, &id("0.3.0", NEW), at(0));
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
        assert!(watch.deadline().is_none());
        assert_eq!(fs::read_to_string(&paths.exe).unwrap(), "new build");
        assert!(read_state(&paths).unwrap().failed.is_empty());
    }

    #[test]
    fn confirm_gives_the_build_once_and_keeps_prev_with_no_pending_file() {
        let paths = setup("confirm-once");
        // No pending file: a kept prev is not this start's business.
        assert_eq!(confirm(&paths).unwrap(), Confirm::NotPending);
        assert!(paths.prev().exists());
        pending_installed(&paths);
        assert_eq!(
            confirm(&paths).unwrap(),
            Confirm::Confirmed(id("0.3.0", NEW))
        );
        assert!(!paths.prev().exists());
        assert_eq!(confirm(&paths).unwrap(), Confirm::NotPending);
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
        assert!(matches!(
            on_start(&paths, &id("0.3.0", NEW), at(0)),
            Start::Error(_)
        ));
        assert!(!paths.pending().exists());
        assert_eq!(fs::read_to_string(&paths.exe).unwrap(), "new build");
    }

    #[test]
    fn a_pending_file_that_cannot_be_read_stays() {
        let paths = setup("unreadable");
        // A folder in place of the file: the read fails, the parse never runs.
        fs::create_dir_all(paths.pending().join("x")).unwrap();
        assert!(matches!(
            on_start(&paths, &id("0.3.0", NEW), at(0)),
            Start::Error(_)
        ));
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
                assert!(matches!(
                    on_start(&paths, &id("0.3.0", NEW), at(0)),
                    Start::Error(_)
                ));
                assert!(
                    paths.pending().exists(),
                    "a file that could not be read was deleted"
                );
            }
        }
    }

    #[test]
    fn a_pending_file_of_an_install_that_stopped_before_the_swap_is_stale() {
        let paths = setup("stale");
        // Written by the install, never counted: the new build never ran.
        pending_installed(&paths);
        // The running build is the old build of the pending file.
        let start = on_start(&paths, &id("0.3.0", OLD), at(0));
        assert_eq!(start, Start::Stale(id("0.3.0", NEW)));
        assert!(!start.must_exit());
        assert!(!paths.pending().exists());
        assert_eq!(fs::read_to_string(&paths.exe).unwrap(), "new build");
        assert!(read_state(&paths).unwrap().failed.is_empty());
    }

    #[test]
    fn a_rollback_that_stopped_before_its_record_is_finished_by_the_old_build() {
        let paths = setup("half-rollback");
        // The new build ran twice; a rollback moved prev back and stopped
        // before it recorded the failed build.
        let mut p = Pending::new(id("0.3.0", OLD), id("0.3.1", NEW));
        p.starts = 2;
        p.first_start = Some("2026-11-08T12:00:00Z".to_string());
        write_pending(&paths, &p).unwrap();
        fs::remove_file(paths.prev()).unwrap();
        fs::write(&paths.exe, "old build").unwrap();
        let start = on_start(&paths, &id("0.3.0", OLD), at(1));
        assert_eq!(start, Start::RollbackFinished(id("0.3.1", NEW)));
        assert!(!start.must_exit(), "the old build already runs");
        assert!(!paths.pending().exists());
        assert_eq!(fs::read_to_string(&paths.exe).unwrap(), "old build");
        assert_eq!(read_state(&paths).unwrap().failed, vec![id("0.3.1", NEW)]);
        // Also after one start of the new build.
        let mut p = Pending::new(id("0.3.0", OLD), id("0.3.2", THIRD));
        p.starts = 1;
        write_pending(&paths, &p).unwrap();
        let start = on_start(&paths, &id("0.3.0", OLD), at(1));
        assert_eq!(start, Start::RollbackFinished(id("0.3.2", THIRD)));
        assert_eq!(
            read_state(&paths).unwrap().failed,
            vec![id("0.3.1", NEW), id("0.3.2", THIRD)]
        );
    }

    #[test]
    fn a_failed_move_of_prev_puts_the_running_binary_back() {
        let paths = setup("undo");
        let mut calls = 0;
        let e = restore_prev(&paths, true, |from, to| {
            calls += 1;
            if calls == 2 {
                Err(io::Error::other("held by another process"))
            } else {
                fs::rename(from, to)
            }
        })
        .unwrap_err();
        assert!(
            format!("{e:#}").contains("held by another process"),
            "{e:#}"
        );
        assert_eq!(fs::read_to_string(&paths.exe).unwrap(), "new build");
        assert!(!paths.failed_exe().exists());
        assert_eq!(fs::read_to_string(paths.prev()).unwrap(), "old build");
        // The Windows rollback with no fault: the failed build is kept aside.
        restore_prev(&paths, true, |from, to| fs::rename(from, to)).unwrap();
        assert_eq!(fs::read_to_string(&paths.exe).unwrap(), "old build");
        assert_eq!(fs::read_to_string(paths.failed_exe()).unwrap(), "new build");
    }

    #[test]
    fn a_confirm_waits_while_an_install_holds_the_lock() {
        let paths = setup("busy");
        pending_installed(&paths);
        let lock = try_lock(&paths).unwrap().unwrap();
        assert_eq!(confirm(&paths).unwrap(), Confirm::Busy);
        assert!(paths.pending().exists());
        assert!(paths.prev().exists());
        drop(lock);
        assert_eq!(
            confirm(&paths).unwrap(),
            Confirm::Confirmed(id("0.3.0", NEW))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_watch_tries_a_busy_confirm_again() {
        let paths = setup("retry");
        pending_installed(&paths);
        let start = on_start(&paths, &id("0.3.0", NEW), at(0));
        let mut watch = Watch::new(paths.clone(), &start, at(0));
        let lock = try_lock(&paths).unwrap().unwrap();
        watch.connected();
        // No rollback deadline after a ConnAck, only the next try.
        let next = watch.deadline().unwrap();
        assert_eq!(next - tokio::time::Instant::now(), CONFIRM_RETRY);
        assert!(paths.pending().exists());
        drop(lock);
        tokio::time::advance(CONFIRM_RETRY).await;
        assert!(!watch.on_deadline(), "a confirm try is not a restart");
        assert!(!paths.pending().exists());
        assert!(!paths.prev().exists());
        assert_eq!(watch.deadline(), None);
        assert_eq!(fs::read_to_string(&paths.exe).unwrap(), "new build");
    }

    #[test]
    fn a_json_write_leaves_no_temporary_file() {
        let paths = setup("tmp");
        pending_installed(&paths);
        let names: Vec<String> = fs::read_dir(&paths.state_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["update-pending.json"]);
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
