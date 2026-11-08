//! The WTS reads of the session sources: the console session for
//! `sessionstate` and the session list for `loggedusers`. The rules are in
//! `wts_rules`.

use ::windows::Win32::System::RemoteDesktop::{
    WTS_CURRENT_SERVER_HANDLE, WTS_SESSION_INFOW, WTSEnumerateSessionsW, WTSFreeMemory,
    WTSGetActiveConsoleSessionId, WTSINFOEXW, WTSQuerySessionInformationW, WTSSessionInfoEx,
};
use ::windows::core::PWSTR;
use tracing::debug;

use super::super::wts_rules::{self, WtsSession};
use crate::core::SessionState;

/// The session state of the console session; `None` when the read failed.
pub fn console_state() -> Option<SessionState> {
    // SAFETY: WTSGetActiveConsoleSessionId has no arguments and no preconditions.
    let console = unsafe { WTSGetActiveConsoleSessionId() };
    wts_rules::session_state(console, session)
}

/// The user names of the logged-in sessions; `None` when the list failed.
pub fn logged_users() -> Option<Vec<String>> {
    sessions().map(|list| wts_rules::logged_users(&list))
}

/// `WTSSessionInfoEx` of each session; `None` when the list failed.
pub fn sessions() -> Option<Vec<WtsSession>> {
    let mut list: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: valid out pointers; the list is freed with WTSFreeMemory below.
    if let Err(e) = unsafe {
        WTSEnumerateSessionsW(Some(WTS_CURRENT_SERVER_HANDLE), 0, 1, &mut list, &mut count)
    } {
        debug!(error = %e, "WTSEnumerateSessionsW failed");
        return None;
    }
    // SAFETY: on success `list` holds `count` entries.
    let ids: Vec<u32> = unsafe { std::slice::from_raw_parts(list, count as usize) }
        .iter()
        .map(|s| s.SessionId)
        .collect();
    // SAFETY: `list` came from WTSEnumerateSessionsW and is not used after this.
    unsafe { WTSFreeMemory(list.cast()) };
    // A failed read of one session fails the whole list: dropping that user
    // would give a false SessionLogoff and then a false SessionLogon. The
    // last list stays until the next poll.
    ids.into_iter().map(session).collect()
}

/// `WTSSessionInfoEx` of one session; `None` when the read failed.
pub(super) fn session(id: u32) -> Option<WtsSession> {
    let mut buf = PWSTR::null();
    let mut bytes = 0u32;
    // SAFETY: valid out pointers; the buffer is freed with WTSFreeMemory below.
    if let Err(e) = unsafe {
        WTSQuerySessionInformationW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            id,
            WTSSessionInfoEx,
            &mut buf,
            &mut bytes,
        )
    } {
        debug!(session = id, error = %e, "WTSSessionInfoEx failed");
        return None;
    }
    let read = if (bytes as usize) < std::mem::size_of::<WTSINFOEXW>() {
        None
    } else {
        // SAFETY: on success the buffer holds a WTSINFOEXW of `bytes` bytes;
        // level 1 is the only level and the union holds WTSINFOEX_LEVEL1_W.
        unsafe {
            let info = &*(buf.0 as *const WTSINFOEXW);
            (info.Level == 1).then(|| {
                let l1 = &info.Data.WTSInfoExLevel1;
                WtsSession {
                    id: l1.SessionId,
                    state: l1.SessionState.0,
                    flags: l1.SessionFlags,
                    user: from_wide(&l1.UserName),
                }
            })
        }
    };
    // SAFETY: `buf` came from WTSQuerySessionInformationW and is not used after this.
    unsafe { WTSFreeMemory(buf.0.cast()) };
    read
}

/// The text of a NUL-terminated UTF-16 array.
fn from_wide(chars: &[u16]) -> String {
    let len = chars.iter().position(|&c| c == 0).unwrap_or(chars.len());
    String::from_utf16_lossy(&chars[..len])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wide_name_ends_at_the_first_nul() {
        let mut name = [0u16; 21];
        for (i, c) in "jahu".encode_utf16().enumerate() {
            name[i] = c;
        }
        assert_eq!(from_wide(&name), "jahu");
        assert_eq!(from_wide(&[0u16; 21]), "");
    }

    #[test]
    fn the_reads_of_this_machine_work() {
        // The CI runner and a dev PC both have session lists.
        assert!(logged_users().is_some());
        assert!(console_state().is_some());
    }
}
