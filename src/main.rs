mod config;
mod core;
mod entities;
#[cfg(any(windows, test))]
mod logfile;
mod mqtt;
mod sources;
mod time;
mod update;

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
       pc-agent update [--check] [--channel stable|edge]

Publishes the state of this PC to Home Assistant over MQTT.

Commands:
  diagnose         Print what each source reads and what the agent makes of
                   it, once, then exit (Linux; run it with sudo). Needs no config.
  update           Check the release channel, install a newer signed build
                   in place of this binary (also a build that failed
                   before), and restart the service pc-agent. Run it with
                   sudo, from the installed binary.
  update --check   Check the release channel for a newer signed build and
                   print the result. Installs nothing; needs no root. The
                   channel is --channel, else [update] channel of the config
                   file, else stable.

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
       pc-agent update [--check] [--channel stable|edge] [--config <path>]

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
  update           Check the release channel, install a newer signed build
                   in place of this exe (also a build that failed before),
                   and restart the service pc-agent. Run it as administrator,
                   from the installed exe.
  update --check   Check the release channel for a newer signed build and
                   print the result. Installs nothing; needs no admin. The
                   channel is --channel, else [update] channel of the config
                   file, else stable.

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
    /// `update --verify <manifest> <signature> <public key file>` (not in
    /// the help): checks a signed release manifest, for CI.
    UpdateVerify {
        manifest: PathBuf,
        signature: PathBuf,
        public_key: PathBuf,
    },
    /// `update --check [--channel stable|edge]` (Windows: also
    /// `[--config <path>]`).
    UpdateCheck {
        channel: Option<update::Channel>,
        config: Option<PathBuf>,
    },
    /// `update [--channel stable|edge]` (Windows: also `[--config <path>]`):
    /// check and install, then restart the service.
    Update {
        channel: Option<update::Channel>,
        config: Option<PathBuf>,
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
            "update" => return update_args(args),
            "--config" => match args.next() {
                Some(path) => config = PathBuf::from(path),
                None => bail!("--config needs a path"),
            },
            other => bail!("unknown argument {other:?}"),
        }
    }
    Ok(Command::Run { config })
}

/// The arguments after `update`: `--verify <manifest> <signature> <public
/// key file>`, or `[--check] [--channel stable|edge]` (Windows: also
/// `[--config <path>]`) in any order. Without `--check`, `update` installs.
fn update_args(mut args: impl Iterator<Item = String>) -> Result<Command> {
    let first = args.next();
    if first.as_deref() == Some("--verify") {
        return match (args.next(), args.next(), args.next(), args.next()) {
            (_, _, _, Some(other)) => bail!("unknown argument {other:?} after update"),
            (Some(manifest), Some(signature), Some(public_key), None) => {
                Ok(Command::UpdateVerify {
                    manifest: PathBuf::from(manifest),
                    signature: PathBuf::from(signature),
                    public_key: PathBuf::from(public_key),
                })
            }
            _ => bail!("--verify needs <manifest> <signature> <public key file>"),
        };
    }
    let mut check = false;
    let mut channel = None;
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut config = None;
    let mut next = first;
    while let Some(arg) = next {
        match arg.as_str() {
            "--check" if !check => check = true,
            "--channel" if channel.is_none() => {
                let Some(name) = args.next() else {
                    bail!("--channel needs stable or edge")
                };
                channel = Some(name.parse()?);
            }
            #[cfg(windows)]
            "--config" if config.is_none() => {
                let Some(path) = args.next() else {
                    bail!("--config needs a path")
                };
                config = Some(PathBuf::from(path));
            }
            other => bail!("unknown argument {other:?} after update"),
        }
        next = args.next();
    }
    Ok(if check {
        Command::UpdateCheck { channel, config }
    } else {
        Command::Update { channel, config }
    })
}

