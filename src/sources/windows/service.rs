//! The service frame: the dispatcher, the control handler and the service
//! status. The handler forwards each control to the runtime and returns at
//! once, with one exception: for `PBT_APMSUSPEND` it sends `Suspend` and
//! blocks until the reply, at most the ack wait, because Windows goes on into
//! sleep when the handler returns. `Stop` stops the agent: it publishes the
//! retained `offline` and disconnects. `Preshutdown` first sends `Shutdown`
//! and waits for the reply. The handler records each console display
//! notification and each user presence notification for the poll. The
//! service starts the session helpers and their pipe.
//!
//! The pending-file check of the self-update is the first step of the
//! service entry. A restart (the rollback of an update) reports `Stopped`
//! with `RESTART_EXIT_CODE`; the failure action on non-crash failures (set
//! by `install` and at each start) starts the service again after 5 s.

use std::ffi::OsString;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use ::windows::Win32::Foundation::{ERROR_FAILED_SERVICE_CONTROLLER_CONNECT, HANDLE};
use ::windows::Win32::System::Power::RegisterPowerSettingNotification;
use ::windows::Win32::System::SystemServices::{
    GUID_CONSOLE_DISPLAY_STATE, GUID_GLOBAL_USER_PRESENCE,
};
use ::windows::Win32::UI::WindowsAndMessaging::DEVICE_NOTIFY_SERVICE_HANDLE;
use anyhow::Result;
use tokio::sync::mpsc::{self, UnboundedReceiver, unbounded_channel};
use tracing::{error, info, warn};
use windows_service::service::{
    PowerBroadcastSetting, PowerEventParam, ServiceControl, ServiceControlAccept, ServiceExitCode,
    ServiceState, ServiceStatus, ServiceType, UserStatus,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

use super::install::restart_on_non_crash_failures;
use super::{DATA_DIR, DISPLAY, EVENTS, SERVICE_NAME, record_presence, start_helpers};
use crate::core::SystemEvent;
use crate::logfile::LogFile;
use crate::mqtt::Exit;
use crate::sources::ACK_WAIT;
use crate::sources::wts_rules::{self, ControlAction};

/// The size limit of the log file; one old file is kept.
const LOG_LIMIT: u64 = 1024 * 1024;

/// The controls that the service accepts.
const ACCEPTED: ServiceControlAccept = ServiceControlAccept::STOP
    .union(ServiceControlAccept::PRESHUTDOWN)
    .union(ServiceControlAccept::SESSION_CHANGE)
    .union(ServiceControlAccept::POWER_EVENT);

/// The time that the stop may take: the ack wait of `offline`, the
/// disconnect and some margin.
const STOP_WAIT_HINT: Duration = Duration::from_secs(5);

/// The service-specific exit code of a restart: not 0, so the failure
/// action of the service starts it again.
const RESTART_EXIT_CODE: u32 = 10;

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
    // The first step of the service path: before the configuration, the
    // log and the OS init.
    let (start, watch) = crate::update::service_start();
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
    crate::update::log_start(&start);
    if start.must_exit() {
        stop_at_once(RESTART_EXIT_CODE);
        return;
    }
    if crate::update::TEST_EXIT {
        warn!("update-test-exit: exit after the pending-file check");
        stop_at_once(1);
        return;
    }
    if let Err(e) = run_service(watch) {
        error!("{e:#}");
    }
}

