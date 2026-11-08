//! The rules of the session helper and of its pipe, on plain Rust values, so
//! that the tests run on each OS: the message lines and their parser, the
//! clamp of an input time, the restart back-off, the XInput change rule, the
//! input-time merge and the send rule of the helper, the pipe DACL and the
//! client check, and the input state of the service with the
//! `GUID_GLOBAL_USER_PRESENCE` fallback. The Win32 calls are in
//! `windows::helper`, `windows::pipe` and `windows::supervisor`.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde::Deserialize;

use super::wts_rules::{WTS_ACTIVE, WtsSession};

/// The pipe of the service and its helpers.
pub const PIPE_NAME: &str = r"\\.\pipe\pc-agent";

/// The longest line that the service reads from a helper, without the newline.
pub const MAX_LINE: usize = 64 * 1024;

/// `FILE_GENERIC_READ` and `FILE_GENERIC_WRITE` of `winnt.h`.
const FILE_GENERIC_READ: u32 = 0x0012_0089;
const FILE_GENERIC_WRITE: u32 = 0x0012_0116;
/// `FILE_CREATE_PIPE_INSTANCE` (= `FILE_APPEND_DATA`), part of
/// `FILE_GENERIC_WRITE`. A client with it could create a second instance of
/// the pipe and catch the connection of a helper.
const FILE_CREATE_PIPE_INSTANCE: u32 = 0x0004;
/// `FILE_WRITE_DATA`.
const FILE_WRITE_DATA: u32 = 0x0002;

/// The rights of `INTERACTIVE` on the pipe: read and write, without the
/// right to create a pipe instance.
pub const PIPE_CLIENT_RIGHTS: u32 =
    (FILE_GENERIC_READ | FILE_GENERIC_WRITE) & !FILE_CREATE_PIPE_INSTANCE;

/// The access that the helper asks for when it opens the pipe: a subset of
/// `PIPE_CLIENT_RIGHTS` (`GENERIC_WRITE` would ask for the create right too).
pub const HELPER_ACCESS: u32 = FILE_GENERIC_READ | FILE_WRITE_DATA;

/// The DACL of the pipe: SYSTEM full access, the `INTERACTIVE` group read and write.
pub const PIPE_SDDL: &str = "D:P(A;;FA;;;SY)(A;;0x12019b;;;IU)";

/// The pipe DACL; `extra_sid` (the CI test only) gets the rights of
/// `INTERACTIVE` too.
pub fn pipe_sddl(extra_sid: Option<&str>) -> String {
    match extra_sid {
        None => PIPE_SDDL.to_string(),
        Some(sid) => format!("{PIPE_SDDL}(A;;{PIPE_CLIENT_RIGHTS:#x};;;{sid})"),
    }
}

/// One message of a helper to the service. The service trusts helper data
/// only for these fields. A new message is a new variant.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HelperMessage {
    /// `{"last_input": <unix s>}`: the time of the last input in the session.
    LastInput(i64),
}

impl HelperMessage {
    /// The line of the message, with the newline.
    pub fn to_line(&self) -> String {
        match self {
            HelperMessage::LastInput(t) => format!("{{\"last_input\":{t}}}\n"),
        }
    }
}

/// Why a line of a helper was dropped.
#[derive(Clone, Debug, PartialEq)]
pub enum LineError {
    /// The line is longer than `MAX_LINE`.
    TooLong,
    /// Not one known message: bad JSON, an unknown field or a wrong type.
    Invalid(String),
}

/// The message of one line (without the newline).
pub fn parse_line(line: &[u8]) -> Result<HelperMessage, LineError> {
    if line.len() > MAX_LINE {
        return Err(LineError::TooLong);
    }
    // An externally tagged enum: serde_json rejects an object with a second
    // key, so an unknown field drops the whole line.
    serde_json::from_slice(line).map_err(|e| LineError::Invalid(e.to_string()))
}

