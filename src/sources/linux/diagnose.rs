//! `pc-agent diagnose`: prints what each Linux source reads and what the
//! agent makes of it, with the same rule functions as the agent. It reads no
//! configuration and prints no event data, only counts.

use std::fmt::Write;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use super::super::nvml;
use super::input::{self, DeviceInfo};
use super::rules::{self, AxisRole, Connector, LockTracker, Monitor, Session};
use super::{drm, hyprland, logind, os};
use crate::core::SessionState;
use crate::time::format_timestamp;

/// The time of the rest sampling of the input devices.
const REST: Duration = Duration::from_secs(10);

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn on_off(value: bool) -> &'static str {
    if value { "ON" } else { "OFF" }
}

/// The sessions and the one that the agent chooses.
pub fn format_sessions(sessions: &[Session]) -> String {
    let mut out = String::new();
    if sessions.is_empty() {
        out.push_str("  no sessions\n");
    }
    for s in sessions {
        let _ = writeln!(
            out,
            "  id {}  uid {}  name {}  class {}  type {}  remote {}  active {}  seat {}",
            s.id,
            s.uid,
            s.user,
            s.class,
            s.kind,
            yes_no(s.remote),
            yes_no(s.active),
            s.seat
        );
    }
    match rules::active_session(sessions) {
        Some(s) => {
            let _ = writeln!(out, "  chosen: session {} ({})", s.id, s.user);
        }
        None => out.push_str("  chosen: none (sessionstate Unknown)\n"),
    }
    out
}

/// The monitors of `j/monitors` and the `sessionstate` and `display` that
/// the agent gives them (with no last value for the "not known" rule).
pub fn format_monitors(monitors: &[Monitor]) -> String {
    let mut out = String::new();
    for m in monitors {
        let blocked = match &m.solitary_blocked_by {
            Some(list) => format!("[{}]", list.join(", ")),
            None => "absent".to_string(),
        };
        let _ = writeln!(
            out,
            "  monitor {}  disabled {}  dpmsStatus {}  solitaryBlockedBy {}",
            m.name,
            yes_no(m.disabled),
            yes_no(m.dpms_status),
            blocked
        );
    }
    let state = match LockTracker::new().update(true, Some(rules::lock_read(monitors))) {
        SessionState::Locked => "Locked",
        _ => "Unlocked",
    };
    let _ = writeln!(
        out,
        "  sessionstate {state}  display {}",
        on_off(rules::display_from_monitors(monitors))
    );
    out
}

/// The DRM connectors and the `display` of the DRM fallback.
pub fn format_connectors(connectors: &[Connector]) -> String {
    let mut out = String::new();
    if connectors.is_empty() {
        out.push_str("  no connectors\n");
    }
    for c in connectors {
        let _ = writeln!(out, "  {}  status {}  dpms {}", c.name, c.status, c.dpms);
    }
    match rules::display_from_drm(connectors) {
        Some(on) => {
            let _ = writeln!(out, "  display {}", on_off(on));
        }
        None => out.push_str("  display not known (no connected connector)\n"),
    }
    out
}

/// The input devices; for a gamepad also each axis with its role.
pub fn format_devices(devices: &[DeviceInfo]) -> String {
    let mut out = String::new();
    if devices.is_empty() {
        out.push_str("  no input devices\n");
    }
    for d in devices {
        let _ = writeln!(
            out,
            "  {}  name {:?}  accelerometer {}  gamepad {}{}",
            d.path.display(),
            d.name,
            yes_no(d.accelerometer),
            yes_no(d.caps.gamepad),
            if d.accelerometer { "  (skipped)" } else { "" }
        );
        if d.accelerometer || !d.caps.gamepad {
            continue;
        }
        let mut axes: Vec<_> = d.caps.axes.iter().collect();
        axes.sort_by_key(|(code, _)| **code);
        for (code, a) in axes {
            let role = match rules::axis_role(&d.caps, *code) {
                AxisRole::Stick => "stick",
                AxisRole::Trigger => "trigger",
                AxisRole::Ignored => "ignored",
            };
            let _ = writeln!(
                out,
                "    axis {code:#04x}  min {}  max {}  flat {}  fuzz {}  role {role}",
                a.min, a.max, a.flat, a.fuzz
            );
        }
    }
    out
}

/// The counted events of the rest sampling: (path, name, count). Only
/// counts, never an event type, code or value.
pub fn format_rest_counts(counts: &[(String, String, usize)]) -> String {
    let mut out = String::new();
    for (path, name, count) in counts {
        let _ = writeln!(out, "  {path} {name:?}: {count} counted events");
    }
    out
}

