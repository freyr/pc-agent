//! The output of `pc-agent diagnose` on Windows, formatted from plain
//! values, and the line format of `pc-agent session-helper --probe`, so that
//! the tests run on each OS. The Win32 reads are in `windows::diagnose` and
//! `windows::helper`.

use std::fmt::Write;

use super::wts_rules::{self, NO_CONSOLE, WtsSession};
use crate::core::SessionState;

/// The last line of the output: the CI waits for it.
pub const END_MARKER: &str = "== End of diagnose";

/// `ERROR_PRIVILEGE_NOT_HELD`: `WTSQueryUserToken` outside LocalSystem.
pub const ERROR_PRIVILEGE_NOT_HELD: u32 = 1314;

/// The console session, its fields and the `sessionstate` that the agent
/// gives it. `session` is `None` when the read failed.
pub fn format_console(console_id: u32, session: Option<&WtsSession>) -> String {
    let mut out = String::new();
    if console_id == NO_CONSOLE {
        out.push_str("  no console session\n");
    } else {
        match session {
            Some(s) => {
                let user = if s.user.is_empty() { "(none)" } else { &s.user };
                let _ = writeln!(
                    out,
                    "  console session {}  user {user}  state {} ({})  SessionFlags {} ({})",
                    s.id,
                    s.state,
                    connect_state_name(s.state),
                    s.flags,
                    match s.flags {
                        wts_rules::WTS_SESSIONSTATE_LOCK => "Locked",
                        wts_rules::WTS_SESSIONSTATE_UNLOCK => "Unlocked",
                        _ => "unknown",
                    }
                );
            }
            None => {
                let _ = writeln!(out, "  console session {console_id}: the read failed");
            }
        }
    }
    let state = match wts_rules::session_state(console_id, |_| session.cloned()) {
        Some(SessionState::Locked) => "Locked",
        Some(SessionState::Unlocked) => "Unlocked",
        Some(SessionState::NoSession) => "Unknown",
        Some(SessionState::NotKnown) | None => "not known (the last value stays)",
    };
    let _ = writeln!(out, "  sessionstate {state}");
    out
}

/// The name of a `WTS_CONNECTSTATE_CLASS` value.
fn connect_state_name(state: i32) -> &'static str {
    match state {
        0 => "Active",
        1 => "Connected",
        2 => "ConnectQuery",
        3 => "Shadow",
        4 => "Disconnected",
        5 => "Idle",
        6 => "Listen",
        7 => "Reset",
        8 => "Down",
        9 => "Init",
        _ => "unknown",
    }
}

/// The user names of `loggedusers`; `None` when the session list failed.
pub fn format_users(users: Option<&[String]>) -> String {
    match users {
        None => "  not available: the session list failed\n".to_string(),
        Some([]) => "  no logged-in users\n".to_string(),
        Some(list) => format!("  users {}\n", list.join(", ")),
    }
}

/// The name, the utilization in percent and the power in watts of GPU 0.
pub type NvmlDevice = (String, u32, Result<f64, String>);

/// The NVML check: the library that opened and the device 0 read, or the
/// error of each library.
pub fn format_nvml<S: AsRef<str>>(
    result: &Result<(S, Result<NvmlDevice, String>), String>,
) -> String {
    let (lib, device) = match result {
        Ok(found) => found,
        Err(e) => return format!("  not available: {e}\n"),
    };
    let mut out = format!("  loaded {}\n", lib.as_ref());
    match device {
        Ok((name, load, power)) => {
            let _ = writeln!(out, "  device 0 {name:?}  utilization {load} %");
            let _ = match power {
                Ok(watts) => writeln!(out, "  power {watts:.1} W"),
                Err(e) => writeln!(out, "  power not available: {e}"),
            };
        }
        Err(e) => {
            let _ = writeln!(out, "  device 0 not available: {e}");
        }
    }
    out
}

/// `HiberbootEnabled`, `HibernateEnabled` and whether Fast Startup is on.
pub fn format_fast_startup(hiberboot: Option<u32>, hibernate: Option<u32>) -> String {
    let value = |v: Option<u32>| v.map_or("not found".to_string(), |v| v.to_string());
    let state = if wts_rules::fast_startup_warning(hiberboot, hibernate).is_some() {
        "on: lastboot is wrong and no SystemShutdown event is sent \
         (turn it off with: powercfg /h off)"
    } else {
        "off"
    };
    format!(
        "  HiberbootEnabled {}  HibernateEnabled {}\n  Fast Startup {state}\n",
        value(hiberboot),
        value(hibernate)
    )
}

