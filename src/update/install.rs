//! The install of a new build and `pc-agent update`: download the asset of
//! this platform next to the binary and check its SHA-256, keep the running
//! binary as `pc-agent.prev`, write `update-pending.json`, move the new file
//! to the binary path, and restart the service. A lock file in the state
//! folder stops two installs at the same time (the CLI and the service).

use std::fs::{self, File, TryLockError};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use super::check::{self, Build, Channel, Outcome};
use super::pending::{self, BuildId, Paths, Pending, remove_if_present};
use super::{Asset, Manifest};

/// The largest binary that the install downloads.
const ASSET_LIMIT: u64 = 64 * 1024 * 1024;

/// The name of the download, next to the binary.
const STAGED_NAME: &str = if cfg!(windows) {
    "pc-agent.download.exe"
} else {
    "pc-agent.download"
};

/// The lock of an install; the OS releases it when the process ends.
#[derive(Debug)]
pub struct Lock {
    _file: File,
}

/// Takes the install lock in the state folder (created when missing).
pub fn lock(paths: &Paths) -> Result<Lock> {
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
        Ok(()) => Ok(Lock { _file: file }),
        Err(TryLockError::WouldBlock) => {
            bail!("another install runs ({} is locked)", path.display())
        }
        Err(TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("cannot lock {}", path.display()))
        }
    }
}

/// Step 1: writes `body` to the temporary file next to the binary and
/// checks its SHA-256 (hex). A different hash deletes the file and stops.
pub fn stage(paths: &Paths, body: impl Read, sha256: &str) -> Result<PathBuf> {
    let staged = paths.exe_dir().join(STAGED_NAME);
    let result = write_hashed(&staged, body).and_then(|hash| {
        if hash.eq_ignore_ascii_case(sha256.trim()) {
            Ok(())
        } else {
            bail!("the SHA-256 of the download is {hash}, the manifest has {sha256}")
        }
    });
    match result {
        Ok(()) => Ok(staged),
        Err(e) => {
            let _ = fs::remove_file(&staged);
            Err(e)
        }
    }
}

