//! The update schedule of the service and the state of the HA `update`
//! entity. The service checks 10 min after the start of its run loop and
//! then every 24 h of wall time, only while connected. The 24 h use the
//! wall clock, because the tokio clock (CLOCK_MONOTONIC on Linux,
//! CLOCK_UPTIME_RAW on macOS) stops while the PC sleeps. With `[update] auto`, a check
//! installs an available update when the last automatic install is 7 days
//! old or more. The install command of HA checks at once and installs an
//! available update, also with `auto` off. A build that failed after an
//! install is never installed here (only `pc-agent update` installs it).
//!
//! The check and the install block (HTTPS downloads, file renames), so they
//! run on a blocking thread, and the run loop keeps polling the MQTT event
//! loop. One job runs at a time.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::json;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{info, warn};

use super::check::{self, Build, Channel, Outcome};
use super::install;
use super::pending::{self, Paths};
use super::{Asset, Manifest};
use crate::entities::UPDATE_INSTALL;

/// The first check after the start of the run loop.
pub const FIRST_CHECK: Duration = Duration::from_secs(10 * 60);
/// The wall time between two scheduled checks.
pub const CHECK_EVERY: TimeDelta = TimeDelta::hours(24);
/// The tick at which the service compares the wall clock with the last
/// scheduled check.
pub const TICK: Duration = Duration::from_secs(60);
/// The shortest time between two automatic installs.
pub const AUTO_EVERY: TimeDelta = TimeDelta::days(7);
/// The `title` of the entity state.
const TITLE: &str = "pc-agent";

/// What started a check.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Trigger {
    /// The 10 min / 24 h schedule: installs only with `auto` and the 7-day rule.
    Schedule,
    /// The install command of HA: installs an available update.
    Command,
}

/// The network and install calls of a check and an install. The service
/// uses `System`; the tests use a fake.
pub trait Effects: Send + Sync + 'static {
    /// Downloads and verifies the manifest and compares it with this build.
    /// `failed` holds the commits of the builds that failed after an install.
    fn check(&self, failed: &[String]) -> Result<(Manifest, Outcome)>;
    /// Steps 1 to 3 of the install (download, keep prev, pending file,
    /// rename) under the install lock.
    fn install(&self, manifest: &Manifest, asset: &Asset) -> Result<()>;
}

/// The release server of this build, the channel of the config, and the
/// files of this PC.
struct System {
    paths: Paths,
    base: String,
    channel: Channel,
}

impl Effects for System {
    fn check(&self, failed: &[String]) -> Result<(Manifest, Outcome)> {
        check::check(
            &self.base,
            self.channel,
            super::public_key(),
            Build::RUNNING,
            &check::platform(),
            failed,
        )
    }

    fn install(&self, manifest: &Manifest, asset: &Asset) -> Result<()> {
        let lock = install::lock(&self.paths)?;
        install::install(
            &lock,
            &self.paths,
            &self.base,
            self.channel,
            manifest,
            asset,
            Build::RUNNING,
        )
    }
}

/// The result of a job.
#[derive(Debug)]
pub enum Done {
    Checked(Trigger, Result<(Manifest, Outcome)>),
    Installed(Trigger, Result<()>),
    /// The job panicked.
    Lost(String),
}

/// What the run loop does after a job.
#[derive(Debug, PartialEq)]
pub enum Step {
    /// Publish the entity state (`state_payload`).
    Publish,
    /// The new binary is in place: end the run loop with `Exit::Restart`.
    Restart,
}

/// The schedule, the job and the entity state of the service.
pub struct Updater {
    /// `None`: no schedule and no install (no state folder, console tests);
    /// the entity still shows the installed version.
    sched: Option<Sched>,
    channel: Channel,
    running: Build<'static>,
    /// The latest version that the last check found (entity format), or
    /// `None` before the first check.
    latest: Option<(String, String)>,
    /// The `release_summary` of the entity: why a newer build does not
    /// install, or empty.
    summary: String,
    in_progress: bool,
}