/// The text of `powercfg /a`, indented, or the error of the run.
pub fn format_powercfg(result: &Result<String, String>) -> String {
    match result {
        Ok(text) => text
            .lines()
            .map(str::trim_end)
            .filter(|line| !line.is_empty())
            .map(|line| format!("  {line}\n"))
            .collect(),
        Err(e) => format!("  not available: {e}\n"),
    }
}

/// `VerifiedAndReputablePolicyState` and its meaning.
pub fn format_smart_app_control(value: Option<u32>) -> String {
    let Some(value) = value else {
        return "  not found\n".to_string();
    };
    let meaning = match value {
        0 => "off",
        1 => "on",
        2 => "evaluation",
        _ => "unknown value",
    };
    format!("  VerifiedAndReputablePolicyState {value} ({meaning})\n")
}

/// What the probe reads once in the user session.
#[derive(Clone, Debug, PartialEq)]
pub struct Probe {
    /// The age of the last input (`GetLastInputInfo`) in milliseconds.
    pub last_input_ms: u32,
    /// `dwPacketNumber` of each XInput slot 0-3; `None` when not connected.
    pub slots: [Option<u32>; 4],
}

/// The age of a `GetTickCount` time `last` at the tick `now`. The 32-bit
/// tick wraps after 49.7 days.
pub fn tick_age(now: u32, last: u32) -> u32 {
    now.wrapping_sub(last)
}

/// The stdout of the probe.
pub fn format_probe(probe: &Probe) -> String {
    let mut out = format!("last-input-ms {}\n", probe.last_input_ms);
    for (slot, packet) in probe.slots.iter().enumerate() {
        let _ = match packet {
            Some(packet) => writeln!(out, "xinput {slot} connected {packet}"),
            None => writeln!(out, "xinput {slot} none"),
        };
    }
    out
}

/// The probe from its stdout.
pub fn parse_probe(text: &str) -> Result<Probe, String> {
    let bad = || format!("unexpected probe output {text:?}");
    let mut lines = text.lines().map(str::trim_end);
    let last_input_ms = lines
        .next()
        .and_then(|l| l.strip_prefix("last-input-ms "))
        .and_then(|n| n.parse().ok())
        .ok_or_else(bad)?;
    let mut slots = [None; 4];
    for (slot, value) in slots.iter_mut().enumerate() {
        let rest = lines
            .next()
            .and_then(|l| l.strip_prefix(&format!("xinput {slot} ")))
            .ok_or_else(bad)?;
        *value = match rest {
            "none" => None,
            _ => Some(
                rest.strip_prefix("connected ")
                    .and_then(|n| n.parse().ok())
                    .ok_or_else(bad)?,
            ),
        };
    }
    Ok(Probe {
        last_input_ms,
        slots,
    })
}

/// The Win32 error code in an HRESULT of facility 7 (`0x8007xxxx`); any
/// other HRESULT as it is.
pub fn win32_code(hresult: i32) -> u32 {
    let code = hresult as u32;
    if code & 0xFFFF_0000 == 0x8007_0000 {
        code & 0xFFFF
    } else {
        code
    }
}

/// The error of a failed step of the helper start.
pub fn step_error(step: &str, code: u32) -> String {
    if code == ERROR_PRIVILEGE_NOT_HELD {
        format!("{step} failed: {code} (not running as SYSTEM)")
    } else {
        format!("{step} failed: {code}")
    }
}

/// The helper line: the session and the probe, or the failure.
pub fn format_helper(result: &Result<(u32, Probe), String>) -> String {
    match result {
        Ok((session, probe)) => {
            let slots: Vec<String> = probe
                .slots
                .iter()
                .enumerate()
                .map(|(slot, packet)| match packet {
                    Some(packet) => format!("slot{slot} connected (packet {packet})"),
                    None => format!("slot{slot} not connected"),
                })
                .collect();
            format!(
                "  helper: started in session {session}, last input {} s ago, XInput: {}\n",
                probe.last_input_ms / 1000,
                slots.join(", ")
            )
        }
        Err(e) => format!("  helper: {e}\n"),
    }
}

/// The result of the MQTT check. It holds no password.
#[derive(Clone, Debug, PartialEq)]
pub enum MqttOutcome {
    /// The configuration file does not exist.
    NoConfig(String),
    /// The configuration file cannot be read or parsed.
    BadConfig(String),
    Connected {
        host: String,
        port: u16,
        client_id: String,
    },
    Failed {
        host: String,
        port: u16,
        error: String,
    },
}

