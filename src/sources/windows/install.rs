//! `pc-agent install` and `pc-agent uninstall`.

use std::ffi::OsString;
use std::path::Path;
use std::process::ExitCode;
use std::thread::sleep;
use std::time::{Duration, Instant};

use ::windows::Win32::Foundation::{HLOCAL, LocalFree};
use ::windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1, SE_FILE_OBJECT,
    SetNamedSecurityInfoW,
};
use ::windows::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR,
};
use ::windows::core::{BOOL, HSTRING, w};
use anyhow::{Context, Result};
use windows_service::service::{
    ServiceAccess, ServiceAction, ServiceActionType, ServiceErrorControl, ServiceFailureActions,
    ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState, ServiceType,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use super::{DATA_DIR, SERVICE_NAME};

const DESCRIPTION: &str = "Publishes the state of this PC to Home Assistant over MQTT.";

/// The example configuration that `install` writes when no config.toml exists.
const EXAMPLE_CONFIG: &str = include_str!("../../../packaging/config.example.toml");

/// The time that `uninstall` waits for the service to stop.
const STOP_TIMEOUT: Duration = Duration::from_secs(20);

/// `pc-agent install [--config <path>]`.
pub fn install(config: Option<&Path>) -> ExitCode {
    match try_install(config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// `pc-agent uninstall`.
pub fn uninstall() -> ExitCode {
    match try_uninstall() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn try_install(config: Option<&Path>) -> Result<()> {
    let dir = Path::new(DATA_DIR);
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {DATA_DIR}"))?;
    protect(dir).with_context(|| format!("cannot set the access list of {DATA_DIR}"))?;

    // A new file inherits the protected access list of the folder.
    let target = dir.join("config.toml");
    match config {
        Some(source) => {
            let text = std::fs::read_to_string(source)
                .with_context(|| format!("cannot read {}", source.display()))?;
            std::fs::write(&target, text)
                .with_context(|| format!("cannot write {}", target.display()))?;
            println!("copied {} to {}", source.display(), target.display());
        }
        None if !target.exists() => {
            std::fs::write(&target, EXAMPLE_CONFIG)
                .with_context(|| format!("cannot write {}", target.display()))?;
            println!(
                "wrote the example configuration to {}: edit it before you start the service",
                target.display()
            );
        }
        None => println!("kept the configuration {}", target.display()),
    }

    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .context("cannot open the service manager (run as administrator)")?;
    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(SERVICE_NAME),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: std::env::current_exe().context("cannot find the path of pc-agent")?,
        launch_arguments: vec![],
        dependencies: vec![],
        // LocalSystem.
        account_name: None,
        account_password: None,
    };
    let service = manager
        // A Restart failure action needs SERVICE_START on the handle.
        .create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START)
        .with_context(|| format!("cannot create the service {SERVICE_NAME}"))?;
    service
        .set_description(DESCRIPTION)
        .context("cannot set the service description")?;
    let restart = ServiceAction {
        action_type: ServiceActionType::Restart,
        delay: Duration::from_secs(5),
    };
    service
        .update_failure_actions(ServiceFailureActions {
            reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(86_400)),
            reboot_msg: None,
            command: None,
            actions: Some(vec![restart.clone(), restart.clone(), restart]),
        })
        .context("cannot set the failure actions")?;
    service
        .set_preshutdown_timeout(Duration::from_secs(10))
        .context("cannot set the preshutdown timeout")?;
    println!("created the service {SERVICE_NAME} (LocalSystem, automatic start)");
    Ok(())
}

/// Sets the protected access list of the spec on `dir`: full access for
/// SYSTEM and Administrators only, inherited by the files and folders in it.
fn protect(dir: &Path) -> Result<()> {
    let path = HSTRING::from(dir.as_os_str());
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: the SDDL string is a static wide string; `sd` receives a buffer
    // that LocalFree releases below, after its last use.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )?;
        let mut present = BOOL(0);
        let mut defaulted = BOOL(0);
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let result = GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted)
            .and_then(|()| {
                SetNamedSecurityInfoW(
                    &path,
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    None,
                    None,
                    Some(dacl),
                    None,
                )
                .ok()
            });
        LocalFree(Some(HLOCAL(sd.0)));
        result?;
    }
    Ok(())
}

fn try_uninstall() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("cannot open the service manager (run as administrator)")?;
    let service = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
        )
        .with_context(|| format!("cannot open the service {SERVICE_NAME}"))?;

    // Stop first, so the agent publishes `offline` before the delete.
    if service.query_status()?.current_state != ServiceState::Stopped {
        service.stop().context("cannot stop the service")?;
        let start = Instant::now();
        while service.query_status()?.current_state != ServiceState::Stopped {
            if start.elapsed() > STOP_TIMEOUT {
                anyhow::bail!(
                    "the service did not stop within {} s",
                    STOP_TIMEOUT.as_secs()
                );
            }
            sleep(Duration::from_millis(250));
        }
    }
    service.delete().context("cannot delete the service")?;
    println!("deleted the service {SERVICE_NAME}; the configuration in {DATA_DIR} stays");
    Ok(())
}
