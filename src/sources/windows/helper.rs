//! The session helper. `pc-agent session-helper` runs in the user session:
//! each 1 s it reads `GetLastInputInfo` and the XInput slots, and sends the
//! input time to the service over the pipe. `pc-agent session-helper
//! --probe` reads the input state once, for `diagnose`. The start of both
//! from the service (LocalSystem) in a user session is here too. The rules
//! are in `helper_rules`.

use std::fs::File;
use std::io::{Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ::windows::Win32::Foundation::{
    CloseHandle, ERROR_PIPE_BUSY, HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation,
    WAIT_OBJECT_0,
};
use ::windows::Win32::Security::SECURITY_ATTRIBUTES;
use ::windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_NONE, OPEN_EXISTING, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use ::windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use ::windows::Win32::System::Pipes::{CreatePipe, PeekNamedPipe};
use ::windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use ::windows::Win32::System::SystemInformation::GetTickCount;
use ::windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, GetExitCodeProcess,
    PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};
use ::windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use ::windows::Win32::UI::Input::XboxController::{XINPUT_STATE, XInputGetState};
use ::windows::core::{HSTRING, PWSTR};

use super::super::helper_rules::{
    HELPER_ACCESS, HelperMessage, PIPE_NAME, Pad, SendRule, gamepad_activity, keyboard_input,
    merge_input,
};
use super::super::win_diagnose::{self, Probe, step_error, win32_code};
use super::super::wts_rules::NO_CONSOLE;

/// The longest wait for the probe to exit.
const PROBE_WAIT_MS: u32 = 5_000;

/// The interval of the input reads of the helper.
const READ_INTERVAL: Duration = Duration::from_secs(1);

/// The tries to open the pipe while all its instances are busy.
const OPEN_TRIES: u32 = 10;

/// The age of the last keyboard or mouse input in ms (`GetLastInputInfo`);
/// `None` when the read failed.
fn last_input_age() -> Option<u32> {
    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    // SAFETY: `info` is a valid LASTINPUTINFO with cbSize set.
    let ok = unsafe { GetLastInputInfo(&mut info) }.as_bool();
    // SAFETY: GetTickCount has no arguments and no preconditions.
    let now = unsafe { GetTickCount() };
    ok.then(|| win_diagnose::tick_age(now, info.dwTime))
}

/// The state of XInput slot `slot` (0-3): the packet number and the
/// gamepad; `None` when no controller is connected there.
fn xinput(slot: u32) -> Option<(u32, Pad)> {
    let mut state = XINPUT_STATE::default();
    // SAFETY: `state` is a valid out pointer; slots 0-3 are the valid indexes.
    if unsafe { XInputGetState(slot, &mut state) } != 0 {
        return None;
    }
    let g = state.Gamepad;
    Some((
        state.dwPacketNumber,
        Pad {
            buttons: g.wButtons.0,
            left_trigger: g.bLeftTrigger,
            right_trigger: g.bRightTrigger,
            thumb_lx: g.sThumbLX,
            thumb_ly: g.sThumbLY,
            thumb_rx: g.sThumbRX,
            thumb_ry: g.sThumbRY,
        },
    ))
}

/// `pc-agent session-helper --probe`: one read, printed to stdout.
pub fn probe() -> ExitCode {
    let last = last_input_age();
    let mut slots = [None; 4];
    for (slot, packet) in slots.iter_mut().enumerate() {
        *packet = xinput(slot as u32).map(|(packet, _)| packet);
    }
    let Some(last_input_ms) = last else {
        eprintln!("GetLastInputInfo failed");
        return ExitCode::FAILURE;
    };
    print!(
        "{}",
        win_diagnose::format_probe(&Probe {
            last_input_ms,
            slots
        })
    );
    ExitCode::SUCCESS
}