/// Splits the bytes of the pipe into lines. It keeps at most `MAX_LINE`
/// bytes: a longer line is dropped up to its newline.
#[derive(Debug, Default)]
pub struct LineFramer {
    buf: Vec<u8>,
    /// The current line is too long: skip its bytes up to the newline.
    dropping: bool,
}

impl LineFramer {
    /// The complete lines of `bytes` (with the bytes kept from before), each
    /// parsed; a line above `MAX_LINE` gives `LineError::TooLong` once.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Result<HelperMessage, LineError>> {
        let mut out = Vec::new();
        let mut pieces = bytes.split(|&b| b == b'\n').peekable();
        while let Some(piece) = pieces.next() {
            let complete = pieces.peek().is_some();
            if !self.dropping {
                self.buf.extend_from_slice(piece);
                if self.buf.len() > MAX_LINE {
                    out.push(Err(LineError::TooLong));
                    self.buf.clear();
                    self.dropping = true;
                }
            }
            if complete {
                if !self.dropping {
                    out.push(parse_line(&self.buf));
                }
                self.buf.clear();
                self.dropping = false;
            }
        }
        out
    }
}

/// An input time of a helper, clamped to the range from the last value to
/// `now` (all in unix seconds).
pub fn clamp_input(t: i64, last: Option<i64>, now: i64) -> i64 {
    let t = t.min(now);
    // The last value wins over `now`: the input time never goes back.
    last.map_or(t, |last| t.max(last))
}

/// The restart delays of a helper: 5 s, 30 s, then 5 min.
const BACKOFF: [Duration; 3] = [
    Duration::from_secs(5),
    Duration::from_secs(30),
    Duration::from_secs(300),
];

/// The back-off of the restarts of the helper of one session.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Backoff {
    /// The restarts since the last reset.
    restarts: usize,
}

impl Backoff {
    /// The delay of the next restart.
    pub fn next(&mut self) -> Duration {
        let delay = BACKOFF[self.restarts.min(BACKOFF.len() - 1)];
        self.restarts += 1;
        delay
    }
}

/// The helper of one session.
#[derive(Clone, Debug, Default, PartialEq)]
struct Slot {
    /// The process ID and the start time of the running helper.
    running: Option<(u32, Instant)>,
    /// When the next start is due, while no helper runs.
    retry_at: Option<Instant>,
    backoff: Backoff,
}

/// The helpers of the sessions: which sessions need a start now, and which
/// process the service started for which session.
#[derive(Debug, Default)]
pub struct Supervisor {
    slots: BTreeMap<u32, Slot>,
}

impl Supervisor {
    pub const fn new() -> Self {
        Supervisor {
            slots: BTreeMap::new(),
        }
    }

    /// The sessions of `active` where a helper must start now: no helper runs
    /// and the back-off time has passed. A session that is not active and
    /// has no running helper loses its slot, so a new logon starts at once.
    pub fn due(&mut self, active: &[u32], now: Instant) -> Vec<u32> {
        self.slots
            .retain(|session, slot| slot.running.is_some() || active.contains(session));
        active
            .iter()
            .copied()
            .filter(|&session| {
                let slot = self.slots.entry(session).or_default();
                slot.running.is_none() && slot.retry_at.is_none_or(|at| now >= at)
            })
            .collect()
    }

    /// The helper of `session` started as the process `pid`.
    pub fn started(&mut self, session: u32, pid: u32, now: Instant) {
        let slot = self.slots.entry(session).or_default();
        slot.running = Some((pid, now));
        slot.retry_at = None;
    }

    /// The start in `session` failed; gives the delay of the next start.
    pub fn start_failed(&mut self, session: u32, now: Instant) -> Duration {
        let slot = self.slots.entry(session).or_default();
        let delay = slot.backoff.next();
        slot.retry_at = Some(now + delay);
        delay
    }

