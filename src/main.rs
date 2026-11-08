mod config;
mod core;
mod entities;
#[cfg(any(windows, test))]
mod logfile;
mod mqtt;
mod sources;
mod time;

use std::future::Future;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, bail};
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::core::Core;

#[cfg(not(windows))]
const USAGE: &str = "\
Usage: pc-agent [--config <path>]
       pc-agent diagnose

Publishes the state of this PC to Home Assistant over MQTT.

Commands:
  diagnose         Print what each source reads and what the agent makes of
                   it, once, then exit (Linux; run it with sudo). Needs no config.

Options:
  --config <path>  Config file (default: /etc/pc-agent/config.toml)
  -h, --help       Print this help and exit

Log level: RUST_LOG (default: info)";

#[cfg(windows)]
const USAGE: &str = "\
Usage: pc-agent [--config <path>]
       pc-agent install [--config <path>]
       pc-agent uninstall
       pc-agent diagnose [--config <path>]

Publishes the state of this PC to Home Assistant over MQTT. The Windows
service pc-agent runs it; in a console it runs until Ctrl+C.

Commands:
  install          Create the service pc-agent (run as administrator) and
                   C:\\ProgramData\\pc-agent. With --config, copy that file to
                   C:\\ProgramData\\pc-agent\\config.toml; without it, write the
                   example configuration there if no config.toml exists.
  uninstall        Stop and delete the service. The configuration stays.
  diagnose         Print what each Windows source reads, once, then exit. It
                   publishes nothing: the MQTT check only connects and
                   disconnects. Run it as SYSTEM for the helper start test.

Options:
  --config <path>  Config file (default: C:\\ProgramData\\pc-agent\\config.toml)
  -h, --help       Print this help and exit

Log level: RUST_LOG (default: info)";

#[derive(Debug, PartialEq)]
enum Command {
    Run {
        config: PathBuf,
    },
    Diagnose {
        config: Option<PathBuf>,
    },
    #[cfg(windows)]
    Install {
        config: Option<PathBuf>,
    },
    #[cfg(windows)]
    Uninstall,
    /// `session-helper --probe` (not in the help): one read in the user
    /// session, for `diagnose`.
    #[cfg(windows)]
    SessionHelperProbe,
    /// `session-helper [--pipe <name>]` (not in the help): the helper that
    /// the service starts in a user session. `--pipe` is for the CI test.
    #[cfg(windows)]
    SessionHelper {
        pipe: Option<String>,
    },
    Help,
}

fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Result<Command> {
    let mut config = PathBuf::from(config::DEFAULT_PATH);
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            #[cfg(not(windows))]
            "diagnose" => {
                if let Some(other) = args.next() {
                    bail!("unknown argument {other:?} after diagnose");
                }
                return Ok(Command::Diagnose { config: None });
            }
            #[cfg(windows)]
            "diagnose" => {
                let config = config_arg(&mut args, "diagnose")?;
                return Ok(Command::Diagnose { config });
            }
            #[cfg(windows)]
            "install" => {
                let config = config_arg(&mut args, "install")?;
                return Ok(Command::Install { config });
            }
            #[cfg(windows)]
            "session-helper" => {
                let command = match (args.next().as_deref(), args.next()) {
                    (None, _) => Command::SessionHelper { pipe: None },
                    (Some("--probe"), None) => Command::SessionHelperProbe,
                    (Some("--probe"), Some(other)) => {
                        bail!("unknown argument {other:?} after session-helper --probe")
                    }
                    (Some("--pipe"), Some(name)) => Command::SessionHelper { pipe: Some(name) },
                    (Some("--pipe"), None) => bail!("--pipe needs a name"),
                    (Some(other), _) => bail!("unknown argument {other:?} after session-helper"),
                };
                if let Some(other) = args.next() {
                    bail!("unknown argument {other:?} after session-helper");
                }
                return Ok(command);
            }
            #[cfg(windows)]
            "uninstall" => {
                if let Some(other) = args.next() {
                    bail!("unknown argument {other:?} after uninstall");
                }
                return Ok(Command::Uninstall);
            }
            "--config" => match args.next() {
                Some(path) => config = PathBuf::from(path),
                None => bail!("--config needs a path"),
            },
            other => bail!("unknown argument {other:?}"),
        }
    }
    Ok(Command::Run { config })
}

