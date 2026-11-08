//! The signed release manifest of the self-update. No OS code: the check
//! (`check`) and the install (U3 and later) build on `verify`.

mod check;
mod install;
mod key;
pub(crate) mod pending;

pub use check::{Channel, check_command};
pub use install::update_command;
pub use pending::{Paths, Start, Watch};

/// The test feature `update-test-exit`: the service path exits with a
/// failure right after the pending-file check (a broken new build).
pub const TEST_EXIT: bool = cfg!(feature = "update-test-exit");

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result, anyhow};
use minisign_verify::{PublicKey, Signature};
use serde::Deserialize;

/// The version of this build: the Cargo version (a test build of the
/// feature `update-test-key` can set another one; see `build.rs`).
pub const VERSION: &str = env!("PC_AGENT_VERSION");
/// The commit of this build (full hash), or `unknown`; see `build.rs`.
pub const COMMIT: &str = env!("PC_AGENT_COMMIT");
/// The build time of this binary (UTC, RFC 3339); see `build.rs`.
pub const BUILT_AT: &str = env!("PC_AGENT_BUILD_TIME");

/// The pending-file check of a service start (the first step of the
/// service path, before the configuration, the log and the OS init) and the
/// watch of the run loop. The caller logs the result with `log_start` after
/// the log init, and exits when `Start::must_exit` is true.
pub fn service_start() -> (Start, Watch) {
    let now = chrono::Utc::now();
    match Paths::system() {
        Ok(paths) => {
            let start = pending::on_start(&paths, now);
            let watch = Watch::new(paths, &start, now);
            (start, watch)
        }
        Err(e) => (Start::Error(format!("{e:#}")), Watch::none()),
    }
}

/// Logs the result of the pending-file check.
pub fn log_start(start: &Start) {
    match start {
        Start::Normal => {}
        Start::Pending {
            starts,
            deadline,
            new,
        } => tracing::info!(
            version = %new.version,
            commit = %new.commit,
            start = starts,
            "new build: the first ConnAck before {deadline} confirms it"
        ),
        Start::RolledBack(failed) => tracing::warn!(
            version = %failed.version,
            commit = %failed.commit,
            "the new build got no ConnAck: rolled back to the old binary"
        ),
        Start::RollbackFailed(e) => tracing::error!("the rollback failed: {e}"),
        Start::Error(e) => tracing::warn!("update check at start: {e}"),
    }
}

/// The base URL of the releases. In a build of the test feature
/// `update-test-key` only, `PC_AGENT_UPDATE_URL` replaces it (the CI test).
pub fn base_url() -> String {
    #[cfg(feature = "update-test-key")]
    if let Ok(url) = std::env::var("PC_AGENT_UPDATE_URL") {
        return url;
    }
    check::BASE_URL.to_string()
}

/// The key that signs the manifest. A build of the test feature
/// `update-test-key` has the test key of `build.rs` instead of the project key.
pub fn public_key() -> &'static str {
    #[cfg(feature = "update-test-key")]
    return env!("PC_AGENT_TEST_PUBLIC_KEY");
    #[cfg(not(feature = "update-test-key"))]
    return key::PUBLIC_KEY;
}

/// `manifest.json` of a release; `scripts/manifest.sh` writes it.
#[derive(Debug, Deserialize, PartialEq)]
pub struct Manifest {
    pub version: String,
    pub commit: String,
    pub built_at: String,
    /// Keyed by platform: `x86_64-linux`, `aarch64-macos`, `x86_64-windows`.
    pub platforms: BTreeMap<String, Asset>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Asset {
    pub asset: String,
    pub sha256: String,
}

/// Checks the minisign signature of `manifest` with `public_key` (base64)
/// and only then parses it.
pub fn verify(manifest: &[u8], signature: &str, public_key: &str) -> Result<Manifest> {
    let key =
        PublicKey::from_base64(public_key.trim()).map_err(|e| anyhow!("bad public key: {e}"))?;
    let signature = Signature::decode(signature).map_err(|e| anyhow!("bad signature: {e}"))?;
    key.verify(manifest, &signature, false)
        .map_err(|e| anyhow!("the manifest signature does not verify: {e}"))?;
    serde_json::from_slice(manifest).context("the signed manifest is not valid")
}

/// The base64 key of a minisign `.pub` file: the last line that is not empty
/// and not the untrusted comment.
fn key_of_pub_file(text: &str) -> Option<&str> {
    text.lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty() && !line.starts_with("untrusted comment:"))
}

