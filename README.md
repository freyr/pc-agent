# pc-agent

A small system agent that reports the power, session and activity state of a PC to Home Assistant
over MQTT. It runs as a root service on Linux (systemd) and macOS (LaunchDaemon).

It publishes the same MQTT discovery entities as
[HASS.Agent](https://github.com/hass-agent/HASS.Agent), so a dual-boot PC can show as one Home
Assistant device on Windows and on Linux. The Linux target is Omarchy (Arch Linux, Hyprland) with an
NVIDIA GPU.

## Entities

| Entity | Linux | macOS | Source |
|---|---|---|---|
| `sessionstate` | yes | yes | Linux: logind and the Hyprland lock state. macOS: the console session. |
| `loggedusers` | yes | yes | Logged-in local users. |
| `lastactive` | yes | yes | Last keyboard, mouse or gamepad input. Linux: evdev. macOS: HID idle time. |
| `lastboot` | yes | yes | Boot time. |
| `lastsystemstatechange` | yes | yes | `ApplicationStarted`, `SessionLock`/`Unlock`, `SessionLogon`/`Logoff`, `Suspend`, `Resume`, `SystemShutdown` (Linux). |
| `display` | yes | no | Hyprland monitor power, with the DRM `dpms` files as a fallback. |
| `gpuload` | yes | yes | Linux: NVML. macOS: the GPU utilization of IOKit. |
| `gpupower` | yes | no | NVML. |

All messages are retained at QoS 1. The availability topic gets `online` at connect and `offline` at
a clean stop, at sleep and at shutdown (the broker's last will covers a crash).

## Install

Download the files from the [latest release](https://github.com/freyr/pc-agent/releases/latest).
The pre-release `edge` holds the newest build of `main`, for testing.

### Linux (x86_64)

```sh
mkdir pc-agent && cd pc-agent
curl -LO https://github.com/freyr/pc-agent/releases/latest/download/pc-agent-x86_64-linux.tar.gz
curl -LO https://github.com/freyr/pc-agent/releases/latest/download/pc-agent-x86_64-linux.tar.gz.sha256
sha256sum -c pc-agent-x86_64-linux.tar.gz.sha256
tar -xzf pc-agent-x86_64-linux.tar.gz
sudo scripts/install.sh                 # installs the binary and the systemd unit
sudo nano /etc/pc-agent/config.toml     # set the broker and the device (see Configuration)
sudo systemctl restart pc-agent
journalctl -u pc-agent -f
```

If a value in Home Assistant is wrong, run `sudo pc-agent diagnose` and keep the gamepad still for
its 10 s input test. It prints what each source reads. It reads no configuration and prints no
secret.

Uninstall: `sudo scripts/install.sh --uninstall` (keeps the configuration).

### macOS (Apple silicon)

1. Download `pc-agent-<version>-macos.pkg` and double-click it.
2. The package is not signed. If macOS blocks it, open System Settings → Privacy & Security and
   click "Open Anyway".
3. The installer asks for the broker address, the MQTT user and password, and the device name, then
   starts the service. An upgrade keeps the existing configuration and asks nothing.

Log: `tail -f /var/log/pc-agent.log`. Uninstall: `sudo /usr/local/share/pc-agent/uninstall.sh`
(add `--purge` to remove the configuration and the log too).

Without the package: unpack `pc-agent-aarch64-macos.tar.gz`, run `sudo scripts/install-macos.sh`,
edit `/etc/pc-agent/config.toml`, then `sudo launchctl kickstart -k system/pc-agent`.

### From source

Install Rust (<https://rustup.rs>), clone the repo and run the install script of your OS
(`sudo scripts/install.sh` or `sudo scripts/install-macos.sh`). The script builds the binary as your
user, then installs it.

## Configuration

`/etc/pc-agent/config.toml`, mode 0600, owner root. The installers create it from
[`packaging/config.example.toml`](packaging/config.example.toml) only if it does not exist.

```toml
[mqtt]
host = "<broker address>"
port = 1883
username = "<user>"
password = "<password>"

[device]
name = "MyPC"                      # device name in Home Assistant and in the topics
identifier = "pc-agent-MyPC"       # device identifier
entity_prefix = "my_pc"            # prefix of the entity IDs

[gpu]
enabled = true                     # false removes the GPU entities
```

The optional `[update]` section sets the release channel of the self-update: `channel = "stable"`
(the default, the newest release) or `channel = "edge"` (each push to `main`).
`pc-agent update --check [--channel stable|edge]` checks the channel for a newer signed build and
prints the result. It installs nothing and needs no root.

To take over the entities of an existing HASS.Agent device, use its device name and identifier, and
give its sensor IDs in a `[unique_ids]` section (`sessionstate`, `lastactive`, `lastboot`,
`lastsystemstatechange`).

## Repository

| Path | Content |
|---|---|
| `src/core.rs`, `src/mqtt.rs`, `src/entities.rs`, `src/config.rs` | OS-neutral core: the state machine, the MQTT client, the discovery payloads, the configuration. |
| `src/update/`, `build.rs` | The self-update (no OS code): the signed release manifest and the update check; `build.rs` embeds the commit and the build time (the commit time of HEAD). |
| `src/sources/linux/` | Linux sources: logind, Hyprland, DRM, evdev, NVML, the sleep lock, `diagnose`. |
| `src/sources/macos/` | macOS sources: IOKit and the power notifications. |
| `packaging/` | The systemd unit, the LaunchDaemon plist, the example configuration, the macOS package scripts. |
| `scripts/install.sh`, `scripts/install-macos.sh` | Install scripts. |
| `scripts/package.sh`, `scripts/build-pkg.sh` | Release archives and the macOS package (used by CI). |
| `scripts/manifest.sh`, `scripts/check-manifest.sh` | The release manifest of the bare binaries (signed in `release.yml`) and its CI check. |
| `scripts/check-edge.sh` | The CI check of `pc-agent update --check` against the real `edge` release. |
| `scripts/probe.sh`, `scripts/probe-macos.sh` | Signal probes from development. `pc-agent diagnose` replaces them. |

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The Linux code builds and tests on any OS in a container:
`docker run --rm -v "$PWD":/src -w /src rust:1 cargo test`.

`main` is protected: changes go in through pull requests. CI runs on each push and pull request for
Linux and macOS.

Releases come only from `main`:

- Each push to `main` replaces the pre-release
  [`edge`](https://github.com/freyr/pc-agent/releases/tag/edge), the bleeding-edge build.
- A semver tag `vX.Y.Z` on a commit of `main` makes a normal release. The tag must match the
  `version` in `Cargo.toml`.