/// `pc-agent session-helper [--pipe <name>]`: sends the input time of this
/// session to the pipe (`\\.\pipe\pc-agent` by default) until the service
/// closes it. The first value goes at once, then a change at most each 2 s.
/// Each 1 s it checks the pipe, so it exits within 1 s after the service
/// stops, also with no input.
pub fn run(pipe: Option<&str>) -> ExitCode {
    let mut out = match open_pipe(pipe.unwrap_or(PIPE_NAME)) {
        Ok(file) => file,
        Err(e) => {
            eprintln!("cannot open the pipe: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut slots: [Option<(u32, Pad)>; 4] = [None; 4];
    let mut gamepad = None;
    let mut rule = SendRule::default();
    loop {
        if !pipe_open(&out) {
            // The service closed the pipe: it stopped or restarts.
            return ExitCode::SUCCESS;
        }
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);
        let keyboard = last_input_age().map(|age| keyboard_input(now_ms, age));
        for (slot, prev) in slots.iter_mut().enumerate() {
            let read = xinput(slot as u32);
            if let Some((packet, pad)) = &read
                && gamepad_activity(prev.as_ref().map(|(p, s)| (*p, s)), *packet, pad)
            {
                gamepad = Some(now_ms.div_euclid(1000));
            }
            *prev = read;
        }
        if let Some(t) = merge_input(keyboard, gamepad)
            && rule.due(t, now_ms)
            && out
                .write_all(HelperMessage::LastInput(t).to_line().as_bytes())
                .is_err()
        {
            // The service closed the pipe: it stopped or restarts.
            return ExitCode::SUCCESS;
        }
        std::thread::sleep(READ_INTERVAL);
    }
}

/// `false` when the server end of the pipe is closed. `PeekNamedPipe` does
/// not block (a reader thread would: a blocked `ReadFile` on a synchronous
/// handle also blocks the writes); it needs the `FILE_READ_DATA` of
/// `HELPER_ACCESS`.
fn pipe_open(pipe: &File) -> bool {
    // SAFETY: the handle of `pipe` is valid while `pipe` lives; no buffer is passed.
    unsafe { PeekNamedPipe(HANDLE(pipe.as_raw_handle()), None, 0, None, None, None) }.is_ok()
}

/// Opens the client end of the pipe `name` for writing. It asks only for
/// the rights that the pipe DACL gives `INTERACTIVE`, and lets the server
/// identify the user but not impersonate it.
fn open_pipe(name: &str) -> Result<File, String> {
    let wide = HSTRING::from(name);
    let mut tries = 0;
    loop {
        // SAFETY: `wide` is a NUL-terminated name that outlives the call; the
        // handle is owned by the File below.
        let opened = unsafe {
            CreateFileW(
                &wide,
                HELPER_ACCESS,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                None,
            )
        };
        match opened {
            // SAFETY: the handle is valid and owned by this File from here on.
            Ok(handle) => return Ok(unsafe { File::from_raw_handle(handle.0) }),
            Err(e) if e.code() == ERROR_PIPE_BUSY.to_hresult() && tries < OPEN_TRIES => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(500));
            }
            Err(e) => return Err(failed("CreateFileW", &e)),
        }
    }
}

/// Starts the probe in the console session as the user of that session
/// and reads its output: the session ID and the probe, or the failed step.
/// It works only as LocalSystem (`WTSQueryUserToken` needs SE_TCB_NAME).
pub fn start_probe() -> Result<(u32, Probe), String> {
    // SAFETY: WTSGetActiveConsoleSessionId has no arguments and no preconditions.
    let session = unsafe { WTSGetActiveConsoleSessionId() };
    if session == NO_CONSOLE {
        return Err("no console session".to_string());
    }
    let output = run_probe(session)?;
    win_diagnose::parse_probe(&output).map(|probe| (session, probe))
}

/// Starts `pc-agent session-helper` in `session`. Gives the process ID and
/// the process handle, which the caller closes.
pub fn start_helper(session: u32) -> Result<(u32, HANDLE), String> {
    let process = start_in_session(session, "session-helper", None)?;
    // SAFETY: the thread handle is valid and not used after this.
    unsafe {
        let _ = CloseHandle(process.hThread);
    }
    Ok((process.dwProcessId, process.hProcess))
}

/// Starts `<this exe> <args>` in `session` as the user of that session, on
/// the desktop `winsta0\default`, with no window. `stdout` (an inheritable
/// handle) gets stdout and stderr. The caller closes the handles of the
/// process information.
fn start_in_session(
    session: u32,
    args: &str,
    stdout: Option<HANDLE>,
) -> Result<PROCESS_INFORMATION, String> {
    let mut token = HANDLE::default();
    // SAFETY: `token` is a valid out pointer; it is closed below.
    unsafe { WTSQueryUserToken(session, &mut token) }
        .map_err(|e| failed("WTSQueryUserToken", &e))?;
    let result = start_as(token, args, stdout);
    // SAFETY: `token` came from WTSQueryUserToken and is not used after this.
    unsafe {
        let _ = CloseHandle(token);
    }
    result
}

