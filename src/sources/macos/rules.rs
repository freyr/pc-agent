//! The rules of the macOS sources, on plain Rust values. The IOKit reads are
//! in `iokit`.

use std::collections::HashMap;

use crate::core::{GpuSample, SessionState};

/// The `PerformanceStatistics` key of the GPU utilization.
pub const GPU_UTILIZATION_KEY: &str = "Device Utilization %";

/// One entry of the `IOConsoleUsers` list of the IO registry root.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConsoleSession {
    /// `kCGSSessionUserNameKey`.
    pub user: Option<String>,
    /// `kCGSSessionOnConsoleKey`.
    pub on_console: bool,
    /// `kCGSessionLoginDoneKey`.
    pub login_done: bool,
    /// `CGSSessionScreenIsLocked`; absent while unlocked.
    pub locked: Option<bool>,
}

/// The session state from the console list.
pub fn session_state(sessions: &[ConsoleSession]) -> SessionState {
    match sessions.iter().find(|s| s.on_console && s.login_done) {
        None => SessionState::NoSession,
        Some(s) if s.locked == Some(true) => SessionState::Locked,
        Some(_) => SessionState::Unlocked,
    }
}

/// The names of the users that completed the login.
pub fn logged_users(sessions: &[ConsoleSession]) -> Vec<String> {
    sessions
        .iter()
        .filter(|s| s.login_done)
        .filter_map(|s| s.user.clone())
        .collect()
}

/// The time of the last input from the HID idle time.
#[derive(Debug)]
pub struct InputTracker {
    last_idle_ns: Option<u64>,
    last_input: Option<i64>,
}

impl InputTracker {
    pub const fn new() -> Self {
        InputTracker {
            last_idle_ns: None,
            last_input: None,
        }
    }

    /// Takes the idle time read at `now_ms` (milliseconds since the epoch)
    /// and gives the last input time in seconds since the epoch.
    /// A new input is a fall of the idle time; the value changes only then
    /// (or at the first poll), so the rounding gives no 1 s jitter.
    pub fn update(&mut self, now_ms: i64, idle_ns: u64) -> i64 {
        let new_input = self.last_idle_ns.is_none_or(|prev| idle_ns < prev);
        self.last_idle_ns = Some(idle_ns);
        match self.last_input {
            Some(t) if !new_input => t,
            _ => {
                let idle_ms = i64::try_from(idle_ns / 1_000_000).unwrap_or(i64::MAX);
                let t = now_ms.saturating_sub(idle_ms).div_euclid(1000);
                self.last_input = Some(t);
                t
            }
        }
    }
}

/// The GPU sample from the integer values of `PerformanceStatistics`.
/// The load is the integer percent of `Device Utilization %`, in 0..=100.
pub fn gpu_sample(stats: &HashMap<String, i64>) -> Option<GpuSample> {
    stats.get(GPU_UTILIZATION_KEY).map(|&load| GpuSample {
        load: load.clamp(0, 100) as u32,
        power: None,
    })
}

/// `iokit_common_msg(x)` of IOMessage.h: `sys_iokit` is `err_system(0x38)`,
/// that is `0x38 << 26`; `sub_iokit_common` is 0.
const fn iokit_common_msg(message: u32) -> u32 {
    (0x38 << 26) | message
}

pub const MESSAGE_CAN_SYSTEM_SLEEP: u32 = iokit_common_msg(0x270);
pub const MESSAGE_SYSTEM_WILL_SLEEP: u32 = iokit_common_msg(0x280);
pub const MESSAGE_SYSTEM_HAS_POWERED_ON: u32 = iokit_common_msg(0x300);

/// What the power thread does for one message of `IORegisterForSystemPower`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PowerAction {
    /// Idle sleep request: allow it at once.
    AllowNow,
    /// The system goes to sleep: publish `Suspend` and `offline`, then allow.
    Suspend,
    /// The system woke up: publish `Resume` and `online`.
    Resume,
    Ignore,
}