/// The MQTT line.
pub fn format_mqtt(outcome: &MqttOutcome) -> String {
    match outcome {
        MqttOutcome::NoConfig(path) => format!("  MQTT: no configuration ({path} not found)\n"),
        MqttOutcome::BadConfig(e) => format!("  MQTT: invalid configuration: {e}\n"),
        MqttOutcome::Connected {
            host,
            port,
            client_id,
        } => format!("  MQTT: connected to {host}:{port} as {client_id}, then disconnected\n"),
        MqttOutcome::Failed { host, port, error } => {
            format!("  MQTT: cannot connect to {host}:{port}: {error}\n")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: u32, state: i32, flags: i32, user: &str) -> WtsSession {
        WtsSession {
            id,
            state,
            flags,
            user: user.to_string(),
        }
    }

    #[test]
    fn the_console_session_with_its_flags_and_the_sessionstate() {
        let s = session(1, 0, 1, "runneradmin");
        assert_eq!(
            format_console(1, Some(&s)),
            "  console session 1  user runneradmin  state 0 (Active)  SessionFlags 1 (Unlocked)\n\
             \x20 sessionstate Unlocked\n"
        );
        let s = session(2, 0, 0, "jahu");
        assert_eq!(
            format_console(2, Some(&s)),
            "  console session 2  user jahu  state 0 (Active)  SessionFlags 0 (Locked)\n\
             \x20 sessionstate Locked\n"
        );
        let s = session(1, 1, -1, "");
        assert_eq!(
            format_console(1, Some(&s)),
            "  console session 1  user (none)  state 1 (Connected)  SessionFlags -1 (unknown)\n\
             \x20 sessionstate Unknown\n"
        );
        let s = session(3, 0, -1, "jahu");
        assert!(
            format_console(3, Some(&s))
                .ends_with("  sessionstate not known (the last value stays)\n")
        );
    }

    #[test]
    fn no_console_session_or_a_failed_read() {
        assert_eq!(
            format_console(NO_CONSOLE, None),
            "  no console session\n  sessionstate Unknown\n"
        );
        assert_eq!(
            format_console(4, None),
            "  console session 4: the read failed\n  sessionstate not known (the last value stays)\n"
        );
    }

    #[test]
    fn the_logged_in_users() {
        let users = ["runneradmin".to_string(), "jahu".to_string()];
        assert_eq!(format_users(Some(&users)), "  users runneradmin, jahu\n");
        assert_eq!(format_users(Some(&[])), "  no logged-in users\n");
        assert_eq!(
            format_users(None),
            "  not available: the session list failed\n"
        );
    }

    #[test]
    fn nvml_names_the_library_that_opened_or_each_failure() {
        let ok: NvmlDevice = ("RTX 4090".to_string(), 37, Ok(85.25));
        assert_eq!(
            format_nvml(&Ok((r"C:\Windows\System32\nvml.dll", Ok(ok)))),
            "  loaded C:\\Windows\\System32\\nvml.dll\n\
             \x20 device 0 \"RTX 4090\"  utilization 37 %\n\
             \x20 power 85.2 W\n"
        );
        let no_power: NvmlDevice = ("RTX 4090".to_string(), 5, Err("Not Supported".to_string()));
        assert!(
            format_nvml(&Ok(("nvml.dll", Ok(no_power))))
                .ends_with("  power not available: Not Supported\n")
        );
        assert_eq!(
            format_nvml(&Ok(("nvml.dll", Err("GPU is lost".to_string())))),
            "  loaded nvml.dll\n  device 0 not available: GPU is lost\n"
        );
        assert_eq!(
            format_nvml::<&str>(&Err("nvml.dll: a; x.dll: b".to_string())),
            "  not available: nvml.dll: a; x.dll: b\n"
        );
    }

    #[test]
    fn fast_startup_is_on_only_with_hiberboot_and_hibernation() {
        assert_eq!(
            format_fast_startup(Some(1), Some(1)),
            "  HiberbootEnabled 1  HibernateEnabled 1\n  Fast Startup on: lastboot is wrong and \
             no SystemShutdown event is sent (turn it off with: powercfg /h off)\n"
        );
        assert_eq!(
            format_fast_startup(Some(1), Some(0)),
            "  HiberbootEnabled 1  HibernateEnabled 0\n  Fast Startup off\n"
        );
        assert_eq!(
            format_fast_startup(None, None),
            "  HiberbootEnabled not found  HibernateEnabled not found\n  Fast Startup off\n"
        );
    }

    #[test]
    fn the_powercfg_text_is_indented() {
        let text =
            "The following sleep states are available on this system:\r\n    Standby (S3)\r\n\r\n";
        assert_eq!(
            format_powercfg(&Ok(text.to_string())),
            "  The following sleep states are available on this system:\n      Standby (S3)\n"
        );
        assert_eq!(
            format_powercfg(&Err("not found".to_string())),
            "  not available: not found\n"
        );
    }

    #[test]
    fn the_smart_app_control_value_and_its_meaning() {
        let line = |v| format_smart_app_control(Some(v));
        assert_eq!(line(0), "  VerifiedAndReputablePolicyState 0 (off)\n");
        assert_eq!(line(1), "  VerifiedAndReputablePolicyState 1 (on)\n");
        assert_eq!(
            line(2),
            "  VerifiedAndReputablePolicyState 2 (evaluation)\n"
        );
        assert_eq!(
            line(7),
            "  VerifiedAndReputablePolicyState 7 (unknown value)\n"
        );
        assert_eq!(format_smart_app_control(None), "  not found\n");
    }

    #[test]
    fn the_tick_age_wraps_with_the_32_bit_tick() {
        assert_eq!(tick_age(5_000, 2_000), 3_000);
        assert_eq!(tick_age(1_000, u32::MAX - 999), 2_000);
        assert_eq!(tick_age(7, 7), 0);
    }

    #[test]
    fn the_probe_output_parses_back() {
        let probe = Probe {
            last_input_ms: 3_021,
            slots: [Some(17), None, Some(0), None],
        };
        let text = format_probe(&probe);
        assert_eq!(
            text,
            "last-input-ms 3021\nxinput 0 connected 17\nxinput 1 none\nxinput 2 connected 0\nxinput 3 none\n"
        );
        assert_eq!(parse_probe(&text), Ok(probe));
        // Windows line ends are accepted.
        assert_eq!(
            parse_probe(&text.replace('\n', "\r\n")).map(|p| p.last_input_ms),
            Ok(3_021)
        );
    }

    #[test]
    fn a_bad_probe_output_is_an_error() {
        assert!(parse_probe("").is_err());
        assert!(parse_probe("last-input-ms x\n").is_err());
        // A missing slot.
        assert!(parse_probe("last-input-ms 1\nxinput 0 none\n").is_err());
        assert!(parse_probe("last-input-ms 1\nxinput 9 none\n").is_err());
        let err = parse_probe("hello").unwrap_err();
        assert!(err.contains("hello"), "{err}");
    }

    #[test]
    fn the_win32_code_of_an_hresult() {
        // HRESULT_FROM_WIN32(1314)
        assert_eq!(win32_code(0x8007_0522_u32 as i32), 1314);
        // E_FAIL is not a Win32 code.
        assert_eq!(win32_code(0x8000_4005_u32 as i32), 0x8000_4005);
    }

    #[test]
    fn a_failed_step_names_the_step_and_the_code() {
        assert_eq!(
            step_error("WTSQueryUserToken", ERROR_PRIVILEGE_NOT_HELD),
            "WTSQueryUserToken failed: 1314 (not running as SYSTEM)"
        );
        assert_eq!(
            step_error("CreateProcessAsUserW", 5),
            "CreateProcessAsUserW failed: 5"
        );
    }

    #[test]
    fn the_helper_line() {
        let probe = Probe {
            last_input_ms: 3_999,
            slots: [Some(17), None, None, None],
        };
        assert_eq!(
            format_helper(&Ok((1, probe))),
            "  helper: started in session 1, last input 3 s ago, XInput: slot0 connected \
             (packet 17), slot1 not connected, slot2 not connected, slot3 not connected\n"
        );
        assert_eq!(
            format_helper(&Err(step_error("WTSQueryUserToken", 1314))),
            "  helper: WTSQueryUserToken failed: 1314 (not running as SYSTEM)\n"
        );
    }

    #[test]
    fn the_end_marker_is_the_line_that_the_ci_waits_for() {
        // .github/workflows/ci.yml polls the output for this line.
        assert_eq!(END_MARKER, "== End of diagnose");
    }

    #[test]
    fn the_mqtt_line() {
        assert_eq!(
            format_mqtt(&MqttOutcome::NoConfig(
                r"C:\ProgramData\pc-agent\config.toml".into()
            )),
            "  MQTT: no configuration (C:\\ProgramData\\pc-agent\\config.toml not found)\n"
        );
        assert_eq!(
            format_mqtt(&MqttOutcome::BadConfig("invalid config file c.toml".into())),
            "  MQTT: invalid configuration: invalid config file c.toml\n"
        );
        assert_eq!(
            format_mqtt(&MqttOutcome::Connected {
                host: "127.0.0.1".into(),
                port: 1883,
                client_id: "pc-agent-diagnose-CI-TEST".into(),
            }),
            "  MQTT: connected to 127.0.0.1:1883 as pc-agent-diagnose-CI-TEST, then disconnected\n"
        );
        assert_eq!(
            format_mqtt(&MqttOutcome::Failed {
                host: "127.0.0.1".into(),
                port: 1883,
                error: "no ConnAck within 5 s".into(),
            }),
            "  MQTT: cannot connect to 127.0.0.1:1883: no ConnAck within 5 s\n"
        );
    }
}
