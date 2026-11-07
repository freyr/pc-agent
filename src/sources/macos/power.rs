//! The sleep and wake source: `IORegisterForSystemPower` on its own thread
//! with a `CFRunLoop`. This thread is the only second thread of the agent.

use std::ffi::c_void;
use std::ptr;
use std::time::{Duration, Instant};

use core_foundation::base::TCFType;
use core_foundation::runloop::{CFRunLoop, CFRunLoopSource, kCFRunLoopDefaultMode};
use io_kit_sys::types::{io_connect_t, io_object_t, io_service_t};
use io_kit_sys::{IONotificationPortGetRunLoopSource, IONotificationPortRef};
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

use super::rules::{PowerAction, power_action};
use crate::core::SystemEvent;

/// The longest wait for the core's reply to `Suspend`. The core replies
/// within 2 s; macOS waits at most 30 s for the allow.
const SUSPEND_REPLY_TIMEOUT: Duration = Duration::from_secs(3);
const REPLY_POLL: Duration = Duration::from_millis(10);

type InterestCallback = unsafe extern "C" fn(
    refcon: *mut c_void,
    service: io_service_t,
    message_type: u32,
    message_argument: *mut c_void,
);

// IOPMLib.h; io-kit-sys does not declare these two.
#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IORegisterForSystemPower(
        refcon: *mut c_void,
        port: *mut IONotificationPortRef,
        callback: InterestCallback,
        notifier: *mut io_object_t,
    ) -> io_connect_t;
    fn IOAllowPowerChange(kernel_port: io_connect_t, notification_id: isize) -> i32;
}

/// The state that the callback gets through `refcon`.
struct PowerState {
    root_port: io_connect_t,
    events: mpsc::Sender<SystemEvent>,
}

/// Starts the power thread. It sends `Suspend` and `Resume` to `events`.
pub fn start(events: mpsc::Sender<SystemEvent>) {
    let spawned = std::thread::Builder::new()
        .name("power".to_string())
        .spawn(move || run(events));
    if let Err(e) = spawned {
        warn!(error = %e, "cannot start the power thread: no Suspend and Resume");
    }
}

fn run(events: mpsc::Sender<SystemEvent>) {
    // The thread and its registration live for the whole process, so this
    // one-time leak is intentional: the callback uses the state until exit.
    let state: &'static mut PowerState = Box::leak(Box::new(PowerState {
        root_port: 0,
        events,
    }));
    let mut port: IONotificationPortRef = ptr::null_mut();
    let mut notifier: io_object_t = 0;
    // SAFETY: valid out pointers; `refcon` points to the leaked state.
    let root_port = unsafe {
        IORegisterForSystemPower(
            ptr::from_mut(state).cast(),
            &mut port,
            callback,
            &mut notifier,
        )
    };
    if root_port == 0 {
        warn!("cannot register for system power notifications: no Suspend and Resume");
        return;
    }
    // The callback runs only on this thread's run loop, which starts below.
    state.root_port = root_port;

    // SAFETY: `port` is the valid port of the registration; the source follows
    // the get rule (the port owns it).
    let source =
        unsafe { CFRunLoopSource::wrap_under_get_rule(IONotificationPortGetRunLoopSource(port)) };
    // SAFETY: a constant of CoreFoundation.
    CFRunLoop::get_current().add_source(&source, unsafe { kCFRunLoopDefaultMode });
    info!("registered for system power notifications");
    CFRunLoop::run_current();
    warn!("the power run loop stopped: no Suspend and Resume");
}

/// The `IOServiceInterestCallback` of the registration.
unsafe extern "C" fn callback(
    refcon: *mut c_void,
    _service: io_service_t,
    message_type: u32,
    message_argument: *mut c_void,
) {
    // SAFETY: `refcon` is the leaked `PowerState` of `run`.
    let state = unsafe { &*refcon.cast::<PowerState>() };
    let notification_id = message_argument as isize;
    match power_action(message_type) {
        PowerAction::AllowNow => allow(state.root_port, notification_id),
        PowerAction::Suspend => {
            info!("system will sleep");
            suspend(&state.events);
            // Never skip the allow: without it, macOS waits 30 s.
            allow(state.root_port, notification_id);
        }
        PowerAction::Resume => {
            info!("system has powered on");
            if state.events.blocking_send(SystemEvent::Resume).is_err() {
                warn!("cannot send Resume: the core has stopped");
            }
        }
        PowerAction::Ignore => {}
    }
}

/// Sends `Suspend` and waits for the core's reply, at most 3 s.
fn suspend(events: &mpsc::Sender<SystemEvent>) {
    let (done, mut reply) = oneshot::channel();
    if events.blocking_send(SystemEvent::Suspend { done }).is_err() {
        warn!("cannot send Suspend: the core has stopped");
        return;
    }
    let deadline = Instant::now() + SUSPEND_REPLY_TIMEOUT;
    loop {
        match reply.try_recv() {
            // A dropped reply also ends the wait: the core has stopped.
            Ok(()) | Err(oneshot::error::TryRecvError::Closed) => return,
            Err(oneshot::error::TryRecvError::Empty) if Instant::now() >= deadline => {
                warn!(
                    "no reply to Suspend within {} s",
                    SUSPEND_REPLY_TIMEOUT.as_secs()
                );
                return;
            }
            Err(oneshot::error::TryRecvError::Empty) => std::thread::sleep(REPLY_POLL),
        }
    }
}

fn allow(root_port: io_connect_t, notification_id: isize) {
    // SAFETY: the root port of the registration and the id of its message.
    let result = unsafe { IOAllowPowerChange(root_port, notification_id) };
    if result != 0 {
        warn!(result, "IOAllowPowerChange failed");
    }
}