/// The optional `--config <path>` after `command`, and nothing after it.
#[cfg(windows)]
fn config_arg(args: &mut impl Iterator<Item = String>, command: &str) -> Result<Option<PathBuf>> {
    let config = match (args.next().as_deref(), args.next()) {
        (None, _) => None,
        (Some("--config"), Some(path)) => Some(PathBuf::from(path)),
        (Some("--config"), None) => bail!("--config needs a path"),
        (Some(other), _) => bail!("unknown argument {other:?} after {command}"),
    };
    if let Some(other) = args.next() {
        bail!("unknown argument {other:?} after {command}");
    }
    Ok(config)
}

fn main() -> ExitCode {
    let config_path = match parse_args(std::env::args().skip(1)) {
        Ok(Command::Help) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(Command::Diagnose { config }) => return diagnose(config),
        #[cfg(windows)]
        Ok(Command::SessionHelperProbe) => return sources::probe(),
        #[cfg(windows)]
        Ok(Command::SessionHelper { pipe }) => return sources::session_helper(pipe.as_deref()),
        #[cfg(windows)]
        Ok(Command::Install { config }) => return sources::install(config.as_deref()),
        #[cfg(windows)]
        Ok(Command::Uninstall) => return sources::uninstall(),
        Ok(Command::Run { config }) => config,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    // Under the service control manager, the service runs the agent.
    #[cfg(windows)]
    if let Some(code) = sources::run_as_service(&config_path) {
        return code;
    }

    tracing_subscriber::fmt()
        .with_env_filter(env_filter())
        .init();

    match run(config_path, stop_signal) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

/// `pc-agent diagnose`: runs the checks of the OS source once. Only the
/// Windows checks read the configuration (for the MQTT check).
fn diagnose(config: Option<PathBuf>) -> ExitCode {
    match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => {
            #[cfg(windows)]
            runtime.block_on(sources::diagnose(config.as_deref()));
            #[cfg(not(windows))]
            {
                let _ = config;
                runtime.block_on(sources::diagnose());
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: cannot start the runtime: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The log level from `RUST_LOG`, `info` by default.
fn env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
}

/// The stop of a console or systemd/launchd run: SIGTERM or SIGINT.
#[cfg(unix)]
fn stop_signal() -> Result<impl Future<Output = ()>> {
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    Ok(async move {
        tokio::select! {
            _ = sigterm.recv() => info!("SIGTERM received"),
            _ = sigint.recv() => info!("SIGINT received"),
        }
    })
}

/// The stop of a console run on Windows: Ctrl+C.
#[cfg(windows)]
fn stop_signal() -> Result<impl Future<Output = ()>> {
    let mut ctrl_c = tokio::signal::windows::ctrl_c()?;
    Ok(async move {
        ctrl_c.recv().await;
        info!("Ctrl+C received");
    })
}

/// Runs the agent until the future that `stop` gives completes. `stop` is
/// called inside the runtime.
fn run<S, F>(config_path: PathBuf, stop: S) -> Result<()>
where
    S: FnOnce() -> Result<F>,
    F: Future<Output = ()>,
{
    let cfg = Config::load(&config_path)?;
    info!(
        config = %config_path.display(),
        device = %cfg.device.name,
        entity_prefix = %cfg.device.entity_prefix,
        gpu = cfg.gpu.enabled,
        version = env!("CARGO_PKG_VERSION"),
        "starting pc-agent"
    );

    let model = sources::model();
    let gpu = cfg.gpu.enabled;
    let core = Core::new(sources::supplied(gpu), sources::boot_time());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let stop = stop()?;
        let poll = move || sources::poll(gpu);
        let events = sources::events();
        mqtt::run(&cfg, &model, core, poll, events, sources::ACK_WAIT, stop).await
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[cfg(not(windows))]
    #[test]
    fn default_config_path() {
        assert_eq!(
            parse_args(args(&[])).unwrap(),
            Command::Run {
                config: PathBuf::from("/etc/pc-agent/config.toml")
            }
        );
    }

    #[test]
    fn config_flag_sets_path() {
        assert_eq!(
            parse_args(args(&["--config", "/tmp/c.toml"])).unwrap(),
            Command::Run {
                config: PathBuf::from("/tmp/c.toml")
            }
        );
    }

    #[test]
    fn help_flag() {
        assert_eq!(parse_args(args(&["--help"])).unwrap(), Command::Help);
        assert_eq!(parse_args(args(&["-h"])).unwrap(), Command::Help);
    }

    #[test]
    fn diagnose_command() {
        assert_eq!(
            parse_args(args(&["diagnose"])).unwrap(),
            Command::Diagnose { config: None }
        );
        assert!(parse_args(args(&["diagnose", "--bogus"])).is_err());
    }

    #[cfg(not(windows))]
    #[test]
    fn diagnose_takes_no_config_outside_windows() {
        assert!(parse_args(args(&["diagnose", "--config", "/tmp/c.toml"])).is_err());
        assert!(parse_args(args(&["session-helper", "--probe"])).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn diagnose_takes_a_config_on_windows() {
        assert_eq!(
            parse_args(args(&["diagnose", "--config", r"D:\ci.toml"])).unwrap(),
            Command::Diagnose {
                config: Some(PathBuf::from(r"D:\ci.toml"))
            }
        );
        assert!(parse_args(args(&["diagnose", "--config"])).is_err());
        assert!(parse_args(args(&["diagnose", "--config", "a", "b"])).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn session_helper_commands() {
        assert_eq!(
            parse_args(args(&["session-helper", "--probe"])).unwrap(),
            Command::SessionHelperProbe
        );
        assert!(parse_args(args(&["session-helper", "--probe", "x"])).is_err());
        assert_eq!(
            parse_args(args(&["session-helper"])).unwrap(),
            Command::SessionHelper { pipe: None }
        );
        assert_eq!(
            parse_args(args(&["session-helper", "--pipe", r"\\.\pipe\t"])).unwrap(),
            Command::SessionHelper {
                pipe: Some(r"\\.\pipe\t".to_string())
            }
        );
        assert!(parse_args(args(&["session-helper", "--pipe"])).is_err());
        assert!(parse_args(args(&["session-helper", "--pipe", "a", "b"])).is_err());
        assert!(parse_args(args(&["session-helper", "--bogus"])).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn default_config_path_is_in_programdata() {
        assert_eq!(
            parse_args(args(&[])).unwrap(),
            Command::Run {
                config: PathBuf::from(r"C:\ProgramData\pc-agent\config.toml")
            }
        );
    }

    #[cfg(windows)]
    #[test]
    fn install_and_uninstall_commands() {
        assert_eq!(
            parse_args(args(&["install"])).unwrap(),
            Command::Install { config: None }
        );
        assert_eq!(
            parse_args(args(&["install", "--config", r"D:\ci.toml"])).unwrap(),
            Command::Install {
                config: Some(PathBuf::from(r"D:\ci.toml"))
            }
        );
        assert!(parse_args(args(&["install", "--config"])).is_err());
        assert!(parse_args(args(&["install", "--bogus"])).is_err());
        assert!(parse_args(args(&["install", "--config", "a", "b"])).is_err());
        assert_eq!(
            parse_args(args(&["uninstall"])).unwrap(),
            Command::Uninstall
        );
        assert!(parse_args(args(&["uninstall", "now"])).is_err());
    }

    #[cfg(not(windows))]
    #[test]
    fn install_is_for_windows_only() {
        assert!(parse_args(args(&["install"])).is_err());
        assert!(parse_args(args(&["uninstall"])).is_err());
    }

    #[test]
    fn bad_arguments_are_errors() {
        assert!(parse_args(args(&["--config"])).is_err());
        assert!(parse_args(args(&["--bogus"])).is_err());
    }
}