struct Sched {
    paths: Paths,
    effects: Arc<dyn Effects>,
    auto: bool,
    /// The first scheduled check (tokio clock), until it starts.
    first_due: Option<Instant>,
    /// After the first check: the next compare of the wall clock with
    /// `last_scheduled` (tokio clock).
    tick_at: Instant,
    /// The wall time of the start of the last scheduled check.
    last_scheduled: Option<DateTime<Utc>>,
    job: Option<JoinHandle<Done>>,
}

impl Updater {
    /// The updater of the service: the state folder of this OS, the release
    /// URL of this build and the `[update]` section of the config.
    pub fn system(cfg: &crate::config::Update) -> Updater {
        match Paths::system() {
            Ok(paths) => {
                let effects = Arc::new(System {
                    paths: paths.clone(),
                    base: super::base_url(),
                    channel: cfg.channel,
                });
                Updater::new(
                    paths,
                    effects,
                    cfg.auto,
                    cfg.channel,
                    Build::RUNNING,
                    Instant::now(),
                )
            }
            Err(e) => {
                warn!("no update schedule: {e:#}");
                Updater::disabled(cfg.channel)
            }
        }
    }

    /// An updater that checks at `start` + 10 min.
    pub fn new(
        paths: Paths,
        effects: Arc<dyn Effects>,
        auto: bool,
        channel: Channel,
        running: Build<'static>,
        start: Instant,
    ) -> Updater {
        Updater {
            sched: Some(Sched {
                paths,
                effects,
                auto,
                first_due: Some(start + FIRST_CHECK),
                tick_at: start + FIRST_CHECK,
                last_scheduled: None,
                job: None,
            }),
            channel,
            running,
            latest: None,
            summary: String::new(),
            in_progress: false,
        }
    }

    /// No schedule and no install command.
    pub fn disabled(channel: Channel) -> Updater {
        Updater {
            sched: None,
            channel,
            running: Build::RUNNING,
            latest: None,
            summary: String::new(),
            in_progress: false,
        }
    }

    /// The JSON state of the entity. Before the first check, and when the
    /// check finds no newer build or a failed one, `latest_version` is
    /// `installed_version`:
    /// HA keeps the earlier value of a missing key, and shows `unknown`
    /// with no value. All keys are always present for the same reason.
    pub fn state_payload(&self) -> String {
        let installed = shown(self.channel, self.running.version, self.running.commit);
        let (latest, url) = match &self.latest {
            Some((latest, url)) => (latest.clone(), url.clone()),
            None => (
                installed.clone(),
                release_url(self.channel, self.running.version),
            ),
        };
        json!({
            "installed_version": installed,
            "latest_version": latest,
            "release_url": url,
            "release_summary": self.summary,
            "title": TITLE,
            "in_progress": self.in_progress,
        })
        .to_string()
    }

    /// The time at which the run loop calls `on_timer`: the first check
    /// 10 min after the start, then a tick every minute; `None` while a job
    /// runs or with no schedule. The run loop waits for it only while
    /// connected, so a tick that passed while disconnected comes at the
    /// next ConnAck.
    pub fn next_check(&self) -> Option<Instant> {
        self.sched
            .as_ref()
            .filter(|s| s.job.is_none())
            .map(|s| s.first_due.unwrap_or(s.tick_at))
    }

    /// A job runs.
    pub fn busy(&self) -> bool {
        self.sched.as_ref().is_some_and(|s| s.job.is_some())
    }

    /// The time of `next_check` passed (tokio time `now`, wall time
    /// `wall`). Starts the first scheduled check, or a later one when the
    /// last one started 24 h of wall time ago or more. A clock that went
    /// back moves the reference to `wall`: no check storm, and the next
    /// check comes at most 24 h later.
    pub fn on_timer(&mut self, now: Instant, wall: DateTime<Utc>) {
        let Some(s) = &mut self.sched else { return };
        if s.job.is_some() {
            return;
        }
        s.tick_at = now + TICK;
        let due = match (s.first_due, s.last_scheduled) {
            (None, Some(last)) if wall < last => {
                warn!(
                    "the clock went back from {last} to {wall}: the next update check is 24 h later"
                );
                s.last_scheduled = Some(wall);
                false
            }
            (None, Some(last)) => wall.signed_duration_since(last) >= CHECK_EVERY,
            _ => true,
        };
        if due {
            s.first_due = None;
            s.last_scheduled = Some(wall);
            self.start_check(Trigger::Schedule);
        }
    }