/// `start_in_session` with the user token `token`.
fn start_as(
    token: HANDLE,
    args: &str,
    stdout: Option<HANDLE>,
) -> Result<PROCESS_INFORMATION, String> {
    let mut env: *mut core::ffi::c_void = std::ptr::null_mut();
    // SAFETY: `env` is a valid out pointer; the block is destroyed below.
    unsafe { CreateEnvironmentBlock(&mut env, Some(token), false) }
        .map_err(|e| failed("CreateEnvironmentBlock", &e))?;
    let result = start_with_env(token, env, args, stdout);
    // SAFETY: `env` came from CreateEnvironmentBlock and is not used after this.
    unsafe {
        let _ = DestroyEnvironmentBlock(env);
    }
    result
}

fn start_with_env(
    token: HANDLE,
    env: *mut core::ffi::c_void,
    args: &str,
    stdout: Option<HANDLE>,
) -> Result<PROCESS_INFORMATION, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe failed: {e}"))?;
    let exe = HSTRING::from(exe.as_os_str());
    // CreateProcessAsUserW can write to the command line, so it is a
    // mutable, NUL-terminated buffer.
    let mut command: Vec<u16> = format!("\"{exe}\" {args}")
        .encode_utf16()
        .chain([0])
        .collect();
    let mut desktop: Vec<u16> = "winsta0\\default".encode_utf16().chain([0]).collect();
    let mut startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        ..Default::default()
    };
    if let Some(out) = stdout {
        startup.dwFlags = STARTF_USESTDHANDLES;
        startup.hStdOutput = out;
        startup.hStdError = out;
    }
    let mut process = PROCESS_INFORMATION::default();
    // SAFETY: the buffers outlive the call; `startup` and `process` are valid.
    unsafe {
        CreateProcessAsUserW(
            Some(token),
            &exe,
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            // Only the probe passes a handle (its stdout) to the child.
            stdout.is_some(),
            CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
            Some(env.cast_const()),
            None,
            &startup,
            &mut process,
        )
    }
    .map_err(|e| failed("CreateProcessAsUserW", &e))?;
    Ok(process)
}

/// Runs `<this exe> session-helper --probe` in `session` and gives its stdout.
fn run_probe(session: u32) -> Result<String, String> {
    let inherit = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: true.into(),
    };
    let (mut read, mut write) = (HANDLE::default(), HANDLE::default());
    // SAFETY: valid out pointers; both ends are closed below.
    unsafe { CreatePipe(&mut read, &mut write, Some(&inherit), 0) }
        .map_err(|e| failed("CreatePipe", &e))?;
    // SAFETY: `read` is the read end of the new pipe and is owned by this
    // File from here on, which closes it.
    let mut reader = unsafe { File::from_raw_handle(read.0) };
    // Only the write end goes to the child.
    // SAFETY: `read` is a valid handle.
    if let Err(e) = unsafe { SetHandleInformation(read, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)) } {
        // SAFETY: `write` is a valid handle, not used after this.
        unsafe {
            let _ = CloseHandle(write);
        }
        return Err(failed("SetHandleInformation", &e));
    }
    let created = start_in_session(session, "session-helper --probe", Some(write));
    // The child has its copy of the write end: without this close, the read
    // below never sees the end of the pipe.
    // SAFETY: `write` is a valid handle, not used after this.
    unsafe {
        let _ = CloseHandle(write);
    }
    let process = created?;

    // SAFETY: the handles of `process` are valid until they are closed here.
    let exit = unsafe {
        let _ = CloseHandle(process.hThread);
        let exit = if WaitForSingleObject(process.hProcess, PROBE_WAIT_MS) == WAIT_OBJECT_0 {
            let mut code = 0u32;
            GetExitCodeProcess(process.hProcess, &mut code)
                .map(|()| code)
                .map_err(|e| failed("GetExitCodeProcess", &e))
        } else {
            let _ = TerminateProcess(process.hProcess, 1);
            Err(format!(
                "the probe did not exit within {} s",
                PROBE_WAIT_MS / 1000
            ))
        };
        let _ = CloseHandle(process.hProcess);
        exit
    }?;
    // The output is far below the pipe buffer, so the child never blocked
    // on a full pipe, and all of it is there after the exit.
    let mut output = String::new();
    reader
        .read_to_string(&mut output)
        .map_err(|e| format!("cannot read the probe output: {e}"))?;
    if exit != 0 {
        return Err(format!(
            "the probe ended with exit code {exit}: {:?}",
            output.trim()
        ));
    }
    Ok(output)
}

/// The text of a failed Win32 step.
pub(super) fn failed(step: &str, e: &::windows::core::Error) -> String {
    step_error(step, win32_code(e.code().0))
}
