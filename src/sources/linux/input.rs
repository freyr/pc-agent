//! The input devices in `/dev/input`. A task on the tokio runtime opens each
//! `event*` node and reads it as an async stream; it keeps only the time of
//! the last counted event, which `poll()` reads.

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use evdev::{Device, PropType};
use tracing::{debug, info, warn};

use super::rules::{self, AxisInfo, InputCaps};

/// The directory of the input device nodes.
pub const INPUT_DIR: &str = "/dev/input";

const RESCAN: Duration = Duration::from_secs(10);

/// The time of the last counted event in seconds since the epoch; 0 before
/// the first one.
static LAST_INPUT: AtomicI64 = AtomicI64::new(0);

/// The time of the last counted event, or `None` before the first one.
pub fn last_input() -> Option<i64> {
    match LAST_INPUT.load(Ordering::Relaxed) {
        0 => None,
        t => Some(t),
    }
}

/// Starts the input task. Call it once, inside the tokio runtime.
pub fn start() {
    tokio::spawn(run(PathBuf::from(INPUT_DIR)));
}

/// The paths with an open stream.
type OpenSet = Arc<Mutex<HashSet<PathBuf>>>;

async fn run(dir: PathBuf) {
    let open: OpenSet = Arc::default();
    // One warning for a missing directory or a missing permission.
    let mut warned = false;
    loop {
        if let Err(e) = scan(&dir, &open)
            && !warned
        {
            warn!(
                "cannot read the input devices in {}, lastactive stays absent: {e}",
                dir.display()
            );
            warned = true;
        }
        tokio::time::sleep(RESCAN).await;
    }
}

/// Opens the `event*` nodes of `dir` that have no stream yet. Gives an error
/// when `dir` cannot be read or a node cannot be opened for lack of permission.
fn scan(dir: &Path, open: &OpenSet) -> std::io::Result<()> {
    let mut denied = None;
    for entry in std::fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        let is_event = path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("event"));
        if !is_event || lock(open).contains(&path) {
            continue;
        }
        match Device::open(&path) {
            Ok(device) => watch(path, device, open),
            Err(e) if e.kind() == ErrorKind::PermissionDenied => denied = Some(e),
            Err(e) => debug!(path = %path.display(), "cannot open input device: {e}"),
        }
    }
    denied.map_or(Ok(()), Err)
}

fn lock(open: &OpenSet) -> std::sync::MutexGuard<'_, HashSet<PathBuf>> {
    open.lock().unwrap_or_else(|e| e.into_inner())
}

/// The capabilities of the device that the activity test uses.
fn caps(device: &Device) -> InputCaps {
    let gamepad = device
        .supported_keys()
        .is_some_and(|keys| rules::is_gamepad(keys.iter().map(|k| k.code())));
    let axes = match device.get_absinfo() {
        Ok(list) => list
            .map(|(code, info)| {
                (
                    code.0,
                    AxisInfo {
                        min: info.minimum(),
                        max: info.maximum(),
                        flat: info.flat(),
                    },
                )
            })
            .collect(),
        Err(_) => Default::default(),
    };
    InputCaps { gamepad, axes }
}

/// Starts the stream task of one device, unless it is an accelerometer.
fn watch(path: PathBuf, device: Device, open: &OpenSet) {
    if device.properties().contains(PropType::ACCELEROMETER) {
        return;
    }
    let caps = caps(&device);
    let name = device.name().unwrap_or("").to_string();
    let mut stream = match device.into_event_stream() {
        Ok(stream) => stream,
        Err(e) => {
            debug!(path = %path.display(), "cannot read input device: {e}");
            return;
        }
    };
    info!(path = %path.display(), name = %name, "input device opened");
    lock(open).insert(path.clone());
    let open = Arc::clone(open);
    tokio::spawn(async move {
        // Ends with an error when the device goes away.
        while let Ok(event) = stream.next_event().await {
            if rules::is_activity(&caps, event.event_type().0, event.code(), event.value()) {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs() as i64);
                LAST_INPUT.store(now, Ordering::Relaxed);
            }
        }
        lock(&open).remove(&path);
        info!(path = %path.display(), "input device closed");
    });
}