    /// A message on the command topic. Only a live `install` starts a check
    /// (a retained message is old: the broker sends it at the subscribe).
    /// Gives `true` when a check started.
    pub fn on_command(&mut self, payload: &[u8], retain: bool) -> bool {
        if retain {
            info!("ignored a retained update command");
            return false;
        }
        if payload != UPDATE_INSTALL.as_bytes() {
            warn!(
                payload = %String::from_utf8_lossy(payload),
                "ignored an unknown update command"
            );
            return false;
        }
        if self.sched.is_none() {
            warn!("ignored the install command: no update schedule");
            return false;
        }
        if self.busy() {
            info!("ignored the install command: a check or an install runs");
            return false;
        }
        info!("install command: check now");
        self.start_check(Trigger::Command)
    }

    fn start_check(&mut self, trigger: Trigger) -> bool {
        let Some(s) = &mut self.sched else {
            return false;
        };
        if s.job.is_some() {
            return false;
        }
        let paths = s.paths.clone();
        let effects = s.effects.clone();
        s.job = Some(tokio::task::spawn_blocking(move || {
            // Without the failed list a failed build could install: the
            // check fails when `update-state.json` cannot be read.
            let result = pending::read_state(&paths)
                .and_then(|state| effects.check(&state.failed_commits()));
            Done::Checked(trigger, result)
        }));
        true
    }

    /// The result of the running job; never completes with no job. Cancel
    /// safe: the job keeps running when the run loop takes another branch.
    pub async fn job_done(&mut self) -> Done {
        let Some(job) = self.sched.as_mut().and_then(|s| s.job.as_mut()) else {
            return std::future::pending().await;
        };
        let done = job
            .await
            .unwrap_or_else(|e| Done::Lost(format!("the update job failed: {e}")));
        if let Some(s) = &mut self.sched {
            s.job = None;
        }
        done
    }

    /// Takes the result of a job at the wall-clock time `now`: records the
    /// check, starts an install, or gives the restart after an install.
    pub fn on_job_done(&mut self, done: Done, now: DateTime<Utc>) -> Step {
        let Some(s) = &mut self.sched else {
            return Step::Publish;
        };
        match done {
            Done::Checked(trigger, Ok((manifest, outcome))) => {
                let mut state = match pending::read_state(&s.paths) {
                    Ok(state) => state,
                    Err(e) => {
                        warn!("update check: {e:#}");
                        return Step::Publish;
                    }
                };
                // A failed build shows as "up to date" (its Install button
                // would do nothing), with the reason in the summary.
                let shown_new = shown(self.channel, &manifest.version, &manifest.commit);
                self.summary = match outcome {
                    Outcome::FailedBefore(_) => format!(
                        "Build {shown_new} failed before and is not installed automatically; \
                         `pc-agent update` can install it."
                    ),
                    _ => String::new(),
                };
                self.latest = Some(if matches!(outcome, Outcome::Available(_)) {
                    (shown_new, release_url(self.channel, &manifest.version))
                } else {
                    (
                        shown(self.channel, self.running.version, self.running.commit),
                        release_url(self.channel, self.running.version),
                    )
                });
                info!(
                    version = %manifest.version,
                    commit = %manifest.commit,
                    ?trigger,
                    "update check: {}",
                    describe(&outcome)
                );
                let allowed = match trigger {
                    Trigger::Command => true,
                    Trigger::Schedule => {
                        s.auto && auto_due(state.last_auto_install.as_deref(), now)
                    }
                };
                state.last_check = Some(stamp(now));
                if let Err(e) = pending::write_state(&s.paths, &state) {
                    warn!("cannot record the update check: {e:#}");
                }
                // `false`: a failed build never installs from here.
                let asset = install::to_install(outcome, false).filter(|_| allowed);
                if let Some(asset) = asset {
                    info!(
                        version = %manifest.version,
                        commit = %manifest.commit,
                        ?trigger,
                        "installing the update"
                    );
                    self.in_progress = true;
                    let effects = s.effects.clone();
                    s.job = Some(tokio::task::spawn_blocking(move || {
                        Done::Installed(trigger, effects.install(&manifest, &asset))
                    }));
                }
                Step::Publish
            }
            Done::Checked(_, Err(e)) => {
                warn!("update check failed: {e:#}");
                Step::Publish
            }
            Done::Installed(trigger, Ok(())) => {
                if trigger == Trigger::Schedule {
                    let recorded = pending::read_state(&s.paths).and_then(|mut state| {
                        state.last_auto_install = Some(stamp(now));
                        pending::write_state(&s.paths, &state)
                    });
                    if let Err(e) = recorded {
                        warn!("cannot record the automatic install: {e:#}");
                    }
                }
                info!("the update is installed: restart with the new binary");
                Step::Restart
            }
            Done::Installed(_, Err(e)) => {
                warn!("the update install failed: {e:#}");
                self.in_progress = false;
                Step::Publish
            }
            Done::Lost(e) => {
                warn!("{e}");
                self.in_progress = false;
                Step::Publish
            }
        }
    }
}