/// Copies `body` to a new file at `path` (mode 0755 on Unix) and gives its
/// SHA-256 in hex.
fn write_hashed(path: &Path, mut body: impl Read) -> Result<String> {
    let mut file =
        File::create(path).with_context(|| format!("cannot create {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = body.read(&mut buf).context("cannot read the download")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])
            .with_context(|| format!("cannot write {}", path.display()))?;
    }
    file.sync_all()
        .with_context(|| format!("cannot write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("cannot set the mode of {}", path.display()))?;
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Steps 2 and 3: deletes a `pc-agent.prev` of an earlier update (a failed
/// delete stops the install), keeps the running binary as `pc-agent.prev`,
/// writes `update-pending.json` and moves `staged` to the binary path. An
/// update that is still pending (not confirmed by a ConnAck) stops the
/// install: `pc-agent.prev` is then the only binary that is known to work.
/// On an error, the download is deleted and the binary stays as it was.
pub fn swap(_lock: &Lock, paths: &Paths, staged: &Path, pending: &Pending) -> Result<()> {
    let result = try_swap(paths, staged, pending);
    if result.is_err() {
        let _ = fs::remove_file(staged);
    }
    result
}

fn try_swap(paths: &Paths, staged: &Path, pending: &Pending) -> Result<()> {
    if let Some(earlier) = pending::read_pending(paths)? {
        bail!(
            "the update to {} commit {} is still pending (no ConnAck yet); try again after \
             the service connects or rolls back",
            earlier.new.version,
            earlier.new.commit
        );
    }
    let prev = paths.prev();
    remove_if_present(&prev).context("cannot delete the pc-agent.prev of an earlier update")?;
    if cfg!(windows) {
        // Left by a rollback; it does not stop the install.
        let _ = remove_if_present(&paths.failed_exe());
        // Windows cannot replace a running image, but it can rename it.
        fs::rename(&paths.exe, &prev).with_context(|| {
            format!(
                "cannot rename {} to {}",
                paths.exe.display(),
                prev.display()
            )
        })?;
    } else {
        // A hard link: the binary path is never missing.
        fs::hard_link(&paths.exe, &prev).with_context(|| {
            format!("cannot keep {} as {}", paths.exe.display(), prev.display())
        })?;
    }
    // Before the rename: a crash between the two leaves a pending file that
    // the old binary confirms, never a new binary with no rollback.
    let moved = pending::write_pending(paths, pending).and_then(|()| {
        fs::rename(staged, &paths.exe).with_context(|| {
            format!(
                "cannot move {} to {}",
                staged.display(),
                paths.exe.display()
            )
        })
    });
    if let Err(e) = moved {
        let _ = remove_if_present(&paths.pending());
        if cfg!(windows) {
            let _ = fs::rename(&prev, &paths.exe);
        } else {
            let _ = fs::remove_file(&prev);
        }
        return Err(e);
    }
    Ok(())
}

/// The asset to install for `outcome`. A build that failed once installs
/// only when the user asked for it (`manual`), never automatically.
pub fn to_install(outcome: Outcome, manual: bool) -> Option<Asset> {
    match outcome {
        Outcome::Available(asset) => Some(asset),
        Outcome::FailedBefore(asset) if manual => Some(asset),
        Outcome::FailedBefore(_) | Outcome::UpToDate | Outcome::Older => None,
    }
}

/// Steps 1 to 3 for `asset` of the verified `manifest` of `channel`.
pub fn install(
    lock: &Lock,
    paths: &Paths,
    base: &str,
    channel: Channel,
    manifest: &Manifest,
    asset: &Asset,
    running: Build,
) -> Result<()> {
    let url = check::url(base, channel, &asset.asset);
    let mut response = check::agent()
        .get(&url)
        .call()
        .map_err(anyhow::Error::from)
        .with_context(|| format!("cannot download {url}"))?;
    let body = response
        .body_mut()
        .with_config()
        .limit(ASSET_LIMIT)
        .reader();
    let staged = stage(paths, body, &asset.sha256).with_context(|| format!("download {url}"))?;
    let pending = Pending::new(
        BuildId {
            version: running.version.to_string(),
            commit: running.commit.to_string(),
        },
        BuildId {
            version: manifest.version.clone(),
            commit: manifest.commit.clone(),
        },
    );
    swap(lock, paths, &staged, &pending)
}

/// Restarts the service through the service manager, so that it runs the
/// new binary.
fn restart_service() -> Result<()> {
    #[cfg(target_os = "linux")]
    return run_tool("systemctl", &["restart", "pc-agent"]);
    #[cfg(target_os = "macos")]
    return run_tool("launchctl", &["kickstart", "-k", "system/pc-agent"]);
    #[cfg(windows)]
    return crate::sources::restart_service();
}

#[cfg(unix)]
fn run_tool(program: &str, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("cannot run {program}"))?;
    if !status.success() {
        bail!("{program} {} failed: {status}", args.join(" "));
    }
    Ok(())
}

/// `pc-agent update [--channel stable|edge]`: checks the channel and
/// installs an available build (also one that failed before), then restarts
/// the service through the service manager. Exit 0 when the build is up to
/// date or the service was restarted with the new one, 1 on an error.
pub fn update_command(flag: Option<Channel>, config: &Path) -> ExitCode {
    match try_update(flag, config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn try_update(flag: Option<Channel>, config: &Path) -> Result<()> {
    let running = Build::RUNNING;
    let short = running.commit.get(..7).unwrap_or(running.commit);
    println!(
        "running: {} commit {short} built_at {}",
        running.version, running.built_at
    );
    let paths = Paths::system()?;
    let lock = lock(&paths)?;
    let (channel, from) = check::channel_of(flag, config)?;
    println!("channel: {channel} ({from})");
    let failed = pending::read_state(&paths)?.failed_commits();
    let base = super::base_url();
    let (manifest, outcome) = check::check(
        &base,
        channel,
        super::public_key(),
        running,
        &check::platform(),
        &failed,
    )?;
    println!(
        "manifest: {} commit {} built_at {}",
        manifest.version, manifest.commit, manifest.built_at
    );
    if matches!(outcome, Outcome::FailedBefore(_)) {
        println!("note: this build failed after an earlier install; it installs as asked");
    }
    let Some(asset) = to_install(outcome, true) else {
        println!("up to date");
        return Ok(());
    };
    install(&lock, &paths, &base, channel, &manifest, &asset, running)?;
    println!(
        "installed {} commit {} at {}",
        manifest.version,
        manifest.commit,
        paths.exe.display()
    );
    restart_service().context("the new binary is installed, but the service restart failed")?;
    println!("restarted the service pc-agent");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::update::check::tests::{Reply, serve};

    const OLD: &str = "1111111111111111111111111111111111111111";
    const NEW: &str = "2222222222222222222222222222222222222222";

    fn setup(name: &str) -> Paths {
        let root =
            std::env::temp_dir().join(format!("pc-agent-install-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let state_dir = root.join("state");
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let exe = bin.join(if cfg!(windows) {
            "pc-agent.exe"
        } else {
            "pc-agent"
        });
        fs::write(&exe, "running build").unwrap();
        Paths { state_dir, exe }
    }

    fn sha(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn id(version: &str, commit: &str) -> BuildId {
        BuildId {
            version: version.to_string(),
            commit: commit.to_string(),
        }
    }

    fn pending() -> Pending {
        Pending::new(id("0.3.0", OLD), id("0.3.0", NEW))
    }

    /// The files in the folder of the binary.
    fn bin_files(paths: &Paths) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(paths.exe_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn the_sha256_of_a_known_text() {
        // `printf abc | shasum -a 256`
        assert_eq!(
            sha(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn a_download_with_the_right_hash_is_staged_next_to_the_binary() {
        let paths = setup("stage");
        let staged = stage(&paths, &b"new build"[..], &sha(b"new build")).unwrap();
        assert_eq!(staged.parent(), paths.exe.parent());
        assert_eq!(fs::read_to_string(&staged).unwrap(), "new build");
        // The hash may be upper case in a hand-made manifest.
        let staged = stage(&paths, &b"new build"[..], &sha(b"new build").to_uppercase()).unwrap();
        assert_eq!(fs::read_to_string(&staged).unwrap(), "new build");
    }

    #[test]
    fn a_different_hash_stops_and_leaves_no_file() {
        let paths = setup("hash");
        let e = stage(&paths, &b"changed build"[..], &sha(b"new build")).unwrap_err();
        assert!(format!("{e:#}").contains("SHA-256"), "{e:#}");
        assert_eq!(bin_files(&paths).len(), 1, "{:?}", bin_files(&paths));
        assert_eq!(fs::read_to_string(&paths.exe).unwrap(), "running build");
    }

    #[test]
    fn the_swap_keeps_the_old_binary_as_prev_and_writes_the_pending_file() {
        let paths = setup("swap");
        let lock = lock(&paths).unwrap();
        // A pc-agent.prev that an earlier update left.
        fs::write(paths.prev(), "stale prev").unwrap();
        let staged = stage(&paths, &b"new build"[..], &sha(b"new build")).unwrap();
        swap(&lock, &paths, &staged, &pending()).unwrap();
        assert_eq!(fs::read_to_string(&paths.exe).unwrap(), "new build");
        assert_eq!(fs::read_to_string(paths.prev()).unwrap(), "running build");
        assert!(!staged.exists());
        let p = pending::read_pending(&paths).unwrap().unwrap();
        assert_eq!(p, pending());
        assert_eq!(p.starts, 0);
        assert_eq!(p.first_start, None);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&paths.exe).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }
    }

    #[test]
    fn a_failed_delete_of_a_stale_prev_stops_the_install() {
        let paths = setup("stale");
        let lock = lock(&paths).unwrap();
        // A folder with a file in it: the file delete fails.
        fs::create_dir_all(paths.prev().join("x")).unwrap();
        let staged = stage(&paths, &b"new build"[..], &sha(b"new build")).unwrap();
        let e = swap(&lock, &paths, &staged, &pending()).unwrap_err();
        assert!(format!("{e:#}").contains("prev"), "{e:#}");
        assert_eq!(fs::read_to_string(&paths.exe).unwrap(), "running build");
        assert!(!paths.pending().exists());
        assert!(!staged.exists(), "the download is deleted");
    }

    #[test]
    fn a_pending_update_stops_the_install_and_keeps_its_count() {
        let paths = setup("pending");
        let lock = lock(&paths).unwrap();
        let mut earlier = pending();
        earlier.starts = 1;
        pending::write_pending(&paths, &earlier).unwrap();
        fs::write(paths.prev(), "good build").unwrap();
        let staged = stage(&paths, &b"new build"[..], &sha(b"new build")).unwrap();
        let e = swap(&lock, &paths, &staged, &pending()).unwrap_err();
        assert!(format!("{e:#}").contains("pending"), "{e:#}");
        assert_eq!(pending::read_pending(&paths).unwrap().unwrap(), earlier);
        assert_eq!(fs::read_to_string(paths.prev()).unwrap(), "good build");
        assert_eq!(fs::read_to_string(&paths.exe).unwrap(), "running build");
    }

    #[test]
    fn the_lock_stops_a_second_install() {
        let paths = setup("lock");
        let first = lock(&paths).unwrap();
        let e = lock(&paths).unwrap_err();
        assert!(format!("{e:#}").contains("another install"), "{e:#}");
        drop(first);
        lock(&paths).unwrap();
    }

    fn asset() -> Asset {
        Asset {
            asset: "pc-agent-test".to_string(),
            sha256: sha(b"new build"),
        }
    }

    #[test]
    fn a_failed_build_installs_only_when_the_user_asks() {
        assert_eq!(
            to_install(Outcome::Available(asset()), false),
            Some(asset())
        );
        assert_eq!(to_install(Outcome::Available(asset()), true), Some(asset()));
        assert_eq!(to_install(Outcome::FailedBefore(asset()), false), None);
        assert_eq!(
            to_install(Outcome::FailedBefore(asset()), true),
            Some(asset())
        );
        assert_eq!(to_install(Outcome::UpToDate, true), None);
        assert_eq!(to_install(Outcome::Older, true), None);
    }

    fn manifest(version: &str, commit: &str, built_at: &str) -> Manifest {
        let mut platforms = BTreeMap::new();
        platforms.insert("x86_64-linux".to_string(), asset());
        Manifest {
            version: version.to_string(),
            commit: commit.to_string(),
            built_at: built_at.to_string(),
            platforms,
        }
    }

    #[test]
    fn a_later_edge_build_with_the_same_version_installs_after_a_failed_one() {
        let paths = setup("edge-failed");
        fs::create_dir_all(&paths.state_dir).unwrap();
        // The build NEW failed after an install.
        pending::write_pending(&paths, &Pending::new(id("0.3.0", OLD), id("0.3.0", NEW))).unwrap();
        fs::write(paths.prev(), "old").unwrap();
        pending::rollback(&paths).unwrap();
        let failed = pending::read_state(&paths).unwrap().failed_commits();
        assert_eq!(failed, vec![NEW]);
        let running = Build {
            version: "0.3.0",
            commit: OLD,
            built_at: "2026-11-08T10:00:00Z",
        };
        // The failed build itself: not automatically.
        let m = manifest("0.3.0", NEW, "2026-11-08T11:00:00Z");
        let outcome = check::compare(&m, Channel::Edge, running, "x86_64-linux", &failed).unwrap();
        assert_eq!(to_install(outcome, false), None);
        // A later edge build: the same version, a new commit.
        let later = "3333333333333333333333333333333333333333";
        let m = manifest("0.3.0", later, "2026-11-08T12:00:00Z");
        let outcome = check::compare(&m, Channel::Edge, running, "x86_64-linux", &failed).unwrap();
        assert_eq!(to_install(outcome, false), Some(asset()));
    }

    #[test]
    fn install_downloads_the_asset_of_the_channel_and_swaps() {
        let paths = setup("http");
        let base = serve(vec![
            (
                "/download/edge/pc-agent-test",
                Reply::Ok(b"new build".to_vec()),
            ),
            (
                "/latest/download/pc-agent-test",
                Reply::Ok(b"changed build".to_vec()),
            ),
        ]);
        let lock = lock(&paths).unwrap();
        let running = Build {
            version: "0.3.0",
            commit: OLD,
            built_at: "2026-11-08T10:00:00Z",
        };
        let m = manifest("0.3.1", NEW, "2026-11-08T12:00:00Z");
        // The stable asset is changed: its hash does not match.
        let e = install(&lock, &paths, &base, Channel::Stable, &m, &asset(), running).unwrap_err();
        assert!(format!("{e:#}").contains("SHA-256"), "{e:#}");
        assert_eq!(fs::read_to_string(&paths.exe).unwrap(), "running build");
        install(&lock, &paths, &base, Channel::Edge, &m, &asset(), running).unwrap();
        assert_eq!(fs::read_to_string(&paths.exe).unwrap(), "new build");
        assert_eq!(fs::read_to_string(paths.prev()).unwrap(), "running build");
        assert_eq!(
            pending::read_pending(&paths).unwrap().unwrap(),
            Pending::new(id("0.3.0", OLD), id("0.3.1", NEW))
        );
    }
}