/// The pending-file check of the self-update is the first step of the
/// service path: on Linux and macOS the run with no subcommand, on Windows
/// the service entry (`sources::run_as_service`). A CLI run never reads or
/// changes the start count.
fn pending_check_applies(command: &Command) -> bool {
    cfg!(not(windows)) && matches!(command, Command::Run { .. })
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
    let command = match parse_args(std::env::args().skip(1)) {
        Ok(command) => command,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    // The first step of the service path, before the configuration, the
    // log and the OS init.
    let start = pending_check_applies(&command).then(update::service_start);
    let config_path = match command {
        Command::Help => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Command::Diagnose { config } => return diagnose(config),
        Command::UpdateVerify {
            manifest,
            signature,
            public_key,
        } => return update::verify_files(&manifest, &signature, &public_key),
        Command::UpdateCheck { channel, config } => {
            let config = config.unwrap_or_else(|| PathBuf::from(config::DEFAULT_PATH));
            return update::check_command(channel, &config);
        }
        Command::Update { channel, config } => {
            let config = config.unwrap_or_else(|| PathBuf::from(config::DEFAULT_PATH));
            return update::update_command(channel, &config);
        }
        #[cfg(windows)]
        Command::SessionHelperProbe => return sources::probe(),
        #[cfg(windows)]
        Command::SessionHelper { pipe } => return sources::session_helper(pipe.as_deref()),
        #[cfg(windows)]
        Command::Install { config } => return sources::install(config.as_deref()),
        #[cfg(windows)]
        Command::Uninstall => return sources::uninstall(),
        Command::Run { config } => config,
    };

    // Under the service control manager, the service runs the agent.
    #[cfg(windows)]
    if let Some(code) = sources::run_as_service(&config_path) {
        return code;
    }

    tracing_subscriber::fmt()
        .with_env_filter(env_filter())
        .init();

    let watch = match start {
        Some((start, watch)) => {
            update::log_start(&start);
            // The restart path: systemd (`Restart=always`) and launchd
            // (`KeepAlive`) start the binary at the binary path again.
            if start.must_exit() {
                return ExitCode::SUCCESS;
            }
            if update::TEST_EXIT {
                tracing::warn!("update-test-exit: exit after the pending-file check");
                return ExitCode::FAILURE;
            }
            watch
        }
        None => update::Watch::none(),
    };

    // Exit 0 also for a restart: the service manager starts the binary again.
    match run(config_path, stop_signal, watch) {
        Ok(_) => ExitCode::SUCCESS,
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

/// Runs the agent until the future that `stop` gives completes, or until
/// `watch` ends the run for a restart. `stop` is called inside the runtime.
fn run<S, F>(config_path: PathBuf, stop: S, mut watch: update::Watch) -> Result<mqtt::Exit>
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
        version = update::VERSION,
        update_auto = cfg.update.auto,
        update_channel = %cfg.update.channel,
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
        // In the runtime: the schedule starts on its clock.
        let updater = update::Updater::system(&cfg.update);
        mqtt::run(
            &cfg,
            &model,
            core,
            poll,
            events,
            sources::ACK_WAIT,
            stop,
            &mut watch,
            updater,
        )
        .await
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
    fn update_verify_command() {
        assert_eq!(
            parse_args(args(&[
                "update",
                "--verify",
                "m.json",
                "m.json.minisig",
                "k.pub"
            ]))
            .unwrap(),
            Command::UpdateVerify {
                manifest: PathBuf::from("m.json"),
                signature: PathBuf::from("m.json.minisig"),
                public_key: PathBuf::from("k.pub"),
            }
        );
        assert!(parse_args(args(&["update", "--verify", "m.json", "m.json.minisig"])).is_err());
        assert!(parse_args(args(&["update", "--verify", "m", "s", "k", "x"])).is_err());
        assert!(parse_args(args(&["update", "--bogus", "m", "s", "k"])).is_err());
    }

    #[test]
    fn update_check_command() {
        assert_eq!(
            parse_args(args(&["update", "--check"])).unwrap(),
            Command::UpdateCheck {
                channel: None,
                config: None
            }
        );
        assert_eq!(
            parse_args(args(&["update", "--check", "--channel", "edge"])).unwrap(),
            Command::UpdateCheck {
                channel: Some(update::Channel::Edge),
                config: None
            }
        );
        assert_eq!(
            parse_args(args(&["update", "--channel", "stable", "--check"])).unwrap(),
            Command::UpdateCheck {
                channel: Some(update::Channel::Stable),
                config: None
            }
        );
        assert!(parse_args(args(&["update", "--check", "--channel"])).is_err());
        assert!(parse_args(args(&["update", "--check", "--channel", "beta"])).is_err());
        assert!(parse_args(args(&["update", "--check", "--check"])).is_err());
        assert!(parse_args(args(&["update", "--check", "--bogus"])).is_err());
    }

    #[test]
    fn update_command_installs() {
        assert_eq!(
            parse_args(args(&["update"])).unwrap(),
            Command::Update {
                channel: None,
                config: None
            }
        );
        assert_eq!(
            parse_args(args(&["update", "--channel", "edge"])).unwrap(),
            Command::Update {
                channel: Some(update::Channel::Edge),
                config: None
            }
        );
        assert!(parse_args(args(&["update", "--channel"])).is_err());
        assert!(parse_args(args(&["update", "--channel", "edge", "--channel", "edge"])).is_err());
        assert!(parse_args(args(&["update", "now"])).is_err());
    }

    #[test]
    fn only_the_service_run_checks_the_pending_file() {
        let run = parse_args(args(&[])).unwrap();
        // Linux and macOS: the run with no subcommand. Windows: the service
        // entry, not main.
        assert_eq!(pending_check_applies(&run), cfg!(not(windows)));
        for cli in [
            vec!["diagnose"],
            vec!["update"],
            vec!["update", "--check"],
            vec!["update", "--verify", "m", "s", "k"],
            vec!["--help"],
        ] {
            let command = parse_args(args(&cli)).unwrap();
            assert!(!pending_check_applies(&command), "{cli:?}");
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn update_check_takes_no_config_outside_windows() {
        assert!(parse_args(args(&["update", "--check", "--config", "/tmp/c.toml"])).is_err());
        assert!(parse_args(args(&["update", "--config", "/tmp/c.toml"])).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn update_check_takes_a_config_on_windows() {
        assert_eq!(
            parse_args(args(&["update", "--check", "--config", r"D:\ci.toml"])).unwrap(),
            Command::UpdateCheck {
                channel: None,
                config: Some(PathBuf::from(r"D:\ci.toml"))
            }
        );
        assert!(parse_args(args(&["update", "--check", "--config"])).is_err());
        assert_eq!(
            parse_args(args(&[
                "update",
                "--config",
                r"D:\ci.toml",
                "--channel",
                "stable"
            ]))
            .unwrap(),
            Command::Update {
                channel: Some(update::Channel::Stable),
                config: Some(PathBuf::from(r"D:\ci.toml"))
            }
        );
        let session = parse_args(args(&["session-helper"])).unwrap();
        assert!(!pending_check_applies(&session));
        let install = parse_args(args(&["install"])).unwrap();
        assert!(!pending_check_applies(&install));
    }

    #[test]
    fn bad_arguments_are_errors() {
        assert!(parse_args(args(&["--config"])).is_err());
        assert!(parse_args(args(&["--bogus"])).is_err());
    }
}