/// Runs the checks once and prints the result to stdout.
pub async fn run() {
    println!("== Version");
    println!("  pc-agent {}", crate::update::VERSION);
    let omarchy = std::fs::read_to_string(os::VERSION_PATH)
        .map(|t| t.trim().to_string())
        .unwrap_or_else(|_| "absent".to_string());
    println!("  omarchy {omarchy}");
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|t| t.trim().to_string())
        .unwrap_or_else(|_| "not known".to_string());
    println!("  kernel {kernel}");

    println!("== logind");
    let sessions = match logind::read_once().await {
        Ok(list) => {
            print!("{}", format_sessions(&list));
            Some(list)
        }
        Err(e) => {
            println!("  not available: {e}");
            None
        }
    };

    println!("== Hyprland");
    match sessions.as_deref().and_then(rules::active_session) {
        None => println!("  not available: no active session"),
        Some(s) => {
            let run_user = Path::new(hyprland::RUN_USER);
            let dirs = hyprland::instance_dirs(run_user, s.uid);
            if dirs.is_empty() {
                println!(
                    "  not available: no instance directory in {}/{}/hypr",
                    hyprland::RUN_USER,
                    s.uid
                );
            }
            let mut answer = None;
            for (mtime, dir) in dirs {
                let result = hyprland::probe_limited(dir.join(".socket.sock"));
                let secs = mtime
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs() as i64);
                println!(
                    "  {}  mtime {}  socket {}",
                    dir.display(),
                    format_timestamp(secs),
                    match &result {
                        Some(Some(_)) => "answers",
                        Some(None) => "does not answer",
                        None => "no answer within 1 s",
                    }
                );
                let monitors = result.flatten();
                if answer.is_none() {
                    answer = monitors;
                }
            }
            match answer {
                Some(monitors) => print!("{}", format_monitors(&monitors)),
                None => println!("  no socket answers: sessionstate Unlocked"),
            }
        }
    }

    println!("== DRM");
    print!(
        "{}",
        format_connectors(&drm::connectors(Path::new(drm::DRM_DIR)))
    );

    println!("== Input");
    let dir = Path::new(input::INPUT_DIR);
    match input::devices(dir) {
        Ok(devices) => {
            print!("{}", format_devices(&devices));
            println!("  Do not touch the gamepad for {} s", REST.as_secs());
            let counts = input::count_rest(&devices, REST).await;
            print!("{}", format_rest_counts(&counts));
        }
        Err(e) => println!("  not available: {}: {e}", dir.display()),
    }

    println!("== NVML");
    match nvml::check() {
        Ok((name, load, power)) => {
            println!("  device 0 {name:?}  utilization {load} %");
            match power {
                Ok(watts) => println!("  power {watts:.1} W"),
                Err(e) => println!("  power not available: {e}"),
            }
        }
        Err(e) => println!("  not available: {e}"),
    }

    println!("== Sleep inhibitor");
    match logind::check_inhibit().await {
        Ok(()) => println!("  the delay lock sleep:shutdown works (taken and released)"),
        Err(e) => println!("  not available: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::sources::linux::rules::{AxisInfo, InputCaps};

    fn session(id: &str, uid: u32, user: &str, class: &str, active: bool) -> Session {
        Session {
            id: id.to_string(),
            seat: "seat0".to_string(),
            uid,
            user: user.to_string(),
            class: class.to_string(),
            kind: "wayland".to_string(),
            remote: false,
            active,
        }
    }

    #[test]
    fn sessions_and_the_chosen_one() {
        let list = [
            session("c1", 967, "sddm", "greeter", false),
            session("2", 1000, "alice", "user", true),
        ];
        assert_eq!(
            format_sessions(&list),
            "  id c1  uid 967  name sddm  class greeter  type wayland  remote no  active no  seat seat0\n\
             \x20 id 2  uid 1000  name alice  class user  type wayland  remote no  active yes  seat seat0\n\
             \x20 chosen: session 2 (alice)\n"
        );
    }

    #[test]
    fn no_session_is_chosen_without_an_active_user_session() {
        assert_eq!(
            format_sessions(&[]),
            "  no sessions\n  chosen: none (sessionstate Unknown)\n"
        );
        let list = [session("c1", 967, "sddm", "greeter", true)];
        assert!(format_sessions(&list).ends_with("  chosen: none (sessionstate Unknown)\n"));
    }

    #[test]
    fn monitors_and_the_resulting_states() {
        let monitors = crate::sources::linux::rules::parse_monitors(
            r#"[{"name":"DP-1","disabled":false,"dpmsStatus":true,"solitaryBlockedBy":["LOCK","CANDIDATE"]},
                {"name":"DP-2","disabled":false,"dpmsStatus":false}]"#,
        )
        .unwrap();
        assert_eq!(
            format_monitors(&monitors),
            "  monitor DP-1  disabled no  dpmsStatus yes  solitaryBlockedBy [LOCK, CANDIDATE]\n\
             \x20 monitor DP-2  disabled no  dpmsStatus no  solitaryBlockedBy absent\n\
             \x20 sessionstate Locked  display ON\n"
        );
    }

    #[test]
    fn unlocked_and_off() {
        let monitors = crate::sources::linux::rules::parse_monitors(
            r#"[{"name":"DP-1","disabled":false,"dpmsStatus":false,"solitaryBlockedBy":null}]"#,
        )
        .unwrap();
        assert_eq!(
            format_monitors(&monitors),
            "  monitor DP-1  disabled no  dpmsStatus no  solitaryBlockedBy []\n\
             \x20 sessionstate Unlocked  display OFF\n"
        );
    }

    fn connector(name: &str, status: &str, dpms: &str) -> Connector {
        Connector {
            name: name.to_string(),
            status: status.to_string(),
            dpms: dpms.to_string(),
        }
    }

    #[test]
    fn connectors_and_the_display() {
        let list = [
            connector("card1-DP-1", "connected", "On"),
            connector("card1-HDMI-A-1", "disconnected", "Off"),
        ];
        assert_eq!(
            format_connectors(&list),
            "  card1-DP-1  status connected  dpms On\n\
             \x20 card1-HDMI-A-1  status disconnected  dpms Off\n\
             \x20 display ON\n"
        );
        assert_eq!(
            format_connectors(&[]),
            "  no connectors\n  display not known (no connected connector)\n"
        );
    }

    fn axis(min: i32, max: i32, flat: i32, fuzz: i32) -> AxisInfo {
        AxisInfo {
            min,
            max,
            flat,
            fuzz,
        }
    }

    #[test]
    fn devices_with_the_axis_roles_of_a_gamepad() {
        let pad = DeviceInfo {
            path: PathBuf::from("/dev/input/event7"),
            name: "Test Pad".to_string(),
            accelerometer: false,
            caps: InputCaps::new(
                true,
                [
                    (0x05, axis(0, 1023, 0, 3)),
                    (0x00, axis(-32768, 32767, 128, 16)),
                    (0x03, axis(-32768, 32767, 128, 16)),
                    (0x28, axis(0, 255, 0, 0)),
                ]
                .into_iter()
                .collect(),
            ),
        };
        let keyboard = DeviceInfo {
            path: PathBuf::from("/dev/input/event3"),
            name: "Test Keyboard".to_string(),
            accelerometer: false,
            caps: InputCaps::new(false, Default::default()),
        };
        let motion = DeviceInfo {
            path: PathBuf::from("/dev/input/event8"),
            name: "Test Pad Motion Sensors".to_string(),
            accelerometer: true,
            caps: InputCaps::new(
                false,
                [(0x00, axis(-32768, 32767, 0, 0))].into_iter().collect(),
            ),
        };
        assert_eq!(
            format_devices(&[keyboard, pad, motion]),
            "  /dev/input/event3  name \"Test Keyboard\"  accelerometer no  gamepad no\n\
             \x20 /dev/input/event7  name \"Test Pad\"  accelerometer no  gamepad yes\n\
             \x20   axis 0x00  min -32768  max 32767  flat 128  fuzz 16  role stick\n\
             \x20   axis 0x03  min -32768  max 32767  flat 128  fuzz 16  role stick\n\
             \x20   axis 0x05  min 0  max 1023  flat 0  fuzz 3  role trigger\n\
             \x20   axis 0x28  min 0  max 255  flat 0  fuzz 0  role ignored\n\
             \x20 /dev/input/event8  name \"Test Pad Motion Sensors\"  accelerometer yes  gamepad no  (skipped)\n"
        );
        assert_eq!(format_devices(&[]), "  no input devices\n");
    }

    #[test]
    fn the_rest_sampling_prints_only_counts() {
        let counts = [
            (
                "/dev/input/event3".to_string(),
                "Test Keyboard".to_string(),
                0,
            ),
            ("/dev/input/event7".to_string(), "Test Pad".to_string(), 12),
        ];
        assert_eq!(
            format_rest_counts(&counts),
            "  /dev/input/event3 \"Test Keyboard\": 0 counted events\n\
             \x20 /dev/input/event7 \"Test Pad\": 12 counted events\n"
        );
    }
}
