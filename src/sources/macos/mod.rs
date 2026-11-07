//! The macOS sources. A stub: task M2 adds the sources.

use tokio::sync::mpsc;

use crate::core::{Snapshot, SystemEvent};
use crate::entities::Entity;

pub fn supplied() -> &'static [&'static Entity] {
    &[]
}

pub fn model() -> String {
    "macOS".to_string()
}

pub fn boot_time() -> Option<i64> {
    None
}

pub fn poll() -> Snapshot {
    Snapshot::default()
}

/// The receiver of the sleep and wake events. No source sends them yet.
pub fn events() -> mpsc::Receiver<SystemEvent> {
    mpsc::channel(1).1
}
