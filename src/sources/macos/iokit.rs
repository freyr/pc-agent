//! The thin reads of the macOS sources: IOKit and `sysctlbyname`, in
//! process. Each read releases every CF object and IOKit handle it takes.

use std::collections::HashMap;
use std::ffi::{CStr, c_void};

use core_foundation::array::CFArray;
use core_foundation::base::{CFType, TCFType, kCFAllocatorDefault};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use io_kit_sys::types::{io_iterator_t, io_object_t};
use io_kit_sys::{
    IOIteratorNext, IOObjectRelease, IORegistryEntryCreateCFProperty, IORegistryGetRootEntry,
    IOServiceGetMatchingService, IOServiceGetMatchingServices, IOServiceMatching,
};

use super::rules::ConsoleSession;

/// `kIOMainPortDefault`: the null port selects the default main port.
const MAIN_PORT: u32 = 0;
const KERN_SUCCESS: i32 = 0;

/// An IOKit object that is released on drop.
struct Object(io_object_t);

impl Object {
    fn new(object: io_object_t) -> Option<Object> {
        (object != 0).then_some(Object(object))
    }
}

impl Drop for Object {
    fn drop(&mut self) {
        // SAFETY: the object is a valid reference that this value owns.
        unsafe { IOObjectRelease(self.0) };
    }
}

/// A property of a registry entry (create rule: released on drop).
fn property(entry: &Object, key: &str) -> Option<CFType> {
    let key = CFString::new(key);
    // SAFETY: valid entry and key; the result follows the create rule.
    let value = unsafe {
        IORegistryEntryCreateCFProperty(entry.0, key.as_concrete_TypeRef(), kCFAllocatorDefault, 0)
    };
    // SAFETY: a non-null result is a CF object that we own.
    (!value.is_null()).then(|| unsafe { CFType::wrap_under_create_rule(value) })
}

/// The value of `key` in a dictionary (get rule: retained by the wrapper).
fn get(dict: &CFDictionary, key: &str) -> Option<CFType> {
    let key = CFString::new(key);
    dict.find(key.as_CFTypeRef())
        // SAFETY: the dictionary holds a valid CF object for the key.
        .map(|value| unsafe { CFType::wrap_under_get_rule(*value) })
}

fn as_bool(value: &CFType) -> Option<bool> {
    match value.downcast::<CFBoolean>() {
        Some(b) => Some(b.into()),
        None => as_i64(value).map(|n| n != 0),
    }
}

fn as_i64(value: &CFType) -> Option<i64> {
    value.downcast::<CFNumber>().and_then(|n| n.to_i64())
}

fn first_service(class: &CStr) -> Option<Object> {
    // SAFETY: `IOServiceMatching` gives a dictionary that
    // `IOServiceGetMatchingService` consumes.
    let service = unsafe {
        let matching = IOServiceMatching(class.as_ptr());
        if matching.is_null() {
            return None;
        }
        IOServiceGetMatchingService(MAIN_PORT, matching as _)
    };
    Object::new(service)
}

fn services(class: &CStr) -> Vec<Object> {
    let mut iterator: io_iterator_t = 0;
    // SAFETY: as in `first_service`; `IOServiceGetMatchingServices` consumes
    // the dictionary and gives an iterator that we own.
    let result = unsafe {
        let matching = IOServiceMatching(class.as_ptr());
        if matching.is_null() {
            return Vec::new();
        }
        IOServiceGetMatchingServices(MAIN_PORT, matching as _, &mut iterator)
    };
    let Some(iterator) = Object::new(iterator).filter(|_| result == KERN_SUCCESS) else {
        return Vec::new();
    };
    // SAFETY: a valid iterator; each object it gives is ours.
    std::iter::from_fn(|| Object::new(unsafe { IOIteratorNext(iterator.0) })).collect()
}

/// The `IOConsoleUsers` list of the IO registry root. An absent list is an
/// empty list; `None` when the root cannot be read.
pub fn console_sessions() -> Option<Vec<ConsoleSession>> {
    // SAFETY: the root entry is a reference that we own.
    let root = Object::new(unsafe { IORegistryGetRootEntry(MAIN_PORT) })?;
    let Some(list) = property(&root, "IOConsoleUsers") else {
        return Some(Vec::new());
    };
    let list = list.downcast::<CFArray>()?;
    let sessions = list
        .iter()
        // SAFETY: the array holds valid CF objects.
        .map(|item| unsafe { CFType::wrap_under_get_rule(*item) })
        .filter_map(|item| item.downcast::<CFDictionary>())
        .map(|entry| {
            let flag = |key| get(&entry, key).and_then(|v| as_bool(&v));
            ConsoleSession {
                user: get(&entry, "kCGSSessionUserNameKey")
                    .and_then(|v| v.downcast::<CFString>())
                    .map(|s| s.to_string()),
                on_console: flag("kCGSSessionOnConsoleKey").unwrap_or(false),
                login_done: flag("kCGSessionLoginDoneKey").unwrap_or(false),
                locked: flag("CGSSessionScreenIsLocked"),
            }
        })
        .collect();
    Some(sessions)
}

/// `HIDIdleTime` of the `IOHIDSystem` service, in ns.
pub fn hid_idle_ns() -> Option<u64> {
    let service = first_service(c"IOHIDSystem")?;
    let idle = property(&service, "HIDIdleTime")?;
    as_i64(&idle).and_then(|n| u64::try_from(n).ok())
}

/// The integer values of `PerformanceStatistics` of the first
/// `IOAccelerator` service that has the utilization key.
pub fn gpu_stats() -> Option<HashMap<String, i64>> {
    services(c"IOAccelerator").iter().find_map(|service| {
        let stats = property(service, "PerformanceStatistics")?.downcast::<CFDictionary>()?;
        let (keys, values) = stats.get_keys_and_values();
        let map: HashMap<String, i64> = keys
            .into_iter()
            .zip(values)
            .filter_map(|(k, v)| {
                // SAFETY: the dictionary holds valid CF objects.
                let (k, v) = unsafe {
                    (
                        CFType::wrap_under_get_rule(k),
                        CFType::wrap_under_get_rule(v),
                    )
                };
                Some((k.downcast::<CFString>()?.to_string(), as_i64(&v)?))
            })
            .collect();
        map.contains_key(super::rules::GPU_UTILIZATION_KEY)
            .then_some(map)
    })
}

/// `kern.boottime`, in seconds since the epoch.
pub fn boot_time() -> Option<i64> {
    // SAFETY: `timeval` is plain data.
    let mut tv: libc::timeval = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::timeval>();
    // SAFETY: the buffer and its length are valid.
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.boottime".as_ptr(),
            (&raw mut tv).cast::<c_void>(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && len == std::mem::size_of::<libc::timeval>()).then_some(tv.tv_sec)
}

/// `kern.osproductversion`, for example `26.5`.
pub fn os_version() -> Option<String> {
    let mut buf = [0u8; 64];
    let mut len = buf.len();
    // SAFETY: the buffer and its length are valid.
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.osproductversion".as_ptr(),
            buf.as_mut_ptr().cast::<c_void>(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    let version = CStr::from_bytes_until_nul(&buf[..len])
        .ok()?
        .to_str()
        .ok()?;
    (!version.is_empty()).then(|| version.to_string())
}