/// `pc-agent update --verify <manifest> <signature> <public key file>`:
/// prints the manifest when its signature verifies, and the commit and build
/// time of this binary, for the CI check that both have the same rule.
pub fn verify_files(manifest: &Path, signature: &Path, public_key: &Path) -> ExitCode {
    let result = (|| -> Result<Manifest> {
        let bytes =
            fs::read(manifest).with_context(|| format!("cannot read {}", manifest.display()))?;
        let signature = fs::read_to_string(signature)
            .with_context(|| format!("cannot read {}", signature.display()))?;
        let pub_file = fs::read_to_string(public_key)
            .with_context(|| format!("cannot read {}", public_key.display()))?;
        let key = key_of_pub_file(&pub_file)
            .with_context(|| format!("no key in {}", public_key.display()))?;
        verify(&bytes, &signature, key)
    })();
    match result {
        Ok(m) => {
            println!("signature: OK");
            println!("manifest version: {}", m.version);
            println!("manifest commit: {}", m.commit);
            println!("manifest built_at: {}", m.built_at);
            for (platform, asset) in &m.platforms {
                println!(
                    "manifest {platform}: {} sha256 {}",
                    asset.asset, asset.sha256
                );
            }
            println!("running version: {VERSION}");
            println!("running commit: {COMMIT}");
            println!("running built_at: {BUILT_AT}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use key::PUBLIC_KEY;

    pub(super) const MANIFEST: &[u8] = include_bytes!("testdata/manifest.json");
    pub(super) const SIGNATURE: &str = include_str!("testdata/manifest.json.minisig");
    /// The public key of the throwaway test key that signed the fixture
    /// (`testdata/test.pub`).
    pub(super) const TEST_KEY: &str = "RWQ+2MlekOLCM8wYNdyQldq86IT7dyJ01dgZjWDbq+j8VH7zuIua3IiJ";

    #[test]
    fn a_signed_manifest_parses() {
        let m = verify(MANIFEST, SIGNATURE, TEST_KEY).unwrap();
        assert_eq!(m.version, "0.2.0");
        assert_eq!(m.commit, "4cf806aab3b9e17c29f7ea12c20f512b2557e5b6");
        assert_eq!(m.built_at, "2026-11-08T16:52:57Z");
        let names: Vec<_> = m.platforms.keys().map(String::as_str).collect();
        assert_eq!(names, ["aarch64-macos", "x86_64-linux", "x86_64-windows"]);
        let windows = &m.platforms["x86_64-windows"];
        assert_eq!(windows.asset, "pc-agent-x86_64-windows.exe");
        assert_eq!(
            windows.sha256,
            "5d1a3327d257b41322d76e3718177c35f503ee84b4a89ff5746b38d00e9f0649"
        );
    }

    #[test]
    fn the_key_of_a_pub_file_is_its_key_line() {
        let text = include_str!("testdata/test.pub");
        assert_eq!(key_of_pub_file(text), Some(TEST_KEY));
        assert_eq!(key_of_pub_file(&text.replace('\n', "\r\n")), Some(TEST_KEY));
        assert_eq!(key_of_pub_file("untrusted comment: x\n\n"), None);
    }

    #[test]
    fn a_changed_manifest_fails() {
        let text = std::str::from_utf8(MANIFEST).unwrap();
        let changed = text.replace("\"0.2.0\"", "\"9.2.0\"");
        assert_ne!(changed, text);
        assert!(verify(changed.as_bytes(), SIGNATURE, TEST_KEY).is_err());
    }

    #[test]
    fn a_different_key_fails() {
        assert_ne!(PUBLIC_KEY, TEST_KEY);
        assert!(verify(MANIFEST, SIGNATURE, PUBLIC_KEY).is_err());
    }

    #[test]
    fn a_bad_signature_or_key_fails() {
        assert!(verify(MANIFEST, "not a signature", TEST_KEY).is_err());
        assert!(verify(MANIFEST, SIGNATURE, "not a key").is_err());
    }

    #[test]
    fn the_build_embeds_a_commit_and_a_build_time() {
        assert!(
            COMMIT == "unknown"
                || (COMMIT.len() == 40 && COMMIT.bytes().all(|b| b.is_ascii_hexdigit())),
            "{COMMIT}"
        );
        assert!(BUILT_AT.ends_with('Z'), "{BUILT_AT}");
        // Only a test build can have another version.
        #[cfg(not(feature = "update-test-key"))]
        assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
        #[cfg(not(feature = "update-test-key"))]
        assert_eq!(public_key(), key::PUBLIC_KEY);
        #[cfg(not(feature = "update-test-key"))]
        assert_eq!(base_url(), check::BASE_URL);
        chrono::DateTime::parse_from_rfc3339(BUILT_AT).unwrap();
    }
}
