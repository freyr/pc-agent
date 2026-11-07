//! The sources of the running OS. Each OS module gives the same interface to
//! the core: `supplied()`, `model()`, `boot_time()`, `poll()` and `events()`.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;