    /// The process `pid` exited; gives its session and the delay of the next
    /// start. A helper that ran for the longest delay or more resets the back-off.
    pub fn exited(&mut self, pid: u32, now: Instant) -> Option<(u32, Duration)> {
        let (&session, slot) = self
            .slots
            .iter_mut()
            .find(|(_, slot)| slot.running.is_some_and(|(p, _)| p == pid))?;
        let (_, since) = slot.running.take()?;
        if now.duration_since(since) >= BACKOFF[BACKOFF.len() - 1] {
            slot.backoff = Backoff::default();
        }
        let delay = slot.backoff.next();
        slot.retry_at = Some(now + delay);
        Some((session, delay))
    }

    /// The session that the service started the running process `pid` for.
    pub fn session_of(&self, pid: u32) -> Option<u32> {
        self.slots
            .iter()
            .find(|(_, slot)| slot.running.is_some_and(|(p, _)| p == pid))
            .map(|(&session, _)| session)
    }
}

/// The sessions that need a helper: `WTSActive`, with a user, not session 0.
pub fn helper_sessions(sessions: &[WtsSession]) -> Vec<u32> {
    sessions
        .iter()
        .filter(|s| s.id != 0 && !s.user.is_empty() && s.state == WTS_ACTIVE)
        .map(|s| s.id)
        .collect()
}

/// The client check: the session of a client process that the service
/// started (`started_for`) and that runs in that session (`runs_in`).
pub fn accept_client(started_for: Option<u32>, runs_in: Option<u32>) -> Option<u32> {
    started_for.filter(|&session| runs_in == Some(session))
}

/// The fields of `XINPUT_GAMEPAD` that the change rule reads.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Pad {
    pub buttons: u16,
    pub left_trigger: u8,
    pub right_trigger: u8,
    pub thumb_lx: i16,
    pub thumb_ly: i16,
    pub thumb_rx: i16,
    pub thumb_ry: i16,
}

/// The XInput change rule: `true` when the state `pad` with the packet
/// number `packet` counts as input after the last read `prev` (packet and
/// state) of the slot. The first read of a slot only records the state.
pub fn gamepad_activity(prev: Option<(u32, &Pad)>, packet: u32, pad: &Pad) -> bool {
    let Some((prev_packet, prev)) = prev else {
        return false;
    };
    if packet == prev_packet {
        return false;
    }
    let sticks = [pad.thumb_lx, pad.thumb_ly, pad.thumb_rx, pad.thumb_ry];
    // A press counts, a release does not (`EV_KEY` value 1 on Linux); the
    // D-pad is buttons in XInput.
    pad.buttons & !prev.buttons != 0
        || sticks.into_iter().any(stick_counts)
        || [pad.left_trigger, pad.right_trigger]
            .into_iter()
            .any(trigger_counts)
}

/// The stick rule of Linux with no `flat` (XInput has none): farther from
/// the center than 10 % of the range. In tenths and doubled, as in
/// `linux::rules::is_activity`.
fn stick_counts(value: i16) -> bool {
    let (v, min, max) = (i64::from(value), i64::from(i16::MIN), i64::from(i16::MAX));
    10 * (2 * v - (min + max)).abs() > 2 * (max - min)
}

/// The trigger rule of Linux: above the minimum plus 10 % of the range (0-255).
fn trigger_counts(value: u8) -> bool {
    10 * i64::from(value) > i64::from(u8::MAX)
}

/// The time of the last keyboard or mouse input in unix seconds: `now_ms`
/// (unix ms) minus the age of `GetLastInputInfo` in ms.
pub fn keyboard_input(now_ms: i64, age_ms: u32) -> i64 {
    (now_ms - i64::from(age_ms)).div_euclid(1000)
}

/// The input time of the helper: the newest of the keyboard and mouse time
/// and the gamepad time.
pub fn merge_input(keyboard: Option<i64>, gamepad: Option<i64>) -> Option<i64> {
    keyboard.max(gamepad)
}

/// The minimum interval between two messages of the helper.
const SEND_INTERVAL_MS: i64 = 2_000;

/// The send rule of the helper: the first value at once, then a change at
/// most once each 2 s.
#[derive(Debug, Default)]
pub struct SendRule {
    /// The last sent value and when it was sent (unix ms).
    sent: Option<(i64, i64)>,
}

