//! The sources of the running OS. Each OS module gives the same interface to
//! the core: `supplied(gpu)`, `model()`, `boot_time()`, `poll(gpu)` and
//! `events()`, and `diagnose()` for `pc-agent diagnose`. `gpu` is `[gpu] enabled` of the configuration: when it is
//! `false`, the source supplies no GPU entities and reads no GPU. The Windows
//! module also gives the service frame: `run_as_service`, `install` and
//! `uninstall`.

/// The wait of the core for the broker's ack of `offline` before sleep,
/// shutdown or stop. Windows allows about 2 s for the suspend handler, which
/// blocks for this wait, so the wait is shorter there.
#[cfg(windows)]
pub const ACK_WAIT: std::time::Duration = std::time::Duration::from_millis(1500);
#[cfg(not(windows))]
pub const ACK_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(any(target_os = "linux", windows))]
mod nvml;

#[cfg(any(windows, test))]
mod win_diagnose;

#[cfg(any(windows, test))]
mod wts_rules;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;
