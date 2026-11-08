//! Embeds the commit and the build time of the binary for the self-update:
//! `PC_AGENT_COMMIT` (the full commit of HEAD) and `PC_AGENT_BUILD_TIME` (the
//! commit time of HEAD, UTC, RFC 3339).
//!
//! The build time is the commit time, not the clock: every job of a release
//! run checks out the same commit, so the three binaries and `manifest.json`
//! (`scripts/manifest.sh`, the same rule) get the same value with nothing
//! passed between the jobs, and a re-run gives the same value again.
//!
//! With no git (a source tarball, or a checkout that git cannot read), the
//! commit is `unknown` and the time is 1970-01-01T00:00:00Z: such a build is
//! older than each release.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let commit = git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    let seconds = git(&["log", "-1", "--format=%ct", "HEAD"])
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    println!("cargo:rustc-env=PC_AGENT_COMMIT={commit}");
    println!("cargo:rustc-env=PC_AGENT_BUILD_TIME={}", rfc3339(seconds));

    // A new commit changes HEAD or the file of the branch it points to.
    let mut watched = vec!["HEAD".to_string(), "packed-refs".to_string()];
    if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
        watched.push(branch);
    }
    for name in watched {
        if let Some(path) = git(&["rev-parse", "--git-path", &name])
            && Path::new(&path).exists()
        {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}

/// The trimmed output of a git command, or `None` when it fails.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// Seconds since the epoch as `YYYY-MM-DDTHH:MM:SSZ` (a build script has no
/// chrono). The date is the civil-from-days algorithm of Howard Hinnant.
fn rfc3339(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let secs = seconds.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs / 3_600,
        secs % 3_600 / 60,
        secs % 60
    )
}
