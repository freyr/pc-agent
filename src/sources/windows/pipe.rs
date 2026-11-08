//! The pipe server of the service. It creates the instances of the pipe
//! with the DACL of the spec, accepts a client only when the client check
//! gives its session, and reads its JSON lines. The rules (line framing,
//! parser, DACL string, client check) are in `helper_rules`.

use std::io;
use std::os::windows::io::AsRawHandle;

use ::windows::Win32::Foundation::{HANDLE, HLOCAL, LocalFree};
use ::windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use ::windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use ::windows::Win32::System::Pipes::GetNamedPipeClientProcessId;
use ::windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use ::windows::core::HSTRING;
use tokio::io::AsyncReadExt;
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, warn};

use super::super::helper_rules::{HelperMessage, LineFramer};

/// What the pipe server reports, with the session of the helper.
#[derive(Debug, PartialEq)]
pub enum PipeEvent {
    Connected(u32),
    Message(u32, HelperMessage),
    Disconnected(u32),
}

/// The session of the process `pid`; `None` when the read failed.
pub fn process_session(pid: u32) -> Option<u32> {
    let mut session = 0u32;
    // SAFETY: `session` is a valid out pointer.
    unsafe { ProcessIdToSessionId(pid, &mut session) }
        .ok()
        .map(|()| session)
}

/// Serves the pipe `name` with the DACL `sddl` until an instance cannot be
/// created. `check` gives the session of an accepted client process, or
/// `None` to refuse it. Each accepted client is read on its own task.
pub async fn serve(
    name: &str,
    sddl: &str,
    check: impl Fn(u32) -> Option<u32>,
    events: UnboundedSender<PipeEvent>,
) -> io::Result<()> {
    // The first instance fails when a pipe of this name exists already, so
    // no other process holds the name before the service.
    let mut server = create(name, sddl, true)?;
    loop {
        let connected = server.connect().await;
        // A listening instance exists at all times: the next one is made
        // before the connected one leaves `server`.
        let client = std::mem::replace(&mut server, create(name, sddl, false)?);
        if let Err(e) = connected {
            debug!(error = %e, "pipe connect failed");
            continue;
        }
        let pid = client_pid(&client);
        match pid.and_then(&check) {
            Some(session) => {
                tokio::spawn(read_client(client, session, events.clone()));
            }
            None => warn!(
                ?pid,
                "pipe: refused a client that the service did not start"
            ),
        }
    }
}

/// The process ID of the client of a connected instance.
fn client_pid(pipe: &NamedPipeServer) -> Option<u32> {
    let mut pid = 0u32;
    // SAFETY: the handle of `pipe` is valid while `pipe` lives; `pid` is a
    // valid out pointer.
    unsafe { GetNamedPipeClientProcessId(HANDLE(pipe.as_raw_handle()), &mut pid) }
        .ok()
        .map(|()| pid)
}

/// Reads the lines of one helper until it closes the pipe.
async fn read_client(mut pipe: NamedPipeServer, session: u32, events: UnboundedSender<PipeEvent>) {
    let _ = events.send(PipeEvent::Connected(session));
    let mut framer = LineFramer::default();
    let mut buf = [0u8; 4096];
    loop {
        match pipe.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                for line in framer.push(&buf[..n]) {
                    match line {
                        Ok(message) => {
                            let _ = events.send(PipeEvent::Message(session, message));
                        }
                        Err(e) => warn!(session, error = ?e, "pipe: dropped a helper line"),
                    }
                }
            }
        }
    }
    let _ = events.send(PipeEvent::Disconnected(session));
}

/// One instance of the pipe with the DACL `sddl`.
fn create(name: &str, sddl: &str, first: bool) -> io::Result<NamedPipeServer> {
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: the SDDL string outlives the call; `sd` receives a buffer that
    // LocalFree releases below, after its last use.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            &HSTRING::from(sddl),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )
    }
    .map_err(io::Error::other)?;
    let mut attrs = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: false.into(),
    };
    // SAFETY: `attrs` is a valid SECURITY_ATTRIBUTES whose descriptor lives
    // until the LocalFree below; the call copies it into the pipe.
    let server = unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(name, (&raw mut attrs).cast())
    };
    // SAFETY: `sd` came from ConvertStringSecurityDescriptorToSecurityDescriptorW
    // and is not used after this.
    unsafe {
        LocalFree(Some(HLOCAL(sd.0)));
    }
    server
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use tokio::sync::mpsc::unbounded_channel;

    use super::*;
    use crate::sources::helper_rules::{accept_client, pipe_sddl};

    /// The SID of the user of this process, from `whoami /user`.
    fn user_sid() -> String {
        let out = Command::new("whoami")
            .args(["/user", "/fo", "csv", "/nh"])
            .output()
            .expect("whoami /user");
        let text = String::from_utf8_lossy(&out.stdout);
        text.trim()
            .rsplit(',')
            .next()
            .map(|s| s.trim_matches('"').to_string())
            .filter(|s| s.starts_with("S-1-"))
            .unwrap_or_else(|| panic!("no SID in {text:?}"))
    }

    /// The CI job `windows` runs it after a build, with `PC_AGENT_EXE` set
    /// to the built exe: a unit test of a bin crate has no path to it.
    #[tokio::test]
    #[ignore = "needs PC_AGENT_EXE; the CI job windows runs it"]
    async fn the_helper_sends_last_input_over_a_test_pipe() {
        let exe = std::env::var("PC_AGENT_EXE").expect("PC_AGENT_EXE");
        let groups = Command::new("whoami").arg("/groups").output().unwrap();
        println!("{}", String::from_utf8_lossy(&groups.stdout));
        let sid = user_sid();
        println!("user SID {sid}");

        let name = format!(r"\\.\pipe\pc-agent-test-{}", std::process::id());
        // The test process may not have INTERACTIVE: grant its user too.
        let sddl = pipe_sddl(Some(&sid));
        let own_session = process_session(std::process::id()).unwrap();
        let child = Arc::new(AtomicU32::new(0));
        let started = Arc::clone(&child);
        // The client check of the service: only the process that this test
        // started, in the session it runs in.
        let check = move |pid: u32| {
            let started_for = (pid == started.load(Ordering::SeqCst)).then_some(own_session);
            accept_client(started_for, process_session(pid))
        };
        let (tx, mut rx) = unbounded_channel();
        let server_name = name.clone();
        let server = tokio::spawn(async move { serve(&server_name, &sddl, check, tx).await });
        // Let the server create its first instance.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let mut helper = Command::new(&exe)
            .args(["session-helper", "--pipe", &name])
            .spawn()
            .expect("start pc-agent session-helper");
        child.store(helper.id(), Ordering::SeqCst);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let got = tokio::time::timeout(Duration::from_secs(20), async {
            let mut connected = false;
            while let Some(event) = rx.recv().await {
                println!("{event:?}");
                match event {
                    PipeEvent::Connected(s) => {
                        assert_eq!(s, own_session);
                        connected = true;
                    }
                    PipeEvent::Message(s, HelperMessage::LastInput(t)) => {
                        assert!(connected);
                        return (s, t);
                    }
                    PipeEvent::Disconnected(_) => panic!("the helper disconnected"),
                }
            }
            panic!("the server stopped");
        })
        .await;
        let _ = helper.kill();
        let _ = helper.wait();
        server.abort();
        let (session, t) = got.expect("no last_input within 20 s");
        assert_eq!(session, own_session);
        // The last input of this session: not in the future, after 2023.
        assert!(t <= now + 1 && t > 1_700_000_000, "{t}");
    }
}
