mod config;
mod core;
mod entities;
mod mqtt;
mod sources;
mod time;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, bail};
use tokio::signal::unix::{SignalKind, signal};
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::core::Core;

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

#[derive(Debug, PartialEq)]
enum Command {
    Run { config: PathBuf },
    Diagnose,
    Help,
}

fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Result<Command> {
    let mut config = PathBuf::from(config::DEFAULT_PATH);
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "diagnose" => {
                if let Some(other) = args.next() {
                    bail!("unknown argument {other:?} after diagnose");
                }
                return Ok(Command::Diagnose);
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

fn main() -> ExitCode {
    let config_path = match parse_args(std::env::args().skip(1)) {
        Ok(Command::Help) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(Command::Diagnose) => return diagnose(),
        Ok(Command::Run { config }) => config,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    match run(config_path) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

/// `pc-agent diagnose`: runs the checks of the OS source once.
fn diagnose() -> ExitCode {
    match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => {
            runtime.block_on(sources::diagnose());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: cannot start the runtime: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(config_path: PathBuf) -> Result<()> {
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
        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sigint = signal(SignalKind::interrupt())?;
        let stop = async move {
            tokio::select! {
                _ = sigterm.recv() => info!("SIGTERM received"),
                _ = sigint.recv() => info!("SIGINT received"),
            }
        };
        let poll = move || sources::poll(gpu);
        mqtt::run(&cfg, &model, core, poll, sources::events(), stop).await
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

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
        assert_eq!(parse_args(args(&["diagnose"])).unwrap(), Command::Diagnose);
        assert!(parse_args(args(&["diagnose", "--bogus"])).is_err());
    }

    #[test]
    fn bad_arguments_are_errors() {
        assert!(parse_args(args(&["--config"])).is_err());
        assert!(parse_args(args(&["--bogus"])).is_err());
    }
}
