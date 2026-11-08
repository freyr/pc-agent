//! The update check: download the manifest and its signature of a channel,
//! verify them, compare the release with the running build and choose the
//! asset of this platform. It installs nothing.

use std::fmt;
use std::fs;
use std::path::Path;
use std::process::ExitCode;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, FixedOffset};
use serde::Deserialize;

use super::{Asset, BUILT_AT, COMMIT, Manifest, key, verify};
use crate::config::Update;

/// The releases of the project. `stable` reads
/// `<base>/latest/download/<file>`, `edge` reads `<base>/download/edge/<file>`.
pub const BASE_URL: &str = "https://github.com/freyr/pc-agent/releases";

/// The limit of one download (connect, redirects, body).
const TIMEOUT: Duration = Duration::from_secs(30);
/// The largest manifest or signature that the check reads.
const BODY_LIMIT: u64 = 64 * 1024;

/// The release channel: `[update] channel` and `--channel`.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    /// The newest release that is not a pre-release, compared by semver.
    #[default]
    Stable,
    /// The `edge` pre-release (each push to `main`), compared by commit and
    /// build time.
    Edge,
}

impl fmt::Display for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Channel::Stable => "stable",
            Channel::Edge => "edge",
        })
    }
}

impl FromStr for Channel {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "stable" => Ok(Channel::Stable),
            "edge" => Ok(Channel::Edge),
            other => bail!("unknown channel {other:?} (stable or edge)"),
        }
    }
}

/// The name of this platform in the manifest: `x86_64-linux`,
/// `aarch64-macos` or `x86_64-windows`. Another target gets a name that no
/// manifest has, so its check fails.
pub fn platform() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

/// The version, commit and build time of a build.
#[derive(Clone, Copy, Debug)]
pub struct Build<'a> {
    pub version: &'a str,
    pub commit: &'a str,
    pub built_at: &'a str,
}

impl Build<'static> {
    /// This binary.
    pub const RUNNING: Build<'static> = Build {
        version: env!("CARGO_PKG_VERSION"),
        commit: COMMIT,
        built_at: BUILT_AT,
    };
}

/// The result of a compare.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// The release is this build, or an older version.
    UpToDate,
    /// `edge` only: a different build that is not newer than this one (for
    /// example a branch build that is newer than `edge`). No update.
    Older,
    /// A newer release; the asset of this platform.
    Available(Asset),
    /// A newer release that failed once after an install (its commit is in
    /// the failed list). Not installed automatically.
    FailedBefore(Asset),
}

/// Compares a verified manifest with the running build. The asset of
/// `platform` must be in the manifest, also when no update is available.
/// `failed` holds the commits of the builds that failed after an install.
pub fn compare(
    manifest: &Manifest,
    channel: Channel,
    running: Build,
    platform: &str,
    failed: &[String],
) -> Result<Outcome> {
    let asset = manifest
        .platforms
        .get(platform)
        .with_context(|| format!("the manifest has no asset for the platform {platform}"))?;
    let newer = match channel {
        Channel::Stable => {
            version(&manifest.version, "manifest version")?
                > version(running.version, "running version")?
        }
        Channel::Edge => {
            if manifest.commit == running.commit {
                false
            } else if time(&manifest.built_at, "manifest built_at")?
                > time(running.built_at, "running built_at")?
            {
                true
            } else {
                return Ok(Outcome::Older);
            }
        }
    };
    Ok(if !newer {
        Outcome::UpToDate
    } else if failed.contains(&manifest.commit) {
        Outcome::FailedBefore(asset.clone())
    } else {
        Outcome::Available(asset.clone())
    })
}

/// The URL of `file` (`manifest.json` or `manifest.json.minisig`) of the
/// release of `channel` under `base`.
fn url(base: &str, channel: Channel, file: &str) -> String {
    let base = base.trim_end_matches('/');
    match channel {
        Channel::Stable => format!("{base}/latest/download/{file}"),
        Channel::Edge => format!("{base}/download/edge/{file}"),
    }
}