impl SendRule {
    /// `true` when `value` must be sent now; it then counts as sent.
    pub fn due(&mut self, value: i64, now_ms: i64) -> bool {
        let due = self
            .sent
            .is_none_or(|(sent, at)| sent != value && now_ms - at >= SEND_INTERVAL_MS);
        if due {
            self.sent = Some((value, now_ms));
        }
        due
    }
}

/// The input state of the service: the last input time and the helpers
/// that are connected, by session.
#[derive(Debug, Default)]
pub struct InputState {
    last: Option<i64>,
    /// The connections of each session (a restart can overlap the old one).
    connected: BTreeMap<u32, usize>,
    /// The last `GUID_GLOBAL_USER_PRESENCE` value: `true` for present.
    present: Option<bool>,
}

impl InputState {
    pub const fn new() -> Self {
        InputState {
            last: None,
            connected: BTreeMap::new(),
            present: None,
        }
    }

    pub fn connected(&mut self, session: u32) {
        *self.connected.entry(session).or_insert(0) += 1;
    }

    pub fn disconnected(&mut self, session: u32) {
        if let Some(count) = self.connected.get_mut(&session) {
            *count -= 1;
            if *count == 0 {
                self.connected.remove(&session);
            }
        }
    }

    /// `true` when a helper of `session` is connected.
    pub fn helper(&self, session: u32) -> bool {
        self.connected.contains_key(&session)
    }

    /// An input time of the helper of `session`. Only the console session
    /// counts; the time is clamped to the range from the last value to `now`.
    pub fn helper_input(&mut self, session: u32, t: i64, console: u32, now: i64) {
        if session == console {
            self.last = Some(clamp_input(t, self.last, now));
        }
    }

    /// A `GUID_GLOBAL_USER_PRESENCE` value. A change to present sets the last
    /// input to `now` when no helper of the console session is connected.
    /// The first value (sent at the registration) is no change.
    pub fn presence(&mut self, present: bool, console: u32, now: i64) {
        let change = present && self.present == Some(false);
        self.present = Some(present);
        if change && !self.helper(console) {
            self.last = Some(clamp_input(now, self.last, now));
        }
    }

