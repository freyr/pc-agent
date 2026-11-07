# pc-agent

Linux system agent that reports PC power, session and activity state to Home Assistant over MQTT.

It publishes the same MQTT discovery entities as [HASS.Agent](https://github.com/hass-agent/HASS.Agent),
so a dual-boot PC shows as one Home Assistant device on Windows and on Linux. The target is
Omarchy (Arch Linux, Hyprland) with an NVIDIA GPU.

The agent is under construction. The repo holds the probe script below and the Cargo package.

## Probe the target PC

`scripts/probe.sh` reads the signals that the agent depends on and prints them as plain text:

- the Omarchy version and the installed packages;
- the logind sessions;
- the Hyprland monitor state (lock and display), read as root through the user's socket;
- the DRM `dpms` files;
- the NVIDIA driver, and the memory and CPU cost of NVML;
- the input devices, and the gamepad axis limits with 10 s of readings at rest.

It needs bash. It uses `python3` and `jq` if they are present. The output holds user names and
device names but no secret. Do not commit the output files (`probe-*.txt` is git-ignored).

Run it on the target PC, twice:

1. Get the repo:

   ```sh
   git clone https://github.com/freyr/pc-agent.git
   cd pc-agent
   ```

2. Run the probe while the PC is unlocked. Do not touch the gamepad while it runs.

   ```sh
   sudo bash scripts/probe.sh > probe-unlocked.txt 2>&1
   ```

3. Run the probe while the PC is locked. The first command asks for the password, then the probe
   starts 20 s later in the background:

   ```sh
   sudo -v; (sleep 20; sudo bash scripts/probe.sh > probe-locked.txt 2>&1) &
   ```

   Lock the PC at once. Unlock it after 30 s.

4. Send `probe-unlocked.txt` and `probe-locked.txt` to the maintainer.

## Install on macOS

The macOS agent runs as a LaunchDaemon. It publishes these entities: sessionstate, lastactive,
lastboot, lastsystemstatechange, loggedusers and gpuload. The entities display and gpupower are
not supplied on macOS.

### Recommended: the installer package

1. Download `pc-agent-<version>-macos.pkg` from the latest release. The package is for Apple
   silicon.
2. Double-click the package. The package is not signed, so macOS blocks it. Open System
   Settings, then Privacy & Security. Click "Open Anyway" for the package. You can also
   right-click the package and choose Open. Then follow the installer.
3. The installer asks for the MQTT broker address, the MQTT user, the MQTT password and the
   device name in dialogs. It writes `/etc/pc-agent/config.toml` with mode 0600 and starts the
   service. A last dialog shows if the agent connected to the broker.
4. An upgrade keeps the existing configuration. The installer asks no questions then.
5. If nobody is logged in, or you cancel a dialog, the installer writes the example
   configuration and does not start the service. Edit `/etc/pc-agent/config.toml`. Then run
   `sudo launchctl bootstrap system /Library/LaunchDaemons/pc-agent.plist`.

To uninstall, run `sudo /usr/local/share/pc-agent/uninstall.sh`. It keeps
`/etc/pc-agent/config.toml`. The option `--purge` also removes `/etc/pc-agent` and the log.

### Other ways

Use the prebuilt archive or a clone of the repo.

1. Get the files. Use one of two ways.

   Download `pc-agent-aarch64-macos.tar.gz` and its `.sha256` file from a release. Then check
   the archive and unpack it into a new folder:

   ```sh
   shasum -a 256 -c pc-agent-aarch64-macos.tar.gz.sha256
   mkdir pc-agent
   tar -xzf pc-agent-aarch64-macos.tar.gz -C pc-agent
   cd pc-agent
   ```

   Or clone the repo. You need Rust (https://rustup.rs) for this way:

   ```sh
   git clone https://github.com/freyr/pc-agent.git
   cd pc-agent
   ```

2. Install the service. In a clone, the script builds as your user first. In an unpacked
   archive, it installs the prebuilt binary and removes the Gatekeeper quarantine flag:

   ```sh
   sudo scripts/install-macos.sh
   ```

   The script installs `/usr/local/bin/pc-agent` and `/Library/LaunchDaemons/pc-agent.plist`. It
   creates `/etc/pc-agent/config.toml` from the example only if the file does not exist.

3. Edit `/etc/pc-agent/config.toml`. Set the broker address, the user, the password and the
   device names. The file has mode 0600 and belongs to root.

   ```sh
   sudo nano /etc/pc-agent/config.toml
   ```

4. Restart the service:

   ```sh
   sudo launchctl kickstart -k system/pc-agent
   ```

5. Check the service and read the log:

   ```sh
   sudo launchctl print system/pc-agent
   tail -f /var/log/pc-agent.log
   ```

To uninstall, run the script with `--uninstall`. It stops the service and removes the plist and
the binary. It keeps `/etc/pc-agent/config.toml`.

```sh
sudo scripts/install-macos.sh --uninstall
```