/// Downloads the manifest and its signature of `channel` from `base` and
/// returns the manifest only when the signature verifies with `public_key`.
pub fn fetch(base: &str, channel: Channel, public_key: &str) -> Result<Manifest> {
    let agent = agent();
    let manifest = download(&agent, &url(base, channel, "manifest.json")).map_err(|e| {
        match e.downcast_ref::<ureq::Error>() {
            Some(ureq::Error::StatusCode(404)) => match channel {
                Channel::Stable => anyhow!(
                    "the latest stable release has no manifest (it is older than the updater)"
                ),
                Channel::Edge => anyhow!(
                    "the edge release has no manifest (a build is in progress, or it is older \
                     than the updater)"
                ),
            },
            _ => e,
        }
    })?;
    let signature = download(&agent, &url(base, channel, "manifest.json.minisig"))?;
    let signature = String::from_utf8(signature).context("the signature is not text")?;
    verify(&manifest, &signature, public_key)
}

/// An HTTPS client: rustls with the ring crypto provider and the webpki
/// roots (no OS certificate store), a timeout, and the redirects of GitHub to
/// its asset host.
fn agent() -> ureq::Agent {
    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::Rustls)
        .build();
    ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .tls_config(tls)
        .build()
        .into()
}

/// The body of a GET of `url`, at most `BODY_LIMIT` bytes.
fn download(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>> {
    let mut response = agent
        .get(url)
        .call()
        .map_err(anyhow::Error::from)
        .with_context(|| format!("cannot download {url}"))?;
    response
        .body_mut()
        .with_config()
        .limit(BODY_LIMIT)
        .read_to_vec()
        .with_context(|| format!("cannot read {url} (at most {BODY_LIMIT} bytes)"))
}

/// `fetch` and `compare`.
pub fn check(
    base: &str,
    channel: Channel,
    public_key: &str,
    running: Build,
    platform: &str,
    failed: &[String],
) -> Result<(Manifest, Outcome)> {
    let manifest = fetch(base, channel, public_key)?;
    let outcome = compare(&manifest, channel, running, platform, failed)?;
    Ok((manifest, outcome))
}

/// The channel of `update --check`: `flag` (`--channel`), else the
/// `[update]` section of the config file at `config`, else `stable` when
/// that file is missing or not readable. Returns the channel and where it
/// comes from.
fn channel_of(flag: Option<Channel>, config: &Path) -> Result<(Channel, String)> {
    if let Some(channel) = flag {
        return Ok((channel, "from --channel".to_string()));
    }
    let path = config.display();
    match fs::read_to_string(config) {
        Ok(text) => {
            let update = Update::parse_section(&text)
                .with_context(|| format!("invalid config file {path}"))?;
            Ok((update.channel, format!("from {path}")))
        }
        Err(e) => Ok((
            Channel::Stable,
            format!("the default: cannot read config file {path}: {e}"),
        )),
    }
}

/// `pc-agent update --check [--channel stable|edge]`: prints the running
/// build, the channel, the release of the channel and the result. Installs
/// nothing. Exit 0 when the check gives a result, 1 on an error.
pub fn check_command(flag: Option<Channel>, config: &Path) -> ExitCode {
    let running = Build::RUNNING;
    let short = running.commit.get(..7).unwrap_or(running.commit);
    println!(
        "running: {} commit {short} built_at {}",
        running.version, running.built_at
    );
    let result = channel_of(flag, config).and_then(|(channel, from)| {
        println!("channel: {channel} ({from})");
        check(
            BASE_URL,
            channel,
            key::PUBLIC_KEY,
            running,
            &platform(),
            &[],
        )
    });
    match result {
        Ok((manifest, outcome)) => {
            println!(
                "manifest: {} commit {} built_at {}",
                manifest.version, manifest.commit, manifest.built_at
            );
            match outcome {
                Outcome::UpToDate => println!("up to date"),
                Outcome::Older => println!("up to date (the release is older than this build)"),
                Outcome::Available(asset) => println!("update available: {}", asset.asset),
                Outcome::FailedBefore(asset) => println!(
                    "newer build failed before: {} commit {} ({})",
                    manifest.version, manifest.commit, asset.asset
                ),
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// An RFC 3339 time of a manifest or of this build.
fn time(text: &str, what: &str) -> Result<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(text).with_context(|| format!("bad {what} {text:?}"))
}

/// A semver version of a manifest or of this build.
fn version(text: &str, what: &str) -> Result<semver::Version> {
    semver::Version::parse(text).with_context(|| format!("bad {what} {text:?}"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    use super::*;
    use crate::update::key::PUBLIC_KEY;
    use crate::update::tests::{MANIFEST, SIGNATURE, TEST_KEY};

    const COMMIT_A: &str = "4cf806aab3b9e17c29f7ea12c20f512b2557e5b6";
    const COMMIT_B: &str = "98db6ed000000000000000000000000000000000";

    fn manifest(version: &str, commit: &str, built_at: &str) -> Manifest {
        let mut platforms = BTreeMap::new();
        for name in ["x86_64-linux", "aarch64-macos", "x86_64-windows"] {
            platforms.insert(
                name.to_string(),
                Asset {
                    asset: format!("pc-agent-{name}"),
                    sha256: "00".repeat(32),
                },
            );
        }
        Manifest {
            version: version.to_string(),
            commit: commit.to_string(),
            built_at: built_at.to_string(),
            platforms,
        }
    }

    fn build<'a>(version: &'a str, commit: &'a str, built_at: &'a str) -> Build<'a> {
        Build {
            version,
            commit,
            built_at,
        }
    }

    fn asset(platform: &str) -> Asset {
        Asset {
            asset: format!("pc-agent-{platform}"),
            sha256: "00".repeat(32),
        }
    }

    const T1: &str = "2026-11-08T10:00:00Z";
    const T2: &str = "2026-11-08T16:52:57Z";

    #[test]
    fn stable_offers_only_a_higher_version() {
        let m = manifest("0.3.0", COMMIT_A, T1);
        let cmp = |v| {
            compare(
                &m,
                Channel::Stable,
                build(v, COMMIT_B, T2),
                "x86_64-linux",
                &[],
            )
        };
        assert_eq!(
            cmp("0.2.0").unwrap(),
            Outcome::Available(asset("x86_64-linux"))
        );
        assert_eq!(
            cmp("0.3.0-rc.1").unwrap(),
            Outcome::Available(asset("x86_64-linux"))
        );
        assert_eq!(cmp("0.3.0").unwrap(), Outcome::UpToDate);
        assert_eq!(cmp("0.10.0").unwrap(), Outcome::UpToDate);
        assert_eq!(cmp("1.0.0").unwrap(), Outcome::UpToDate);
    }

    #[test]
    fn stable_ignores_commit_and_build_time() {
        // An edge build of the same version is up to date on stable, also
        // when it is older than the release.
        let m = manifest("0.3.0", COMMIT_A, T2);
        let r = build("0.3.0", COMMIT_B, T1);
        assert_eq!(
            compare(&m, Channel::Stable, r, "aarch64-macos", &[]).unwrap(),
            Outcome::UpToDate
        );
    }

    #[test]
    fn a_bad_version_is_an_error() {
        let m = manifest("v0.3", COMMIT_A, T1);
        let r = build("0.2.0", COMMIT_B, T2);
        assert!(compare(&m, Channel::Stable, r, "x86_64-linux", &[]).is_err());
        let m = manifest("0.3.0", COMMIT_A, T1);
        let r = build("dev", COMMIT_B, T2);
        assert!(compare(&m, Channel::Stable, r, "x86_64-linux", &[]).is_err());
    }

    #[test]
    fn edge_offers_a_different_and_later_build() {
        let m = manifest("0.2.0", COMMIT_A, T2);
        let r = build("0.2.0", COMMIT_B, T1);
        assert_eq!(
            compare(&m, Channel::Edge, r, "x86_64-windows", &[]).unwrap(),
            Outcome::Available(asset("x86_64-windows"))
        );
        // A build with no git (`unknown`, 1970) is older than each release.
        let r = build("0.2.0", "unknown", "1970-01-01T00:00:00Z");
        assert_eq!(
            compare(&m, Channel::Edge, r, "x86_64-windows", &[]).unwrap(),
            Outcome::Available(asset("x86_64-windows"))
        );
    }

    #[test]
    fn edge_with_the_same_commit_is_up_to_date() {
        let m = manifest("0.2.0", COMMIT_A, T2);
        let r = build("0.2.0", COMMIT_A, T2);
        assert_eq!(
            compare(&m, Channel::Edge, r, "x86_64-linux", &[]).unwrap(),
            Outcome::UpToDate
        );
    }

    #[test]
    fn edge_with_an_older_or_equal_build_time_gives_no_update() {
        let m = manifest("0.3.0", COMMIT_A, T1);
        let r = build("0.2.0", COMMIT_B, T2);
        assert_eq!(
            compare(&m, Channel::Edge, r, "x86_64-linux", &[]).unwrap(),
            Outcome::Older
        );
        let m = manifest("0.3.0", COMMIT_A, T2);
        assert_eq!(
            compare(&m, Channel::Edge, r, "x86_64-linux", &[]).unwrap(),
            Outcome::Older
        );
    }

    #[test]
    fn a_bad_build_time_is_an_error() {
        let m = manifest("0.2.0", COMMIT_A, "yesterday");
        let r = build("0.2.0", COMMIT_B, T1);
        assert!(compare(&m, Channel::Edge, r, "x86_64-linux", &[]).is_err());
    }

    #[test]
    fn a_failed_build_is_reported_and_not_offered() {
        let failed = vec![COMMIT_A.to_string()];
        let m = manifest("0.3.0", COMMIT_A, T2);
        let r = build("0.2.0", COMMIT_B, T1);
        for channel in [Channel::Stable, Channel::Edge] {
            assert_eq!(
                compare(&m, channel, r, "x86_64-linux", &failed).unwrap(),
                Outcome::FailedBefore(asset("x86_64-linux"))
            );
        }
        // A newer build than the failed one is offered again.
        let m = manifest("0.3.1", COMMIT_B, T2);
        let r = build("0.2.0", "unknown", T1);
        assert_eq!(
            compare(&m, Channel::Stable, r, "x86_64-linux", &failed).unwrap(),
            Outcome::Available(asset("x86_64-linux"))
        );
    }

    #[test]
    fn a_missing_platform_is_an_error() {
        let m = manifest("0.3.0", COMMIT_A, T2);
        for r in [build("0.2.0", COMMIT_B, T1), build("0.3.0", COMMIT_A, T2)] {
            let e = compare(&m, Channel::Stable, r, "aarch64-linux", &[]).unwrap_err();
            assert!(format!("{e:#}").contains("aarch64-linux"), "{e:#}");
        }
    }

    #[test]
    fn the_platform_name_has_the_manifest_form() {
        let name = platform();
        #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
        assert_eq!(name, "x86_64-linux");
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        assert_eq!(name, "aarch64-macos");
        #[cfg(all(target_arch = "x86_64", windows))]
        assert_eq!(name, "x86_64-windows");
        assert!(name.contains('-'), "{name}");
    }

    #[test]
    fn channel_names() {
        assert_eq!("stable".parse::<Channel>().unwrap(), Channel::Stable);
        assert_eq!("edge".parse::<Channel>().unwrap(), Channel::Edge);
        assert!("beta".parse::<Channel>().is_err());
        assert_eq!(Channel::Edge.to_string(), "edge");
        assert_eq!(Channel::default(), Channel::Stable);
    }

    #[test]
    fn urls_of_the_channels() {
        assert_eq!(
            url(BASE_URL, Channel::Stable, "manifest.json"),
            "https://github.com/freyr/pc-agent/releases/latest/download/manifest.json"
        );
        assert_eq!(
            url(BASE_URL, Channel::Edge, "manifest.json.minisig"),
            "https://github.com/freyr/pc-agent/releases/download/edge/manifest.json.minisig"
        );
    }

    fn temp_config(name: &str, text: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pc-agent-check-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn the_channel_flag_wins_over_the_config_file() {
        let path = temp_config("flag.toml", "[update]\nchannel = \"stable\"\n");
        let (channel, from) = channel_of(Some(Channel::Edge), &path).unwrap();
        assert_eq!(channel, Channel::Edge);
        assert!(from.contains("--channel"), "{from}");
        // The flag needs no config file at all, also a broken one.
        let broken = temp_config("broken-flag.toml", "[update]\nchannel = \"beta\"\n");
        assert_eq!(
            channel_of(Some(Channel::Stable), &broken).unwrap().0,
            Channel::Stable
        );
    }

    #[test]
    fn the_channel_comes_from_the_config_file() {
        let path = temp_config(
            "edge.toml",
            "[mqtt]\nhost = \"x\"\n\n[update]\nchannel = \"edge\"\n",
        );
        let (channel, from) = channel_of(None, &path).unwrap();
        assert_eq!(channel, Channel::Edge);
        assert!(from.contains(&path.display().to_string()), "{from}");
        let path = temp_config("none.toml", "[gpu]\nenabled = false\n");
        assert_eq!(channel_of(None, &path).unwrap().0, Channel::Stable);
    }

    #[test]
    fn no_config_file_gives_stable_with_a_note() {
        let path = std::env::temp_dir().join("pc-agent-check-no-such-dir/config.toml");
        let (channel, from) = channel_of(None, &path).unwrap();
        assert_eq!(channel, Channel::Stable);
        assert!(from.contains("cannot read"), "{from}");
    }

    #[test]
    fn a_bad_channel_in_the_config_file_is_an_error() {
        let path = temp_config("beta.toml", "[update]\nchannel = \"beta\"\n");
        let e = channel_of(None, &path).unwrap_err();
        assert!(
            format!("{e:#}").contains(&path.display().to_string()),
            "{e:#}"
        );
    }

    /// A reply of the test server.
    enum Reply {
        Ok(Vec<u8>),
        Redirect(&'static str),
    }

    /// An HTTP server on 127.0.0.1 that answers each GET of a path in
    /// `routes`, and 404 for each other path. Returns the base URL.
    fn serve(routes: Vec<(&'static str, Reply)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                // The headers end at an empty line.
                loop {
                    let mut header = String::new();
                    match reader.read_line(&mut header) {
                        Ok(0) | Err(_) => break,
                        Ok(_) if header.trim().is_empty() => break,
                        Ok(_) => {}
                    }
                }
                let reply = routes.iter().find(|(p, _)| *p == path).map(|(_, r)| r);
                let (head, body): (String, &[u8]) = match reply {
                    Some(Reply::Ok(body)) => ("200 OK".into(), body),
                    Some(Reply::Redirect(to)) => (format!("302 Found\r\nLocation: {to}"), b""),
                    None => ("404 Not Found".into(), b"Not Found"),
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(body);
            }
        });
        base
    }

    /// The signed fixture as the `stable` release, the signature through a
    /// redirect (as GitHub does).
    fn stable_release(manifest: &[u8]) -> String {
        serve(vec![
            (
                "/latest/download/manifest.json",
                Reply::Ok(manifest.to_vec()),
            ),
            (
                "/latest/download/manifest.json.minisig",
                Reply::Redirect("/assets/manifest.json.minisig"),
            ),
            ("/assets/manifest.json.minisig", Reply::Ok(SIGNATURE.into())),
        ])
    }

    #[test]
    fn a_newer_signed_release_gives_an_update() {
        let base = stable_release(MANIFEST);
        let (m, outcome) = check(
            &base,
            Channel::Stable,
            TEST_KEY,
            build("0.1.9", COMMIT_B, T1),
            "x86_64-windows",
            &[],
        )
        .unwrap();
        assert_eq!(m.commit, COMMIT_A);
        let Outcome::Available(a) = outcome else {
            panic!("{outcome:?}")
        };
        assert_eq!(a.asset, "pc-agent-x86_64-windows.exe");
    }

    #[test]
    fn the_same_or_an_older_signed_release_gives_none() {
        let base = stable_release(MANIFEST);
        for version in ["0.2.0", "0.2.1"] {
            let (_, outcome) = check(
                &base,
                Channel::Stable,
                TEST_KEY,
                build(version, COMMIT_B, T1),
                "x86_64-linux",
                &[],
            )
            .unwrap();
            assert_eq!(outcome, Outcome::UpToDate, "{version}");
        }
    }

    #[test]
    fn a_manifest_of_a_different_key_gives_an_error() {
        let base = stable_release(MANIFEST);
        let r = build("0.1.0", COMMIT_B, T1);
        let e = check(&base, Channel::Stable, PUBLIC_KEY, r, "x86_64-linux", &[]).unwrap_err();
        assert!(format!("{e:#}").contains("signature"), "{e:#}");
    }

    #[test]
    fn a_changed_manifest_gives_an_error() {
        let text = std::str::from_utf8(MANIFEST).unwrap();
        let changed = text.replace("\"0.2.0\"", "\"9.2.0\"");
        let base = stable_release(changed.as_bytes());
        let r = build("0.1.0", COMMIT_B, T1);
        let e = check(&base, Channel::Stable, TEST_KEY, r, "x86_64-linux", &[]).unwrap_err();
        assert!(format!("{e:#}").contains("signature"), "{e:#}");
    }

    #[test]
    fn a_missing_platform_in_a_signed_release_gives_an_error() {
        let base = stable_release(MANIFEST);
        let r = build("0.1.0", COMMIT_B, T1);
        assert!(check(&base, Channel::Stable, TEST_KEY, r, "riscv64-linux", &[]).is_err());
    }

    #[test]
    fn an_older_signed_edge_build_gives_no_update() {
        let base = serve(vec![
            ("/download/edge/manifest.json", Reply::Ok(MANIFEST.to_vec())),
            (
                "/download/edge/manifest.json.minisig",
                Reply::Ok(SIGNATURE.into()),
            ),
        ]);
        let newer = build("0.2.0", COMMIT_B, "2026-11-09T00:00:00Z");
        let (_, outcome) =
            check(&base, Channel::Edge, TEST_KEY, newer, "x86_64-linux", &[]).unwrap();
        assert_eq!(outcome, Outcome::Older);
        let older = build("0.2.0", COMMIT_B, T1);
        let (_, outcome) =
            check(&base, Channel::Edge, TEST_KEY, older, "x86_64-linux", &[]).unwrap();
        assert!(matches!(outcome, Outcome::Available(_)), "{outcome:?}");
    }

    #[test]
    fn a_missing_manifest_names_the_channel() {
        let base = serve(vec![]);
        let r = build("0.1.0", COMMIT_B, T1);
        let e = fetch(&base, Channel::Stable, TEST_KEY).unwrap_err();
        assert!(
            format!("{e:#}").contains("latest stable release has no manifest"),
            "{e:#}"
        );
        let e = check(&base, Channel::Edge, TEST_KEY, r, "x86_64-linux", &[]).unwrap_err();
        assert!(
            format!("{e:#}").contains("edge release has no manifest"),
            "{e:#}"
        );
    }

    #[test]
    fn a_missing_signature_gives_an_error() {
        let base = serve(vec![(
            "/latest/download/manifest.json",
            Reply::Ok(MANIFEST.to_vec()),
        )]);
        assert!(fetch(&base, Channel::Stable, TEST_KEY).is_err());
    }

    #[test]
    fn a_too_large_manifest_gives_an_error() {
        let mut big = MANIFEST.to_vec();
        big.resize(BODY_LIMIT as usize + 1, b' ');
        let base = stable_release(&big);
        let e = fetch(&base, Channel::Stable, TEST_KEY).unwrap_err();
        assert!(format!("{e:#}").contains("at most 65536 bytes"), "{e:#}");
    }

    #[test]
    fn no_server_gives_an_error() {
        // A port that nothing listens on.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        assert!(fetch(&base, Channel::Edge, TEST_KEY).is_err());
    }
}