/// An automatic install is allowed when the last one (RFC 3339) is 7 days
/// old or more, or when none is recorded (or the record does not parse).
pub fn auto_due(last: Option<&str>, now: DateTime<Utc>) -> bool {
    match last.map(DateTime::parse_from_rfc3339) {
        Some(Ok(last)) => now.signed_duration_since(last) >= AUTO_EVERY,
        Some(Err(_)) | None => true,
    }
}

/// A version in the entity: the version on `stable`, the version and the
/// short commit on `edge` (`0.3.0 (4cf806a)`). HA cannot compare the edge
/// form, so it shows an update whenever the two texts differ.
pub fn shown(channel: Channel, version: &str, commit: &str) -> String {
    match channel {
        Channel::Stable => version.to_string(),
        Channel::Edge => format!("{version} ({})", commit.get(..7).unwrap_or(commit)),
    }
}

/// The release page of `version` on `channel`.
fn release_url(channel: Channel, version: &str) -> String {
    match channel {
        Channel::Stable => format!("{}/tag/v{version}", check::BASE_URL),
        Channel::Edge => format!("{}/tag/edge", check::BASE_URL),
    }
}

fn stamp(time: DateTime<Utc>) -> String {
    time.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn describe(outcome: &Outcome) -> &'static str {
    match outcome {
        Outcome::UpToDate => "up to date",
        Outcome::Older => "up to date (the release is older than this build)",
        Outcome::Available(_) => "update available",
        Outcome::FailedBefore(_) => "newer build failed before; not installed automatically",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::Mutex;

    use anyhow::anyhow;

    use super::*;

    const OLD: &str = "1111111111111111111111111111111111111111";
    const NEW: &str = "2222222222222222222222222222222222222222";
    const RUNNING: Build<'static> = Build {
        version: "0.3.0",
        commit: OLD,
        built_at: "2026-11-08T10:00:00Z",
    };

    #[derive(Clone, Copy)]
    enum Kind {
        UpToDate,
        Available,
        FailedBefore,
    }

    /// A release server and an install that only count.
    struct Fake {
        kind: Kind,
        install_ok: bool,
        checks: Mutex<u32>,
        failed_seen: Mutex<Vec<Vec<String>>>,
        installs: Mutex<u32>,
    }

    impl Fake {
        fn with(kind: Kind, install_ok: bool) -> Arc<Fake> {
            Arc::new(Fake {
                kind,
                install_ok,
                checks: Mutex::new(0),
                failed_seen: Mutex::new(Vec::new()),
                installs: Mutex::new(0),
            })
        }

        fn new(kind: Kind) -> Arc<Fake> {
            Fake::with(kind, true)
        }

        fn checks(&self) -> u32 {
            *self.checks.lock().unwrap()
        }

        fn installs(&self) -> u32 {
            *self.installs.lock().unwrap()
        }
    }

    fn asset() -> Asset {
        Asset {
            asset: "pc-agent-test".to_string(),
            sha256: "00".repeat(32),
        }
    }

    fn manifest() -> Manifest {
        let mut platforms = BTreeMap::new();
        platforms.insert("x86_64-linux".to_string(), asset());
        Manifest {
            version: "0.3.1".to_string(),
            commit: NEW.to_string(),
            built_at: "2026-11-08T12:00:00Z".to_string(),
            platforms,
        }
    }

    impl Effects for Fake {
        fn check(&self, failed: &[String]) -> Result<(Manifest, Outcome)> {
            *self.checks.lock().unwrap() += 1;
            self.failed_seen.lock().unwrap().push(failed.to_vec());
            let outcome = match self.kind {
                Kind::UpToDate => Outcome::UpToDate,
                Kind::Available => Outcome::Available(asset()),
                Kind::FailedBefore => Outcome::FailedBefore(asset()),
            };
            Ok((manifest(), outcome))
        }

        fn install(&self, _: &Manifest, _: &Asset) -> Result<()> {
            *self.installs.lock().unwrap() += 1;
            if self.install_ok {
                Ok(())
            } else {
                Err(anyhow!("the download failed"))
            }
        }
    }

    fn paths(name: &str) -> Paths {
        let root =
            std::env::temp_dir().join(format!("pc-agent-schedule-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        Paths {
            state_dir: root.join("state"),
            exe: root.join("bin").join("pc-agent"),
        }
    }

    fn utc(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text).unwrap().to_utc()
    }

    fn updater(paths: &Paths, fake: &Arc<Fake>, auto: bool, channel: Channel) -> Updater {
        Updater::new(
            paths.clone(),
            fake.clone(),
            auto,
            channel,
            RUNNING,
            Instant::now(),
        )
    }

    fn state(u: &Updater) -> serde_json::Value {
        serde_json::from_str(&u.state_payload()).unwrap()
    }

    /// The jobs of one check (and of its install), as the run loop runs
    /// them, at the wall-clock time `now`. Gives the steps.
    async fn finish(u: &mut Updater, now: DateTime<Utc>) -> Vec<Step> {
        let mut steps = Vec::new();
        while u.busy() {
            let done = u.job_done().await;
            steps.push(u.on_job_done(done, now));
        }
        steps
    }

    /// Waits for the next wake of the schedule and calls `on_timer` with
    /// the wall time `wall`; gives `true` when a check started.
    async fn wake(u: &mut Updater, wall: DateTime<Utc>) -> bool {
        let at = u.next_check().unwrap();
        tokio::time::sleep_until(at).await;
        u.on_timer(Instant::now(), wall);
        u.busy()
    }

    #[tokio::test(start_paused = true)]
    async fn the_service_checks_10_min_after_its_start_then_every_24_h() {
        let paths = paths("timer");
        let fake = Fake::new(Kind::UpToDate);
        let start = Instant::now();
        let w0 = utc("2026-11-10T00:00:00Z");
        // A PC that never sleeps: the wall time moves with the tokio time.
        let wall = || w0 + TimeDelta::from_std(Instant::now() - start).unwrap();
        let mut u = updater(&paths, &fake, true, Channel::Stable);
        assert_eq!(u.next_check(), Some(start + Duration::from_secs(600)));
        let mut checks = Vec::new();
        let until = start + Duration::from_secs(3 * 24 * 3600);
        while u.next_check().is_some_and(|at| at < until) {
            tokio::time::sleep_until(u.next_check().unwrap()).await;
            u.on_timer(Instant::now(), wall());
            if u.busy() {
                assert_eq!(u.next_check(), None, "no timer while a job runs");
                checks.push(Instant::now() - start);
                assert_eq!(finish(&mut u, wall()).await, [Step::Publish]);
            }
            // The wake after a check is a short tick, not 24 h later.
            assert!(u.next_check().unwrap() - Instant::now() <= TICK);
        }
        let minutes: Vec<u64> = checks.iter().map(|d| d.as_secs() / 60).collect();
        assert_eq!(minutes, [10, 24 * 60 + 10, 48 * 60 + 10]);
        assert_eq!(fake.checks(), 3);
        assert_eq!(fake.installs(), 0);
        let recorded = pending::read_state(&paths).unwrap();
        assert_eq!(recorded.last_check.as_deref(), Some("2026-11-12T00:10:00Z"));
        assert_eq!(recorded.last_auto_install, None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_suspend_of_25_h_gives_a_check_at_the_next_tick() {
        let paths = paths("suspend");
        let fake = Fake::new(Kind::UpToDate);
        let w0 = utc("2026-11-20T12:00:00Z");
        let mut u = updater(&paths, &fake, true, Channel::Stable);
        assert!(wake(&mut u, w0).await, "the first check at 10 min");
        finish(&mut u, w0).await;
        for minute in 1..=3 {
            assert!(!wake(&mut u, w0 + TimeDelta::minutes(minute)).await);
        }
        // The PC sleeps for 25 h: the tokio clock (CLOCK_MONOTONIC,
        // CLOCK_UPTIME_RAW) stops, the wall clock moves on.
        let before = Instant::now();
        assert!(wake(&mut u, w0 + TimeDelta::hours(25)).await);
        assert!(Instant::now() - before <= TICK);
        finish(&mut u, w0 + TimeDelta::hours(25)).await;
        // The next tick gives no second check.
        assert!(!wake(&mut u, w0 + TimeDelta::hours(25) + TimeDelta::minutes(1)).await);
        assert_eq!(fake.checks(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_clock_set_back_gives_no_check_storm() {
        let paths = paths("clock-back");
        let fake = Fake::new(Kind::UpToDate);
        let w0 = utc("2026-11-20T12:00:00Z");
        let mut u = updater(&paths, &fake, true, Channel::Stable);
        assert!(wake(&mut u, w0).await);
        finish(&mut u, w0).await;
        // The clock goes back 3 h, then runs on for 23 h 59 min: no check.
        let back = w0 - TimeDelta::hours(3);
        for minute in (0..24 * 60).step_by(7) {
            assert!(
                !wake(&mut u, back + TimeDelta::minutes(minute)).await,
                "{minute}"
            );
        }
        // 24 h after the jump back: one check (the delay is at most 24 h).
        assert!(wake(&mut u, back + TimeDelta::hours(24)).await);
        finish(&mut u, back + TimeDelta::hours(24)).await;
        assert!(!wake(&mut u, back + TimeDelta::hours(24) + TimeDelta::minutes(1)).await);
        assert_eq!(fake.checks(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn an_automatic_install_needs_7_days_since_the_last_one() {
        let now = utc("2026-11-20T12:00:00Z");
        // (last automatic install, installs)
        let cases = [
            (Some("2026-11-13T12:00:01Z"), false),
            (Some("2026-11-13T12:00:00Z"), true),
            (None, true),
        ];
        for (i, (last, installs)) in cases.into_iter().enumerate() {
            let paths = paths(&format!("auto-{i}"));
            pending::write_state(
                &paths,
                &pending::UpdateState {
                    last_auto_install: last.map(str::to_string),
                    ..Default::default()
                },
            )
            .unwrap();
            let fake = Fake::new(Kind::Available);
            let mut u = updater(&paths, &fake, true, Channel::Stable);
            u.on_timer(Instant::now(), utc("2026-11-20T12:00:00Z"));
            let steps = finish(&mut u, now).await;
            let recorded = pending::read_state(&paths).unwrap();
            assert_eq!(recorded.last_check.as_deref(), Some("2026-11-20T12:00:00Z"));
            if installs {
                assert_eq!(steps, [Step::Publish, Step::Restart], "{last:?}");
                assert_eq!(fake.installs(), 1, "{last:?}");
                assert_eq!(state(&u)["in_progress"], true);
                assert_eq!(
                    recorded.last_auto_install.as_deref(),
                    Some("2026-11-20T12:00:00Z")
                );
            } else {
                assert_eq!(steps, [Step::Publish], "{last:?}");
                assert_eq!(fake.installs(), 0, "{last:?}");
                assert_eq!(state(&u)["in_progress"], false);
                assert_eq!(recorded.last_auto_install.as_deref(), last);
            }
            // The check updates the entity in both cases.
            assert_eq!(state(&u)["latest_version"], "0.3.1");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn no_automatic_install_with_auto_off() {
        let paths = paths("auto-off");
        let fake = Fake::new(Kind::Available);
        let mut u = updater(&paths, &fake, false, Channel::Stable);
        u.on_timer(Instant::now(), utc("2026-11-20T12:00:00Z"));
        assert_eq!(
            finish(&mut u, utc("2026-11-20T12:00:00Z")).await,
            [Step::Publish]
        );
        assert_eq!(fake.checks(), 1);
        assert_eq!(fake.installs(), 0);
        let s = state(&u);
        assert_eq!(s["installed_version"], "0.3.0");
        assert_eq!(s["latest_version"], "0.3.1");
        assert_eq!(s["release_summary"], "");
        assert_eq!(
            s["release_url"],
            "https://github.com/freyr/pc-mqtt-agent/releases/tag/v0.3.1"
        );
        assert_eq!(s["in_progress"], false);
    }

    #[tokio::test(start_paused = true)]
    async fn the_install_command_installs_also_with_auto_off() {
        let paths = paths("command");
        let fake = Fake::new(Kind::Available);
        let start = Instant::now();
        let mut u = updater(&paths, &fake, false, Channel::Stable);
        assert!(u.on_command(b"install", false));
        // A second command while the job runs does nothing.
        assert!(!u.on_command(b"install", false));
        let now = utc("2026-11-20T12:00:00Z");
        let done = u.job_done().await;
        assert_eq!(u.on_job_done(done, now), Step::Publish);
        // `in_progress` is published before the install job ends.
        assert!(u.busy());
        assert_eq!(state(&u)["in_progress"], true);
        let done = u.job_done().await;
        assert_eq!(u.on_job_done(done, now), Step::Restart);
        assert_eq!(fake.checks(), 1);
        assert_eq!(fake.installs(), 1);
        // Not an automatic install, and the schedule stays.
        assert_eq!(pending::read_state(&paths).unwrap().last_auto_install, None);
        assert_eq!(u.next_check(), Some(start + FIRST_CHECK));
    }

    #[tokio::test(start_paused = true)]
    async fn a_retained_or_unknown_command_does_nothing() {
        let paths = paths("retained");
        let fake = Fake::new(Kind::Available);
        let mut u = updater(&paths, &fake, true, Channel::Stable);
        assert!(!u.on_command(b"install", true));
        assert!(!u.on_command(b"INSTALL", false));
        assert!(!u.on_command(b"", false));
        assert!(!u.busy());
        tokio::task::yield_now().await;
        assert_eq!(fake.checks(), 0);
        // With no schedule, a live command does nothing either.
        let mut off = Updater::disabled(Channel::Stable);
        assert!(!off.on_command(b"install", false));
        assert_eq!(off.next_check(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_build_never_installs_from_the_schedule_or_the_command() {
        let paths = paths("failed");
        pending::write_state(
            &paths,
            &pending::UpdateState {
                failed: vec![pending::BuildId {
                    version: "0.3.1".to_string(),
                    commit: NEW.to_string(),
                }],
                ..Default::default()
            },
        )
        .unwrap();
        let fake = Fake::new(Kind::FailedBefore);
        let mut u = updater(&paths, &fake, true, Channel::Stable);
        let now = utc("2026-11-20T12:00:00Z");
        u.on_timer(Instant::now(), utc("2026-11-20T12:00:00Z"));
        assert_eq!(finish(&mut u, now).await, [Step::Publish]);
        assert!(u.on_command(b"install", false));
        assert_eq!(finish(&mut u, now).await, [Step::Publish]);
        assert_eq!(fake.installs(), 0);
        // HA shows "up to date" (no Install button that does nothing) and
        // says why.
        let s = state(&u);
        assert_eq!(s["latest_version"], "0.3.0");
        assert_eq!(
            s["release_summary"],
            "Build 0.3.1 failed before and is not installed automatically; \
             `pc-agent update` can install it."
        );
        // The check got the failed list of update-state.json.
        assert_eq!(
            *fake.failed_seen.lock().unwrap(),
            vec![vec![NEW.to_string()]; 2]
        );
        assert_eq!(pending::read_state(&paths).unwrap().failed.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_state_file_that_cannot_be_read_stops_the_check() {
        let paths = paths("bad-state");
        fs::create_dir_all(&paths.state_dir).unwrap();
        fs::write(paths.state(), "not json").unwrap();
        let fake = Fake::new(Kind::Available);
        let mut u = updater(&paths, &fake, true, Channel::Stable);
        u.on_timer(Instant::now(), utc("2026-11-20T12:00:00Z"));
        assert_eq!(
            finish(&mut u, utc("2026-11-20T12:00:00Z")).await,
            [Step::Publish]
        );
        assert_eq!(fake.checks(), 0);
        assert_eq!(fake.installs(), 0);
        assert_eq!(fs::read_to_string(paths.state()).unwrap(), "not json");
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_install_publishes_in_progress_false() {
        let paths = paths("install-fails");
        let fake = Fake::with(Kind::Available, false);
        let mut u = updater(&paths, &fake, true, Channel::Stable);
        u.on_timer(Instant::now(), utc("2026-11-20T12:00:00Z"));
        let steps = finish(&mut u, utc("2026-11-20T12:00:00Z")).await;
        assert_eq!(steps, [Step::Publish, Step::Publish]);
        assert_eq!(fake.installs(), 1);
        assert_eq!(state(&u)["in_progress"], false);
        // A failed install is not an automatic install: the next check tries again.
        assert_eq!(pending::read_state(&paths).unwrap().last_auto_install, None);
    }

    #[tokio::test(start_paused = true)]
    async fn before_the_first_check_the_entity_is_up_to_date() {
        let paths = paths("before");
        let fake = Fake::new(Kind::UpToDate);
        let u = updater(&paths, &fake, true, Channel::Stable);
        let s = state(&u);
        let mut keys: Vec<&str> = s.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "in_progress",
                "installed_version",
                "latest_version",
                "release_summary",
                "release_url",
                "title"
            ]
        );
        assert_eq!(s["release_summary"], "");
        assert_eq!(s["installed_version"], "0.3.0");
        assert_eq!(s["latest_version"], "0.3.0");
        assert_eq!(s["title"], "pc-agent");
        assert_eq!(s["in_progress"], false);
        assert_eq!(
            s["release_url"],
            "https://github.com/freyr/pc-mqtt-agent/releases/tag/v0.3.0"
        );
        // Edge: the version and the short commit.
        let edge = updater(&paths, &fake, true, Channel::Edge);
        let s = state(&edge);
        assert_eq!(s["installed_version"], "0.3.0 (1111111)");
        assert_eq!(s["latest_version"], "0.3.0 (1111111)");
        assert_eq!(
            s["release_url"],
            "https://github.com/freyr/pc-mqtt-agent/releases/tag/edge"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_edge_check_shows_the_new_commit() {
        let paths = paths("edge");
        let fake = Fake::new(Kind::Available);
        let mut u = updater(&paths, &fake, false, Channel::Edge);
        u.on_timer(Instant::now(), utc("2026-11-20T12:00:00Z"));
        finish(&mut u, utc("2026-11-20T12:00:00Z")).await;
        assert_eq!(state(&u)["latest_version"], "0.3.1 (2222222)");
        // An up-to-date check shows the installed build as the latest.
        let fake = Fake::new(Kind::UpToDate);
        let mut u = updater(&paths, &fake, false, Channel::Edge);
        u.on_timer(Instant::now(), utc("2026-11-20T12:00:00Z"));
        finish(&mut u, utc("2026-11-20T12:00:00Z")).await;
        assert_eq!(state(&u)["latest_version"], "0.3.0 (1111111)");
    }

    #[test]
    fn the_7_day_rule() {
        let now = utc("2026-11-20T12:00:00Z");
        assert!(auto_due(None, now));
        assert!(auto_due(Some("not a time"), now));
        assert!(auto_due(Some("2026-11-13T12:00:00Z"), now));
        assert!(auto_due(Some("2026-10-01T00:00:00Z"), now));
        assert!(!auto_due(Some("2026-11-13T12:00:01Z"), now));
        assert!(!auto_due(Some("2026-11-20T12:00:00Z"), now));
    }
}
