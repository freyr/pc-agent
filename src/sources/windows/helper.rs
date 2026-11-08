//! The session helper: `pc-agent session-helper --probe` reads the input
//! state once in the user session, and the start of it from the service
//! (LocalSystem) in the console session. W6 adds the running helper.

use std::fs::File;
use std::io::Read;
use std::os::windows::io::FromRawHandle;
use std::process::ExitCode;

use ::windows::Win32::Foundation::{
    CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation, WAIT_OBJECT_0,
};
use ::windows::Win32::Security::SECURITY_ATTRIBUTES;
use ::windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use ::windows::Win32::System::Pipes::CreatePipe;
use ::windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use ::windows::Win32::System::SystemInformation::GetTickCount;
use ::windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, GetExitCodeProcess,
    PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};
use ::windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use ::windows::Win32::UI::Input::XboxController::{XINPUT_STATE, XInputGetState};
use ::windows::core::{HSTRING, PWSTR};

use super::super::win_diagnose::{self, Probe, step_error, win32_code};
use super::super::wts_rules::NO_CONSOLE;

/// The longest wait for the probe to exit.
const PROBE_WAIT_MS: u32 = 5_000;

/// `pc-agent session-helper --probe`: one read, printed to stdout.
pub fn probe() -> ExitCode {
    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    // SAFETY: `info` is a valid LASTINPUTINFO with cbSize set.
    let last = unsafe { GetLastInputInfo(&mut info) }.as_bool();
    // SAFETY: GetTickCount has no arguments and no preconditions.
    let now = unsafe { GetTickCount() };
    let mut slots = [None; 4];
    for (slot, packet) in slots.iter_mut().enumerate() {
        let mut state = XINPUT_STATE::default();
        // SAFETY: `state` is a valid out pointer; slots 0-3 are the valid indexes.
        if unsafe { XInputGetState(slot as u32, &mut state) } == 0 {
            *packet = Some(state.dwPacketNumber);
        }
    }
    if !last {
        eprintln!("GetLastInputInfo failed");
        return ExitCode::FAILURE;
    }
    print!(
        "{}",
        win_diagnose::format_probe(&Probe {
            last_input_ms: win_diagnose::tick_age(now, info.dwTime),
            slots,
        })
    );
    ExitCode::SUCCESS
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
    let mut token = HANDLE::default();
    // SAFETY: `token` is a valid out pointer; it is closed below.
    unsafe { WTSQueryUserToken(session, &mut token) }
        .map_err(|e| failed("WTSQueryUserToken", &e))?;
    let result = start_as(token);
    // SAFETY: `token` came from WTSQueryUserToken and is not used after this.
    unsafe {
        let _ = CloseHandle(token);
    }
    let output = result?;
    win_diagnose::parse_probe(&output).map(|probe| (session, probe))
}

/// Runs `<this exe> session-helper --probe` with the user token `token` on
/// the desktop `winsta0\default` and gives its stdout.
fn start_as(token: HANDLE) -> Result<String, String> {
    let mut env: *mut core::ffi::c_void = std::ptr::null_mut();
    // SAFETY: `env` is a valid out pointer; the block is destroyed below.
    unsafe { CreateEnvironmentBlock(&mut env, Some(token), false) }
        .map_err(|e| failed("CreateEnvironmentBlock", &e))?;
    let result = start_with_env(token, env);
    // SAFETY: `env` came from CreateEnvironmentBlock and is not used after this.
    unsafe {
        let _ = DestroyEnvironmentBlock(env);
    }
    result
}

fn start_with_env(token: HANDLE, env: *mut core::ffi::c_void) -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe failed: {e}"))?;
    let exe = HSTRING::from(exe.as_os_str());
    // CreateProcessAsUserW can write to the command line, so it is a
    // mutable, NUL-terminated buffer.
    let mut command: Vec<u16> = format!("\"{exe}\" session-helper --probe")
        .encode_utf16()
        .chain([0])
        .collect();
    let mut desktop: Vec<u16> = "winsta0\\default".encode_utf16().chain([0]).collect();

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

    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        dwFlags: STARTF_USESTDHANDLES,
        hStdOutput: write,
        hStdError: write,
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    // SAFETY: the buffers outlive the call; `startup` and `process` are valid.
    let created = unsafe {
        CreateProcessAsUserW(
            Some(token),
            &exe,
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            true,
            CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
            Some(env.cast_const()),
            None,
            &startup,
            &mut process,
        )
    };
    // The child has its copy of the write end: without this close, the read
    // below never sees the end of the pipe.
    // SAFETY: `write` is a valid handle, not used after this.
    unsafe {
        let _ = CloseHandle(write);
    }
    created.map_err(|e| failed("CreateProcessAsUserW", &e))?;

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
fn failed(step: &str, e: &::windows::core::Error) -> String {
    step_error(step, win32_code(e.code().0))
}