    /// The last input time.
    pub fn last_input(&self) -> Option<i64> {
        self.last
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Message parser.

    #[test]
    fn a_valid_line_gives_last_input() {
        assert_eq!(
            parse_line(br#"{"last_input": 1787892955}"#),
            Ok(HelperMessage::LastInput(1_787_892_955))
        );
        assert_eq!(
            HelperMessage::LastInput(1_787_892_955).to_line(),
            "{\"last_input\":1787892955}\n"
        );
        let line = HelperMessage::LastInput(42).to_line();
        assert_eq!(
            parse_line(line.trim_end().as_bytes()),
            Ok(HelperMessage::LastInput(42))
        );
    }

    #[test]
    fn an_unknown_field_drops_the_line() {
        for line in [
            &br#"{"last_input": 1, "user": "jahu"}"#[..],
            br#"{"user": "jahu"}"#,
            br#"{"lock": true}"#,
            br#"{"last_input": "1"}"#,
            br#"{"last_input": 1.5}"#,
            br#"{}"#,
            b"last_input 1",
            b"",
        ] {
            assert!(
                matches!(parse_line(line), Err(LineError::Invalid(_))),
                "{:?}",
                String::from_utf8_lossy(line)
            );
        }
    }

    #[test]
    fn a_line_above_64_kib_is_dropped_and_the_next_line_is_read() {
        let mut framer = LineFramer::default();
        let pad = " ".repeat(MAX_LINE);
        // Above the limit, in pieces: one TooLong, then the next line works.
        let long = format!("{{\"last_input\": 1{pad}}}\n");
        let mut out = Vec::new();
        for piece in long.as_bytes().chunks(4096) {
            out.extend(framer.push(piece));
        }
        out.extend(framer.push(b"{\"last_input\": 2}\n"));
        assert_eq!(
            out,
            [Err(LineError::TooLong), Ok(HelperMessage::LastInput(2))]
        );
        // The framer never keeps more than the limit of one line.
        framer.push(&vec![b'x'; 3 * MAX_LINE]);
        assert!(framer.buf.len() <= MAX_LINE, "{}", framer.buf.len());
    }

    #[test]
    fn a_line_of_exactly_64_kib_is_read() {
        let mut framer = LineFramer::default();
        let head = "{\"last_input\": 3";
        let line = format!("{head}{}}}", " ".repeat(MAX_LINE - head.len() - 1));
        assert_eq!(line.len(), MAX_LINE);
        assert_eq!(
            framer.push(format!("{line}\n").as_bytes()),
            [Ok(HelperMessage::LastInput(3))]
        );
    }

    #[test]
    fn lines_split_across_reads_and_two_lines_in_one_read() {
        let mut framer = LineFramer::default();
        assert_eq!(framer.push(b"{\"last_in"), []);
        assert_eq!(
            framer.push(b"put\": 5}\n{\"last_input\": 6}\n{\"last"),
            [
                Ok(HelperMessage::LastInput(5)),
                Ok(HelperMessage::LastInput(6))
            ]
        );
        assert_eq!(
            framer.push(b"_input\": 7}\n"),
            [Ok(HelperMessage::LastInput(7))]
        );
    }

    // Clamp.

    #[test]
    fn an_input_time_in_the_future_is_clamped_to_now() {
        assert_eq!(clamp_input(2_000, Some(900), 1_000), 1_000);
        assert_eq!(clamp_input(i64::MAX, None, 1_000), 1_000);
    }

    #[test]
    fn an_input_time_before_the_last_value_is_clamped_to_the_last_value() {
        assert_eq!(clamp_input(800, Some(900), 1_000), 900);
        assert_eq!(clamp_input(i64::MIN, Some(900), 1_000), 900);
        // In the range: as it is.
        assert_eq!(clamp_input(950, Some(900), 1_000), 950);
        assert_eq!(clamp_input(500, None, 1_000), 500);
    }

    // Back-off and the supervisor.

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn the_backoff_is_5_s_30_s_then_5_min() {
        let mut b = Backoff::default();
        let delays: Vec<Duration> = (0..5).map(|_| b.next()).collect();
        assert_eq!(delays, [secs(5), secs(30), secs(300), secs(300), secs(300)]);
    }

    #[test]
    fn the_helper_starts_at_once_in_each_active_session() {
        let t0 = Instant::now();
        let mut s = Supervisor::new();
        assert_eq!(s.due(&[1, 2], t0), [1, 2]);
        s.started(1, 100, t0);
        s.started(2, 200, t0);
        assert_eq!(s.due(&[1, 2], t0 + secs(2)), Vec::<u32>::new());
        assert_eq!(s.session_of(100), Some(1));
        assert_eq!(s.session_of(200), Some(2));
        assert_eq!(s.session_of(300), None);
    }

    #[test]
    fn after_an_exit_the_restart_waits_5_s_30_s_then_5_min() {
        let t0 = Instant::now();
        let mut s = Supervisor::new();
        s.due(&[1], t0);
        s.started(1, 100, t0);
        let mut now = t0 + secs(1);
        let mut pid = 100;
        for want in [secs(5), secs(30), secs(300), secs(300)] {
            assert_eq!(s.exited(pid, now), Some((1, want)));
            assert_eq!(s.session_of(pid), None, "an exited helper is no client");
            assert!(s.due(&[1], now + want - secs(1)).is_empty());
            now += want;
            assert_eq!(s.due(&[1], now), [1]);
            pid += 1;
            s.started(1, pid, now);
            now += secs(1);
        }
        // A helper that ran for 5 min resets the back-off.
        assert_eq!(s.exited(pid, now + secs(300)), Some((1, secs(5))));
        assert_eq!(s.exited(999, now), None);
    }

    #[test]
    fn after_a_failed_start_the_restart_waits_5_s_30_s_then_5_min() {
        let t0 = Instant::now();
        let mut s = Supervisor::new();
        let mut now = t0;
        for want in [secs(5), secs(30), secs(300), secs(300)] {
            assert_eq!(s.due(&[1], now), [1]);
            assert_eq!(s.start_failed(1, now), want);
            assert!(s.due(&[1], now + want - secs(1)).is_empty());
            now += want;
        }
        // The start works at last: the next exit after a short run goes on
        // with the back-off of the session.
        assert_eq!(s.due(&[1], now), [1]);
        s.started(1, 100, now);
        assert_eq!(s.exited(100, now + secs(1)), Some((1, secs(300))));
    }

    #[test]
    fn a_session_that_ends_loses_its_backoff_and_a_new_logon_starts_at_once() {
        let t0 = Instant::now();
        let mut s = Supervisor::new();
        s.due(&[1], t0);
        s.start_failed(1, t0);
        s.due(&[1], t0 + secs(5));
        s.start_failed(1, t0 + secs(5));
        // Logoff: the session is not active, no helper runs.
        assert!(s.due(&[], t0 + secs(6)).is_empty());
        // A new logon in the same session ID: at once, then 5 s again.
        assert_eq!(s.due(&[1], t0 + secs(7)), [1]);
        assert_eq!(s.start_failed(1, t0 + secs(7)), secs(5));
    }

    #[test]
    fn a_helper_of_a_session_that_is_not_active_is_not_restarted() {
        let t0 = Instant::now();
        let mut s = Supervisor::new();
        s.due(&[1], t0);
        s.started(1, 100, t0);
        // The session is disconnected: the running helper keeps its slot.
        assert!(s.due(&[], t0 + secs(2)).is_empty());
        assert_eq!(s.session_of(100), Some(1));
        s.exited(100, t0 + secs(3));
        assert!(s.due(&[], t0 + secs(60)).is_empty());
        // Active again: the start comes at once.
        assert_eq!(s.due(&[1], t0 + secs(61)), [1]);
    }

    #[test]
    fn helper_sessions_are_active_with_a_user_and_not_session_0() {
        let s = |id, state, user: &str| WtsSession {
            id,
            state,
            flags: -1,
            user: user.to_string(),
        };
        let sessions = [
            s(0, WTS_ACTIVE, "SYSTEM"),
            s(1, WTS_ACTIVE, "jahu"),
            s(2, 4, "zoe"), // WTSDisconnected
            s(3, 1, ""),    // WTSConnected: the login screen
            s(4, WTS_ACTIVE, ""),
            s(5, WTS_ACTIVE, "rdp"),
        ];
        assert_eq!(helper_sessions(&sessions), [1, 5]);
    }

    // Client check.

    #[test]
    fn a_client_is_accepted_only_when_started_by_the_service_in_its_session() {
        assert_eq!(accept_client(Some(1), Some(1)), Some(1));
        // Not started by the service.
        assert_eq!(accept_client(None, Some(1)), None);
        // Started for another session.
        assert_eq!(accept_client(Some(1), Some(2)), None);
        // The session of the process cannot be read.
        assert_eq!(accept_client(Some(1), None), None);
    }

    // Pipe DACL.

    #[test]
    fn the_production_pipe_dacl_is_system_full_and_interactive_read_and_write() {
        assert_eq!(PIPE_NAME, r"\\.\pipe\pc-agent");
        assert_eq!(pipe_sddl(None), PIPE_SDDL);
        assert_eq!(PIPE_SDDL, "D:P(A;;FA;;;SY)(A;;0x12019b;;;IU)");
        // Protected, two allow ACEs: SY (LocalSystem) file-all, IU (INTERACTIVE)
        // the read and write mask.
        let aces: Vec<&str> = PIPE_SDDL
            .strip_prefix("D:P")
            .unwrap()
            .trim_matches(|c| c == '(' || c == ')')
            .split(")(")
            .collect();
        assert_eq!(aces, ["A;;FA;;;SY", "A;;0x12019b;;;IU"]);
        // Read and write: all of FILE_GENERIC_READ and FILE_GENERIC_WRITE
        // except the right to create a pipe instance.
        assert_eq!(PIPE_CLIENT_RIGHTS, 0x0012_019b);
        assert_eq!(PIPE_CLIENT_RIGHTS & FILE_GENERIC_READ, FILE_GENERIC_READ);
        assert_eq!(
            PIPE_CLIENT_RIGHTS & FILE_GENERIC_WRITE,
            FILE_GENERIC_WRITE & !FILE_CREATE_PIPE_INSTANCE
        );
        assert_eq!(PIPE_CLIENT_RIGHTS & FILE_CREATE_PIPE_INSTANCE, 0);
        // The helper asks for no more than INTERACTIVE gets.
        assert_eq!(HELPER_ACCESS & !PIPE_CLIENT_RIGHTS, 0);
    }

    #[test]
    fn the_test_pipe_dacl_adds_the_user_sid_with_the_same_rights() {
        assert_eq!(
            pipe_sddl(Some("S-1-5-21-1-2-3-500")),
            "D:P(A;;FA;;;SY)(A;;0x12019b;;;IU)(A;;0x12019b;;;S-1-5-21-1-2-3-500)"
        );
    }

    // XInput change rule.

    fn stick(lx: i16) -> Pad {
        Pad {
            thumb_lx: lx,
            ..Pad::default()
        }
    }

    #[test]
    fn a_stick_inside_the_dead_zone_does_not_count() {
        let rest = Pad::default();
        // 10 % of the range from the center, as in the Linux rule.
        for lx in [0, 3000, -3000, 6553, -6554] {
            assert!(!gamepad_activity(Some((1, &rest)), 2, &stick(lx)), "{lx}");
        }
        for v in [6554, -6555, 20000, i16::MAX, i16::MIN] {
            assert!(gamepad_activity(Some((1, &rest)), 2, &stick(v)), "{v}");
            let each = [
                Pad {
                    thumb_ly: v,
                    ..rest
                },
                Pad {
                    thumb_rx: v,
                    ..rest
                },
                Pad {
                    thumb_ry: v,
                    ..rest
                },
            ];
            for pad in each {
                assert!(gamepad_activity(Some((1, &rest)), 2, &pad), "{pad:?}");
            }
        }
    }

    #[test]
    fn a_trigger_counts_above_10_percent() {
        let rest = Pad::default();
        for t in [0, 10, 25] {
            let pad = Pad {
                left_trigger: t,
                right_trigger: t,
                ..rest
            };
            assert!(!gamepad_activity(Some((1, &rest)), 2, &pad), "{t}");
        }
        for pad in [
            Pad {
                left_trigger: 26,
                ..rest
            },
            Pad {
                right_trigger: 255,
                ..rest
            },
        ] {
            assert!(gamepad_activity(Some((1, &rest)), 2, &pad), "{pad:?}");
        }
    }

    #[test]
    fn a_new_button_press_counts_and_a_release_does_not() {
        const A: u16 = 0x1000;
        const DPAD_UP: u16 = 0x0001;
        let rest = Pad::default();
        let a = Pad { buttons: A, ..rest };
        assert!(gamepad_activity(Some((1, &rest)), 2, &a));
        assert!(gamepad_activity(
            Some((1, &rest)),
            2,
            &Pad {
                buttons: DPAD_UP,
                ..rest
            }
        ));
        // Release.
        assert!(!gamepad_activity(Some((2, &a)), 3, &rest));
        // A held button with a new packet (stick noise in the dead zone).
        let held = Pad { thumb_lx: 100, ..a };
        assert!(!gamepad_activity(Some((2, &a)), 3, &held));
        // A second button while one is held.
        let both = Pad {
            buttons: A | DPAD_UP,
            ..rest
        };
        assert!(gamepad_activity(Some((2, &a)), 3, &both));
    }

    #[test]
    fn no_packet_change_or_a_first_read_does_not_count() {
        let far = stick(30000);
        let rest = Pad::default();
        assert!(!gamepad_activity(Some((7, &rest)), 7, &far));
        assert!(!gamepad_activity(None, 7, &far));
    }

    // Input-time merge and the send rule.

    #[test]
    fn the_keyboard_time_is_now_minus_the_age() {
        // 2026-08-28 04:55:55.900Z minus 2.5 s.
        assert_eq!(keyboard_input(1_787_892_955_900, 2_500), 1_787_892_953);
        assert_eq!(keyboard_input(1_787_892_955_900, 0), 1_787_892_955);
    }

    #[test]
    fn the_input_time_is_the_newest_of_keyboard_and_gamepad() {
        assert_eq!(merge_input(Some(10), Some(20)), Some(20));
        assert_eq!(merge_input(Some(30), Some(20)), Some(30));
        assert_eq!(merge_input(None, Some(20)), Some(20));
        assert_eq!(merge_input(Some(10), None), Some(10));
        assert_eq!(merge_input(None, None), None);
    }

    #[test]
    fn the_helper_sends_the_first_value_at_once_then_a_change_at_most_each_2_s() {
        let mut rule = SendRule::default();
        assert!(rule.due(100, 0));
        // No change: nothing.
        assert!(!rule.due(100, 5_000));
        // A change within 2 s of the last send waits.
        let mut rule = SendRule::default();
        assert!(rule.due(100, 10_000));
        assert!(!rule.due(101, 11_000));
        assert!(!rule.due(102, 11_999));
        assert!(rule.due(102, 12_000));
        assert!(!rule.due(102, 20_000));
        assert!(rule.due(103, 20_000));
    }

    // Input state of the service and the presence fallback.

    #[test]
    fn only_the_helper_of_the_console_session_sets_the_input_time() {
        let mut s = InputState::new();
        s.connected(1);
        s.connected(2);
        s.helper_input(2, 900, 1, 1_000);
        assert_eq!(s.last_input(), None);
        s.helper_input(1, 900, 1, 1_000);
        assert_eq!(s.last_input(), Some(900));
        // Clamped: before the last value, and in the future.
        s.helper_input(1, 800, 1, 1_000);
        assert_eq!(s.last_input(), Some(900));
        s.helper_input(1, 5_000, 1, 1_010);
        assert_eq!(s.last_input(), Some(1_010));
    }

    #[test]
    fn inputhelper_follows_the_connections_of_the_session() {
        let mut s = InputState::new();
        assert!(!s.helper(1));
        s.connected(1);
        // A restart overlaps: the new helper connects before the old one goes.
        s.connected(1);
        s.disconnected(1);
        assert!(s.helper(1));
        assert!(!s.helper(2));
        s.disconnected(1);
        assert!(!s.helper(1));
        // A spare disconnect does not underflow.
        s.disconnected(1);
        s.connected(1);
        assert!(s.helper(1));
    }

    #[test]
    fn a_change_to_present_without_a_helper_sets_the_input_time_to_now() {
        let mut s = InputState::new();
        // The first value (at the registration) is no change.
        s.presence(true, 1, 1_000);
        assert_eq!(s.last_input(), None);
        s.presence(false, 1, 1_100);
        assert_eq!(s.last_input(), None);
        s.presence(true, 1, 1_200);
        assert_eq!(s.last_input(), Some(1_200));
        // Present again with no change between: nothing.
        s.presence(true, 1, 1_300);
        assert_eq!(s.last_input(), Some(1_200));
        // A first value of inactive, then present, is a change.
        let mut s = InputState::new();
        s.presence(false, 1, 1_000);
        s.presence(true, 1, 1_050);
        assert_eq!(s.last_input(), Some(1_050));
    }

    #[test]
    fn with_a_helper_of_the_console_session_presence_is_not_used() {
        let mut s = InputState::new();
        s.connected(1);
        s.helper_input(1, 900, 1, 1_000);
        s.presence(false, 1, 1_100);
        s.presence(true, 1, 1_200);
        assert_eq!(s.last_input(), Some(900));
        // A helper of another session does not stop the fallback.
        s.presence(false, 2, 1_300);
        s.presence(true, 2, 1_400);
        assert_eq!(s.last_input(), Some(1_400));
    }
}