pub fn power_action(message: u32) -> PowerAction {
    match message {
        MESSAGE_CAN_SYSTEM_SLEEP => PowerAction::AllowNow,
        MESSAGE_SYSTEM_WILL_SLEEP => PowerAction::Suspend,
        MESSAGE_SYSTEM_HAS_POWERED_ON => PowerAction::Resume,
        _ => PowerAction::Ignore,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The entry of the fixture `root-unlocked.plist`: on console, login done,
    /// no `CGSSessionScreenIsLocked`.
    fn on_console(user: &str) -> ConsoleSession {
        ConsoleSession {
            user: Some(user.to_string()),
            on_console: true,
            login_done: true,
            locked: None,
        }
    }

    #[test]
    fn one_unlocked_user() {
        let list = [on_console("alice")];
        assert_eq!(session_state(&list), SessionState::Unlocked);
        assert_eq!(logged_users(&list), ["alice"]);
    }

    #[test]
    fn screen_is_locked_gives_locked() {
        let list = [ConsoleSession {
            locked: Some(true),
            ..on_console("alice")
        }];
        assert_eq!(session_state(&list), SessionState::Locked);
        assert_eq!(logged_users(&list), ["alice"]);
    }

    #[test]
    fn screen_is_locked_no_gives_unlocked() {
        let list = [ConsoleSession {
            locked: Some(false),
            ..on_console("alice")
        }];
        assert_eq!(session_state(&list), SessionState::Unlocked);
    }

    #[test]
    fn an_empty_list_gives_no_session() {
        assert_eq!(session_state(&[]), SessionState::NoSession);
        assert!(logged_users(&[]).is_empty());
    }

    #[test]
    fn the_login_window_gives_no_session() {
        let list = [ConsoleSession {
            user: None,
            on_console: true,
            login_done: false,
            locked: None,
        }];
        assert_eq!(session_state(&list), SessionState::NoSession);
        assert!(logged_users(&list).is_empty());
    }

    #[test]
    fn two_users_one_switched_out() {
        let list = [
            ConsoleSession {
                on_console: false,
                ..on_console("bob")
            },
            on_console("alice"),
        ];
        assert_eq!(session_state(&list), SessionState::Unlocked);
        assert_eq!(logged_users(&list), ["bob", "alice"]);
    }

    #[test]
    fn a_locked_user_switched_out_does_not_lock_the_console_user() {
        let list = [
            ConsoleSession {
                on_console: false,
                locked: Some(true),
                ..on_console("bob")
            },
            on_console("alice"),
        ];
        assert_eq!(session_state(&list), SessionState::Unlocked);
    }

    #[test]
    fn only_switched_out_users_give_no_session() {
        let list = [ConsoleSession {
            on_console: false,
            ..on_console("bob")
        }];
        assert_eq!(session_state(&list), SessionState::NoSession);
        assert_eq!(logged_users(&list), ["bob"]);
    }

    /// 2001-09-09 01:46:40Z in ms.
    const T0_MS: i64 = 1_000_000_000_000;

    #[test]
    fn the_first_poll_gives_poll_time_minus_idle_time() {
        let mut t = InputTracker::new();
        // 75.76 s idle (the fixture `hidsystem.plist`).
        assert_eq!(t.update(T0_MS, 75_761_249_250), 999_999_924);
    }

    #[test]
    fn a_growing_idle_time_keeps_the_last_input() {
        let mut t = InputTracker::new();
        assert_eq!(t.update(T0_MS, 5_000_000_000), 999_999_995);
        // Poll time minus idle time would give 999_999_994 here: the value stays.
        assert_eq!(t.update(T0_MS + 2_000, 7_400_000_000), 999_999_995);
        assert_eq!(t.update(T0_MS + 4_000, 9_000_000_000), 999_999_995);
    }

    #[test]
    fn a_falling_idle_time_is_a_new_input() {
        let mut t = InputTracker::new();
        assert_eq!(t.update(T0_MS, 5_000_000_000), 999_999_995);
        assert_eq!(t.update(T0_MS + 4_000, 500_000_000), 1_000_000_003);
        // It grows again from the new input: no change.
        assert_eq!(t.update(T0_MS + 6_000, 2_600_000_000), 1_000_000_003);
    }

    fn stats(pairs: &[(&str, i64)]) -> HashMap<String, i64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn gpu_load_is_device_utilization() {
        // The keys of the fixture `accelerator.plist`.
        let s = stats(&[
            ("Device Utilization %", 27),
            ("Renderer Utilization %", 27),
            ("Tiler Utilization %", 27),
            ("recoveryCount", 0),
        ]);
        assert_eq!(
            gpu_sample(&s),
            Some(GpuSample {
                load: 27,
                power: None
            })
        );
    }

    #[test]
    fn gpu_without_the_key_gives_no_sample() {
        let s = stats(&[("Renderer Utilization %", 27), ("recoveryCount", 0)]);
        assert_eq!(gpu_sample(&s), None);
    }

    #[test]
    fn gpu_load_stays_in_0_to_100() {
        for (raw, load) in [(0, 0), (100, 100), (130, 100), (-1, 0)] {
            assert_eq!(
                gpu_sample(&stats(&[("Device Utilization %", raw)])).map(|g| g.load),
                Some(load),
                "{raw}"
            );
        }
    }

    #[test]
    fn power_messages_map_to_their_actions() {
        // The values of IOMessage.h: iokit_common_msg(x) = 0xe0000000 | x.
        assert_eq!(power_action(0xe000_0270), PowerAction::AllowNow);
        assert_eq!(power_action(0xe000_0280), PowerAction::Suspend);
        assert_eq!(power_action(0xe000_0300), PowerAction::Resume);
        // kIOMessageSystemWillPowerOn and kIOMessageSystemWillNotSleep.
        assert_eq!(power_action(0xe000_0320), PowerAction::Ignore);
        assert_eq!(power_action(0xe000_0290), PowerAction::Ignore);
    }
}