/// Reports `Stopped` with `exit` before the agent runs.
fn stop_at_once(exit: u32) {
    let result = service_control_handler::register(SERVICE_NAME, |_| {
        ServiceControlHandlerResult::NotImplemented
    })
    .and_then(|handle| {
        handle.set_service_status(status(
            ServiceState::Stopped,
            ServiceControlAccept::empty(),
            exit,
        ))
    });
    if let Err(e) = result {
        error!("cannot report Stopped: {e}");
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

/// The raw codes of a control for `wts_rules::control_action`: the
/// `SERVICE_CONTROL_*` code and the `WTS_*` reason or the `PBT_*` event.
fn raw_codes(control: &ServiceControl) -> (u32, u32) {
    use ::windows::Win32::UI::WindowsAndMessaging as wm;
    let event_type = match control {
        ServiceControl::SessionChange(param) => param.reason.to_raw(),
        ServiceControl::PowerEvent(event) => match event {
            PowerEventParam::PowerStatusChange => wm::PBT_APMPOWERSTATUSCHANGE,
            PowerEventParam::ResumeAutomatic => wm::PBT_APMRESUMEAUTOMATIC,
            PowerEventParam::ResumeSuspend => wm::PBT_APMRESUMESUSPEND,
            PowerEventParam::Suspend => wm::PBT_APMSUSPEND,
            PowerEventParam::PowerSettingChange(_) => wm::PBT_POWERSETTINGCHANGE,
            PowerEventParam::BatteryLow => wm::PBT_APMBATTERYLOW,
            PowerEventParam::OemEvent => wm::PBT_APMOEMEVENT,
            PowerEventParam::QuerySuspend => wm::PBT_APMQUERYSUSPEND,
            PowerEventParam::QuerySuspendFailed => wm::PBT_APMQUERYSUSPENDFAILED,
            PowerEventParam::ResumeCritical => wm::PBT_APMRESUMECRITICAL,
            // A variant of a later windows-service: no PBT event that the rules act on.
            _ => u32::MAX,
        },
        _ => 0,
    };
    (control.raw_service_control_type(), event_type)
}

/// Sends an event of a control to the runtime without a wait.
fn forward(events: &mpsc::Sender<SystemEvent>, event: SystemEvent) {
    if let Err(e) = events.try_send(event) {
        warn!(error = %e, "cannot forward a control event");
    }
}

fn run_service(watch: crate::update::Watch) -> Result<()> {
    let (tx, rx) = unbounded_channel();
    let (events_tx, events_rx) = mpsc::channel(8);
    if let Ok(mut slot) = EVENTS.lock() {
        *slot = Some(events_rx);
    }
    // The handler runs on a thread of the service control manager, outside
    // the runtime of the agent: this runtime gives the timer of the suspend wait.
    let waiter = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    let handler_events = events_tx.clone();
    let handler = move |control: ServiceControl| {
        match &control {
            ServiceControl::PowerEvent(PowerEventParam::PowerSettingChange(
                PowerBroadcastSetting::ConsoleDisplayState(state),
            )) => DISPLAY.record(state.to_raw()),
            ServiceControl::PowerEvent(PowerEventParam::PowerSettingChange(
                PowerBroadcastSetting::GlobalUserPresence(status),
            )) => record_presence(*status == UserStatus::Present),
            _ => {}
        }
        let (raw, event_type) = raw_codes(&control);
        match wts_rules::control_action(raw, event_type) {
            // The runtime is gone after the stop: nothing to forward to then.
            ControlAction::Stop | ControlAction::ShutdownThenStop => {
                let _ = tx.send(control);
            }
            ControlAction::Suspend => {
                info!("system will sleep");
                waiter.block_on(wts_rules::suspend(&handler_events, ACK_WAIT));
            }
            ControlAction::Resume => {
                info!("system resumed");
                forward(&handler_events, SystemEvent::Resume);
            }
            ControlAction::SessionChange => forward(&handler_events, SystemEvent::SessionChange),
            ControlAction::Ignore => {}
        }
        match control {
            ServiceControl::Interrogate
            | ServiceControl::Stop
            | ServiceControl::Preshutdown
            | ServiceControl::SessionChange(_)
            | ServiceControl::PowerEvent(_) => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };
    let handle = service_control_handler::register(SERVICE_NAME, handler)?;
    handle.set_service_status(status(ServiceState::Running, ACCEPTED, 0))?;
    // At each start, so a service installed before the self-update gets it
    // with its first updated start.
    if let Err(e) = restart_on_non_crash_failures() {
        warn!("cannot set the failure actions on non-crash failures: {e:#}");
    }
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
    // The fallback of lastactive while no helper of the console session is
    // connected.
    // SAFETY: as above.
    if let Err(e) = unsafe {
        RegisterPowerSettingNotification(
            HANDLE(handle.as_raw_handle()),
            &GUID_GLOBAL_USER_PRESENCE,
            DEVICE_NOTIFY_SERVICE_HANDLE,
        )
    } {
        warn!("no user presence: RegisterPowerSettingNotification failed: {e}");
    }

    let config = CONFIG
        .get()
        .cloned()
        .unwrap_or_else(|| PathBuf::from(crate::config::DEFAULT_PATH));
    let result = crate::run(
        config,
        || {
            // Inside the runtime: the pipe server runs on it.
            start_helpers();
            Ok(async move {
                let preshutdown = stop_control(rx).await;
                // windows-service frees the handler closure after Stop and
                // Preshutdown, so accept no control from here on, before the
                // shutdown wait below.
                if let Err(e) = handle.set_service_status(status(
                    ServiceState::StopPending,
                    ServiceControlAccept::empty(),
                    0,
                )) {
                    warn!(error = %e, "cannot report StopPending");
                }
                if preshutdown {
                    wts_rules::shutdown(&events_tx).await;
                }
            })
        },
        watch,
    );
    let exit = match result {
        Ok(Exit::Stopped) => 0,
        Ok(Exit::Restart) => RESTART_EXIT_CODE,
        Err(_) => 1,
    };
    handle.set_service_status(status(
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        exit,
    ))?;
    result.map(|_| ())
}

/// Completes at the first `Stop` or `Preshutdown`; true for `Preshutdown`.
async fn stop_control(mut controls: UnboundedReceiver<ServiceControl>) -> bool {
    // The handler sends only Stop and Preshutdown here.
    match controls.recv().await {
        Some(ServiceControl::Preshutdown) => {
            info!("preshutdown received");
            true
        }
        _ => {
            info!("service stop received");
            false
        }
    }
}
