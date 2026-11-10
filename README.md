# pc-mqtt-agent

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

Download the files from the [latest release](https://github.com/freyr/pc-mqtt-agent/releases/latest).
The pre-release `edge` holds the newest build of `main`, for testing.

### Linux (x86_64)

```sh
mkdir pc-agent && cd pc-agent
curl -LO https://github.com/freyr/pc-mqtt-agent/releases/latest/download/pc-agent-x86_64-linux.tar.gz
curl -LO https://github.com/freyr/pc-mqtt-agent/releases/latest/download/pc-agent-x86_64-linux.tar.gz.sha256
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

An upgrade runs the same steps: `install.sh` keeps `/etc/pc-agent/config.toml`, so skip the edit
and the restart. From 0.3.0 on, the agent updates itself (see `[update]` in Configuration); a manual install is
needed only once, for a version older than 0.3.0.

Uninstall: `sudo scripts/install.sh --uninstall` (keeps the configuration).

### Windows (x86_64)

Run PowerShell as administrator. The service must run from `C:\Program Files\pc-agent`: `install`
registers the path of the running exe, and the self-update replaces the exe in that folder.

```powershell
New-Item -ItemType Directory -Force "C:\Program Files\pc-agent" | Out-Null
cd "C:\Program Files\pc-agent"
Invoke-WebRequest https://github.com/freyr/pc-mqtt-agent/releases/latest/download/pc-agent-x86_64-windows.exe -OutFile pc-agent.exe
Invoke-WebRequest https://github.com/freyr/pc-mqtt-agent/releases/latest/download/pc-agent-x86_64-windows.exe.sha256 -OutFile pc-agent.exe.sha256
(Get-FileHash pc-agent.exe).Hash -eq (Get-Content pc-agent.exe.sha256).Split(" ")[0].ToUpper()   # must print True
.\pc-agent.exe install                                  # creates the service and C:\ProgramData\pc-agent
notepad C:\ProgramData\pc-agent\config.toml             # set the broker and the device (see Configuration)
Restart-Service pc-agent
sc.exe qc pc-agent                                      # BINARY_PATH_NAME: C:\Program Files\pc-agent\pc-agent.exe
```

To move an existing install to this folder (or to upgrade from a version older than 0.3.0), download
the exe as above, then run `.\pc-agent.exe uninstall` and `.\pc-agent.exe install` from
`C:\Program Files\pc-agent`. `uninstall` removes only the service; the configuration stays. Then
delete the old exe.

If a value in Home Assistant is wrong, run `.\pc-agent.exe diagnose` as SYSTEM (for example with
`psexec -s`). It prints what each Windows source reads and publishes nothing.

Uninstall: `.\pc-agent.exe uninstall` (keeps `C:\ProgramData\pc-agent`).

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
`pc-agent update [--channel stable|edge]` (with sudo, or as administrator on Windows) installs a
newer signed build in place of the binary that runs it and restarts the service. Run it from the
installed binary (`/usr/local/bin/pc-agent`, or `pc-agent.exe` in the folder of the service). The
first connect of the new build to the broker confirms the update. When the new build gets no
connect at its third start or within 10 min, the service moves `pc-agent.prev` back and records the
build as failed in `update-state.json` (in `/var/lib/pc-agent`, `/Library/Application
Support/pc-agent` or `C:\ProgramData\pc-agent`).

The service checks the channel 10 min after its start and then every 24 h, while it is connected
to the broker. With `auto = true` (the default in `[update]`), a check installs a newer build when
the last automatic install is 7 days old or more; `auto = false` stops the automatic install. The
HA entity `update.<entity_prefix>_agent` shows the installed and the latest version. Its Install
button checks at once and installs a newer build, also with `auto = false`. Neither the schedule
nor HA installs a build that failed before; only `pc-agent update` does.

To take over the entities of an existing HASS.Agent device, use its device name and identifier, and
give its sensor IDs in a `[unique_ids]` section (`sessionstate`, `lastactive`, `lastboot`,
`lastsystemstatechange`).

## Repository

| Path | Content |
|---|---|
| `src/core.rs`, `src/mqtt.rs`, `src/entities.rs`, `src/config.rs` | OS-neutral core: the state machine, the MQTT client, the discovery payloads, the configuration. |
| `src/update/`, `build.rs` | The self-update (no OS code except the restart command): the signed release manifest, the update check, the install, the rollback rules; `build.rs` embeds the commit and the build time (the commit time of HEAD). |
| `src/sources/linux/` | Linux sources: logind, Hyprland, DRM, evdev, NVML, the sleep lock, `diagnose`. |
| `src/sources/macos/` | macOS sources: IOKit and the power notifications. |
| `packaging/` | The systemd unit, the LaunchDaemon plist, the example configuration, the macOS package scripts. |
| `scripts/install.sh`, `scripts/install-macos.sh` | Install scripts. |
| `scripts/package.sh`, `scripts/build-pkg.sh` | Release archives and the macOS package (used by CI). |
| `scripts/manifest.sh`, `scripts/check-manifest.sh` | The release manifest of the bare binaries (signed in `release.yml`) and its CI check. |
| `scripts/check-edge.sh` | The CI check of `pc-agent update --check` against the real `edge` release. |
| `scripts/e2e-update.sh` | The CI end-to-end test of `pc-agent update`, of the rollback and of the install command of the HA update entity (Linux and macOS; builds of the test features `update-test-key` and `update-test-exit`, never in a release). |
| `scripts/probe.sh`, `scripts/probe-macos.sh` | Signal probes from development. `pc-agent diagnose` replaces them. |

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo deny --all-features check   # licenses, advisories, duplicate crates (deny.toml)
cargo llvm-cov --summary-only      # coverage (advisory in CI)
```

CI uses Rust 1.99.1 (`RUST_TOOLCHAIN` in `ci.yml` and `release.yml`) and also runs gitleaks and
zizmor.

The Linux code builds and tests on any OS in a container:
`docker run --rm -v "$PWD":/src -w /src rust:1 cargo test`.

`main` is protected: changes go in through pull requests, merged with squash. CI runs on each pull
request and on each push to `main`, for Linux, macOS and Windows. The required checks are
`pr-title`, `version` and `ci-gate`.

The repository follows the release standard of [`freyr/.github`](https://github.com/freyr/.github).
The PR title is the squash commit title and the only release input. It follows
[Conventional Commits](https://www.conventionalcommits.org/):

| PR title | Version change |
|---|---|
| `feat: …` | minor |
| `fix: …`, `perf: …`, `revert: …` | patch |
| `feat!: …`, or `BREAKING CHANGE:` in the PR body | major (minor before 1.0.0) |
| `docs:`, `test:`, `ci:`, `build:`, `refactor:`, `chore:`, `style:` | none |

A change of only Markdown files, test data or CI-only scripts makes no release (see
`.github/release.json`). Do not edit the version in `Cargo.toml` by hand, and do not push tags.

Releases come only from `main`:

- Each push to `main` replaces the pre-release
  [`edge`](https://github.com/freyr/pc-mqtt-agent/releases/tag/edge), the bleeding-edge build.
- On each PR, the `version` job sets the next version in `Cargo.toml` and `Cargo.lock` from the PR
  title, with a commit of the release App on the PR branch.
- After the merge, `tag.yml` compares the version in `Cargo.toml` with the last tag. When the
  version is higher, the release App pushes the annotated tag `vX.Y.Z`, whose message holds the
  release notes. The tag starts `release.yml`, which builds and signs the assets and creates the
  GitHub release with those notes.
