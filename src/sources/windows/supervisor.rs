//! The helpers of the sessions: a thread of the service starts
//! `pc-agent session-helper` in each active user session, at service start
//! and when a session becomes active (a logon), and starts it again after
//! an exit or a failed start with the back-off of `helper_rules::Supervisor`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ::windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use ::windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
use tracing::{info, warn};

use super::super::helper_rules::{self, Supervisor};
use super::{helper, wts};

/// The interval of the session and exit checks.
const CHECK_INTERVAL: Duration = Duration::from_secs(2);

/// The helpers that the service started: the pipe server reads it for the
/// client check.
pub static SUPERVISOR: Mutex<Supervisor> = Mutex::new(Supervisor::new());

/// Starts the supervisor thread.
pub fn start() {
    if let Err(e) = std::thread::Builder::new()
        .name("helpers".to_string())
        .spawn(run)
    {
        warn!("no session helpers: cannot start the thread: {e}");
    }
}

fn run() {
    // The process handle of each running helper, by process ID. An open
    // handle keeps the ID from reuse, so the client check cannot accept
    // another process with the ID of a helper that exited.
    let mut processes: HashMap<u32, HANDLE> = HashMap::new();
    loop {
        let now = Instant::now();
        processes.retain(|&pid, &mut process| {
            // SAFETY: `process` is a valid process handle until it is closed here.
            if unsafe { WaitForSingleObject(process, 0) } != WAIT_OBJECT_0 {
                return true;
            }
            let mut code = 0u32;
            // SAFETY: `process` is valid; `code` is a valid out pointer.
            let _ = unsafe { GetExitCodeProcess(process, &mut code) };
            if let Some((session, delay)) = lock().exited(pid, now) {
                warn!(
                    session,
                    pid,
                    exit_code = code,
                    "the session helper exited; next start in {delay:?}"
                );
            }
            // SAFETY: `process` is not used after this.
            unsafe {
                let _ = CloseHandle(process);
            }
            false
        });
        if let Some(sessions) = wts::sessions() {
            let active = helper_rules::helper_sessions(&sessions);
            // The lock stays held over the start, so the client check of a
            // helper that connects at once finds its process ID.
            let mut supervisor = lock();
            for session in supervisor.due(&active, now) {
                match helper::start_helper(session) {
                    Ok((pid, process)) => {
                        info!(session, pid, "started the session helper");
                        supervisor.started(session, pid, now);
                        processes.insert(pid, process);
                    }
                    Err(e) => {
                        let delay = supervisor.start_failed(session, now);
                        warn!(
                            session,
                            "cannot start the session helper: {e}; next try in {delay:?}"
                        );
                    }
                }
            }
        }
        std::thread::sleep(CHECK_INTERVAL);
    }
}

fn lock() -> std::sync::MutexGuard<'static, Supervisor> {
    SUPERVISOR.lock().unwrap_or_else(|e| e.into_inner())
}
