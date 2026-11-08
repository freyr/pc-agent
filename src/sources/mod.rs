//! The sources of the running OS. Each OS module gives the same interface to
//! the core: `supplied(gpu)`, `model()`, `boot_time()`, `poll(gpu)` and
//! `events()`, and `diagnose()` for `pc-agent diagnose`. `gpu` is `[gpu] enabled` of the configuration: when it is
//! `false`, the source supplies no GPU entities and reads no GPU.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;
