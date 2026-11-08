//! The service frame: the dispatcher, the control handler and the service
//! status. The handler forwards each control to the runtime through a
//! channel and returns at once. `Stop` and `Preshutdown` stop the agent: it
//! publishes the retained `offline` and disconnects. The handler records
//! each console display notification for the poll. The session and power
//! controls reach the runtime too; the later tasks handle them.

use std::ffi::OsString;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use ::windows::Win32::Foundation::{ERROR_FAILED_SERVICE_CONTROLLER_CONNECT, HANDLE};
use ::windows::Win32::System::Power::RegisterPowerSettingNotification;
use ::windows::Win32::System::SystemServices::GUID_CONSOLE_DISPLAY_STATE;
use ::windows::Win32::UI::WindowsAndMessaging::DEVICE_NOTIFY_SERVICE_HANDLE;
use anyhow::Result;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tracing::{debug, error, info, warn};
use windows_service::service::{
    PowerBroadcastSetting, PowerEventParam, ServiceControl, ServiceControlAccept, ServiceExitCode,
    ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

use super::{DATA_DIR, DISPLAY, SERVICE_NAME};
use crate::logfile::LogFile;

/// The size limit of the log file; one old file is kept.
const LOG_LIMIT: u64 = 1024 * 1024;

/// The controls that the service accepts.
const ACCEPTED: ServiceControlAccept = ServiceControlAccept::STOP
    .union(ServiceControlAccept::PRESHUTDOWN)
    .union(ServiceControlAccept::SESSION_CHANGE)
    .union(ServiceControlAccept::POWER_EVENT);

/// The time that the stop may take: the 2 s ack wait of `offline`, the
/// disconnect and some margin.
const STOP_WAIT_HINT: Duration = Duration::from_secs(5);

/// The config path for the service thread, which gets no arguments from main.
static CONFIG: OnceLock<PathBuf> = OnceLock::new();

define_windows_service!(ffi_service_main, service_main);

/// Runs the agent as the service `pc-agent` when the service control
/// manager started this process, and gives its exit code. Gives `None` when
/// the process runs in a console, so main runs the agent there.
pub fn run_as_service(config: &Path) -> Option<ExitCode> {
    let _ = CONFIG.set(config.to_path_buf());
    match service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
        Ok(()) => Some(ExitCode::SUCCESS),
        Err(windows_service::Error::Winapi(e))
            if e.raw_os_error() == Some(ERROR_FAILED_SERVICE_CONTROLLER_CONNECT.0 as i32) =>
        {
            None
        }
        Err(e) => {
            eprintln!("error: cannot start the service dispatcher: {e}");
            Some(ExitCode::FAILURE)
        }
    }
}

/// The entry of the service thread.
fn service_main(_arguments: Vec<OsString>) {
    // Without the log file the service runs with no log, so a log problem
    // does not stop the agent.
    let log_path = Path::new(DATA_DIR).join("pc-agent.log");
    if let Ok(log) = LogFile::open(&log_path, LOG_LIMIT) {
        tracing_subscriber::fmt()
            .with_env_filter(crate::env_filter())
            .with_ansi(false)
            .with_writer(Mutex::new(log))
            .init();
    }
    if let Err(e) = run_service() {
        error!("{e:#}");
    }
}

fn status(state: ServiceState, accepted: ServiceControlAccept, exit: u32) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accepted,
        exit_code: if exit == 0 {
            ServiceExitCode::NO_ERROR
        } else {
            ServiceExitCode::ServiceSpecific(exit)
        },
        checkpoint: 0,
        wait_hint: if state == ServiceState::StopPending {
            STOP_WAIT_HINT
        } else {
            Duration::ZERO
        },
        process_id: None,
    }
}

fn run_service() -> Result<()> {
    let (tx, rx) = unbounded_channel();
    let handler = move |control| match control {
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        ServiceControl::Stop
        | ServiceControl::Preshutdown
        | ServiceControl::SessionChange(_)
        | ServiceControl::PowerEvent(_) => {
            if let ServiceControl::PowerEvent(PowerEventParam::PowerSettingChange(
                PowerBroadcastSetting::ConsoleDisplayState(state),
            )) = &control
            {
                DISPLAY.record(state.to_raw());
            }
            // The runtime is gone after the stop: nothing to forward to then.
            let _ = tx.send(control);
            ServiceControlHandlerResult::NoError
        }
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let handle = service_control_handler::register(SERVICE_NAME, handler)?;
    handle.set_service_status(status(ServiceState::Running, ACCEPTED, 0))?;
    // Register only after POWER_EVENT is accepted: the SCM drops a power
    // event before that, and Windows sends the current display state at
    // registration.
    // SAFETY: the status handle stays valid while the service runs; the
    // registration ends with the process.
    if let Err(e) = unsafe {
        RegisterPowerSettingNotification(
            HANDLE(handle.as_raw_handle()),
            &GUID_CONSOLE_DISPLAY_STATE,
            DEVICE_NOTIFY_SERVICE_HANDLE,
        )
    } {
        warn!("no display state: RegisterPowerSettingNotification failed: {e}");
    }

    let config = CONFIG
        .get()
        .cloned()
        .unwrap_or_else(|| PathBuf::from(crate::config::DEFAULT_PATH));
    let result = crate::run(config, || {
        Ok(async move {
            stop_control(rx).await;
            if let Err(e) = handle.set_service_status(status(
                ServiceState::StopPending,
                ServiceControlAccept::empty(),
                0,
            )) {
                warn!(error = %e, "cannot report StopPending");
            }
        })
    });
    let exit = if result.is_ok() { 0 } else { 1 };
    handle.set_service_status(status(
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        exit,
    ))?;
    result
}

/// Completes at the first `Stop` or `Preshutdown`.
async fn stop_control(mut controls: UnboundedReceiver<ServiceControl>) {
    while let Some(control) = controls.recv().await {
        match control {
            ServiceControl::Stop => {
                info!("service stop received");
                return;
            }
            ServiceControl::Preshutdown => {
                info!("preshutdown received");
                return;
            }
            // W2 and W3 turn the session and power controls into events.
            other => debug!(control = ?other, "control not handled yet"),
        }
    }
}
