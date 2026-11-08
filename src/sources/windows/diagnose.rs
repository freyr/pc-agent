//! `pc-agent diagnose` on Windows: prints what each Windows source reads,
//! once. It publishes nothing: the MQTT check connects and disconnects
//! only, with its own client ID and no last will. It never prints the MQTT
//! password. The formatting is in `win_diagnose`.

use std::path::Path;
use std::time::Duration;

use ::windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use ::windows::core::w;
use rumqttc::{AsyncClient, Event, Incoming, Outgoing};
use tokio::time::timeout;

use super::super::nvml;
use super::super::win_diagnose::{self as fmt, MqttOutcome};
use super::super::wts_rules::NO_CONSOLE;
use super::install::reg_dword;
use super::{helper, wts};
use crate::config::{self, Config};
use crate::mqtt;

/// The longest wait for the ConnAck of the broker.
const CONNACK_WAIT: Duration = Duration::from_secs(5);
/// The longest wait for the DISCONNECT to go out.
const DISCONNECT_WAIT: Duration = Duration::from_millis(500);

/// Runs the checks once and prints the result to stdout. `config` is the
/// file of `--config`, else the default path.
pub async fn run(config: Option<&Path>) {
    println!("== Version");
    println!("  pc-agent {}", env!("CARGO_PKG_VERSION"));

    println!("== Console session");
    // SAFETY: WTSGetActiveConsoleSessionId has no arguments and no preconditions.
    let console = unsafe { WTSGetActiveConsoleSessionId() };
    let session = if console == NO_CONSOLE {
        None
    } else {
        wts::session(console)
    };
    print!("{}", fmt::format_console(console, session.as_ref()));

    println!("== Logged-in users");
    print!("{}", fmt::format_users(wts::logged_users().as_deref()));

    println!("== NVML");
    print!("{}", fmt::format_nvml(&nvml::check()));

    println!("== Fast Startup");
    let hiberboot = reg_dword(
        w!(r"SYSTEM\CurrentControlSet\Control\Session Manager\Power"),
        w!("HiberbootEnabled"),
    );
    let hibernate = reg_dword(
        w!(r"SYSTEM\CurrentControlSet\Control\Power"),
        w!("HibernateEnabled"),
    );
    print!("{}", fmt::format_fast_startup(hiberboot, hibernate));

    println!("== Sleep states (powercfg /a)");
    let powercfg = std::process::Command::new("powercfg")
        .arg("/a")
        .output()
        .map_err(|e| e.to_string())
        .and_then(|out| {
            if out.status.success() {
                Ok(String::from_utf8_lossy(&out.stdout).into_owned())
            } else {
                Err(format!(
                    "{}: {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                ))
            }
        });
    print!("{}", fmt::format_powercfg(&powercfg));

    println!("== Smart App Control");
    let sac = reg_dword(
        w!(r"SYSTEM\CurrentControlSet\Control\CI\Policy"),
        w!("VerifiedAndReputablePolicyState"),
    );
    print!("{}", fmt::format_smart_app_control(sac));

    println!("== Helper start");
    print!("{}", fmt::format_helper(&helper::start_probe()));

    println!("== MQTT");
    let path = config.unwrap_or(Path::new(config::DEFAULT_PATH));
    print!("{}", fmt::format_mqtt(&check_mqtt(path).await));

    println!("{}", fmt::END_MARKER);
}

/// Connects with the diagnose options, waits for the ConnAck and
/// disconnects. Nothing is published or subscribed.
async fn check_mqtt(path: &Path) -> MqttOutcome {
    if !path.exists() {
        return MqttOutcome::NoConfig(path.display().to_string());
    }
    let cfg = match Config::load(path) {
        Ok(cfg) => cfg,
        Err(e) => return MqttOutcome::BadConfig(config_error(&e)),
    };
    let opts = mqtt::diagnose_options(&cfg);
    let client_id = opts.client_id();
    let (client, mut eventloop) = AsyncClient::new(opts, 10);
    let connected = timeout(CONNACK_WAIT, async {
        loop {
            match eventloop.poll().await {
                Ok(Event::Incoming(Incoming::ConnAck(_))) => return Ok(()),
                Ok(_) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
    })
    .await
    .unwrap_or_else(|_| Err(format!("no ConnAck within {} s", CONNACK_WAIT.as_secs())));
    let (host, port) = (cfg.mqtt.host.clone(), cfg.mqtt.port);
    if let Err(error) = connected {
        return MqttOutcome::Failed { host, port, error };
    }
    let _ = client.disconnect().await;
    let _ = timeout(DISCONNECT_WAIT, async {
        loop {
            match eventloop.poll().await {
                Ok(Event::Outgoing(Outgoing::Disconnect)) | Err(_) => return,
                Ok(_) => {}
            }
        }
    })
    .await;
    MqttOutcome::Connected {
        host,
        port,
        client_id,
    }
}

/// The error of a config load with its cause, but never the toml source
/// snippet: the quoted line can hold the password.
fn config_error(e: &anyhow::Error) -> String {
    let mut text = e.to_string();
    for cause in e.chain().skip(1) {
        if let Some(toml) = cause.downcast_ref::<toml::de::Error>() {
            text.push_str(&format!(": {}", toml.message()));
        } else {
            text.push_str(&format!(": {cause}"));
        }
    }
    text
}
