//! The GPU source of Linux and Windows: NVML calls and the timing and
//! failure rules of `GpuPoller` (30 s query, new init 5 min after a failure,
//! one warning for each outage).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use nvml_wrapper::Nvml;
use tracing::{info, warn};

use crate::core::GpuSample;

/// The NVML libraries that the Windows init tries, in this order.
#[cfg(any(windows, test))]
const WINDOWS_LIBS: [&str; 3] = [
    "nvml.dll",
    r"C:\Program Files\NVIDIA Corporation\NVSMI\nvml.dll",
    r"C:\Windows\System32\nvml.dll",
];

static GPU: Mutex<GpuPoller<Nvml>> = Mutex::new(GpuPoller::new());

/// The GPU sample of this poll. NVML is queried at most every 30 s.
pub fn sample() -> Option<GpuSample> {
    let mut gpu = GPU.lock().unwrap_or_else(|e| e.into_inner());
    let (sample, logs) = gpu.poll(Instant::now(), init, query);
    for log in logs {
        match log {
            GpuLog::InitFailed(e) => {
                warn!("NVML init failed, no GPU entities, new init in 5 min: {e}")
            }
            GpuLog::QueryFailed(e) => {
                warn!("NVML query failed, no GPU entities, new init in 5 min: {e}")
            }
            GpuLog::PowerFailed(e) => warn!("NVML power query failed, no gpupower: {e}"),
            GpuLog::Recovered => info!("NVML works again"),
        }
    }
    sample
}

/// For `diagnose`: one init and one query of GPU 0, as (name, load, power).
#[cfg(target_os = "linux")]
pub fn check() -> Result<(String, u32, Result<f64, String>), String> {
    device_check(&init()?)
}

/// For `diagnose` on Windows: the library that opened and one query of
/// GPU 0, or the error of each library.
#[cfg(windows)]
pub fn check() -> Result<
    (
        &'static str,
        Result<super::win_diagnose::NvmlDevice, String>,
    ),
    String,
> {
    let (nvml, lib) = open_first(&WINDOWS_LIBS, open_windows)?;
    Ok((lib, device_check(&nvml)))
}

/// The name of GPU 0 and one query of it.
fn device_check(nvml: &Nvml) -> Result<(String, u32, Result<f64, String>), String> {
    let name = nvml
        .device_by_index(0)
        .and_then(|d| d.name())
        .map_err(|e| e.to_string())?;
    let (load, power) = query(nvml)?;
    Ok((name, load, power))
}

#[cfg(not(windows))]
fn init() -> Result<Nvml, String> {
    Nvml::init().map_err(|e| e.to_string())
}

#[cfg(windows)]
fn init() -> Result<Nvml, String> {
    open_first(&WINDOWS_LIBS, open_windows).map(|(nvml, _)| nvml)
}

#[cfg(windows)]
fn open_windows(lib: &str) -> Result<Nvml, String> {
    Nvml::builder()
        .lib_path(std::ffi::OsStr::new(lib))
        .init()
        .map_err(|e| e.to_string())
}

/// The handle of the first library in `libs` that `open` opens, with that
/// library. When all fail, the error names each library and its error.
#[cfg(any(windows, test))]
fn open_first<'a, H>(
    libs: &[&'a str],
    mut open: impl FnMut(&str) -> Result<H, String>,
) -> Result<(H, &'a str), String> {
    let mut errors = Vec::new();
    for lib in libs {
        match open(lib) {
            Ok(handle) => return Ok((handle, lib)),
            Err(e) => errors.push(format!("{lib}: {e}")),
        }
    }
    Err(errors.join("; "))
}

/// The utilization of GPU 0 in percent and its power in watts.
fn query(nvml: &Nvml) -> Result<(u32, Result<f64, String>), String> {
    let device = nvml.device_by_index(0).map_err(|e| e.to_string())?;
    let load = device.utilization_rates().map_err(|e| e.to_string())?.gpu;
    let power = device
        .power_usage()
        .map(|mw| f64::from(mw) / 1000.0)
        .map_err(|e| e.to_string());
    Ok((load, power))
}

/// The interval of the NVML query.
pub const GPU_QUERY: Duration = Duration::from_secs(30);
/// The delay before a new NVML init after a failure.
pub const GPU_RETRY: Duration = Duration::from_secs(300);

/// What the GPU source logs after one poll.
#[derive(Clone, Debug, PartialEq)]
pub enum GpuLog {
    /// The init failed (warning, once for each outage).
    InitFailed(String),
    /// The utilization query failed (warning, once for each outage).
    QueryFailed(String),
    /// Only the power query failed (warning, once until it works again).
    PowerFailed(String),
    /// A sample after an outage (info).
    Recovered,
}

/// The timing and failure rules of the GPU source. `H` is the NVML handle;
/// `init` and `query` are the NVML calls.
#[derive(Debug)]
pub struct GpuPoller<H> {
    handle: Option<H>,
    next_init: Option<Instant>,
    next_query: Option<Instant>,
    last: Option<GpuSample>,
    warned: bool,
    warned_power: bool,
}

impl<H> GpuPoller<H> {
    pub const fn new() -> Self {
        GpuPoller {
            handle: None,
            next_init: None,
            next_query: None,
            last: None,
            warned: false,
            warned_power: false,
        }
    }

    /// The GPU sample at `now` and the lines to log. `query` gives the
    /// utilization percent and the power in watts (or the power error).
    pub fn poll(
        &mut self,
        now: Instant,
        init: impl FnOnce() -> Result<H, String>,
        query: impl FnOnce(&H) -> Result<(u32, Result<f64, String>), String>,
    ) -> (Option<GpuSample>, Vec<GpuLog>) {
        let mut logs = Vec::new();
        if self.handle.is_none() {
            if self.next_init.is_some_and(|t| now < t) {
                return (None, logs);
            }
            match init() {
                Ok(handle) => {
                    self.handle = Some(handle);
                    self.next_init = None;
                    self.next_query = None;
                }
                Err(e) => {
                    self.next_init = Some(now + GPU_RETRY);
                    if !self.warned {
                        self.warned = true;
                        logs.push(GpuLog::InitFailed(e));
                    }
                    return (None, logs);
                }
            }
        }
        if self.next_query.is_some_and(|t| now < t) {
            return (self.last, logs);
        }
        let Some(handle) = &self.handle else {
            return (None, logs);
        };
        match query(handle) {
            Ok((load, power)) => {
                if self.warned {
                    self.warned = false;
                    logs.push(GpuLog::Recovered);
                }
                let power = match power {
                    Ok(watts) => {
                        self.warned_power = false;
                        Some(watts)
                    }
                    Err(e) => {
                        if !self.warned_power {
                            self.warned_power = true;
                            logs.push(GpuLog::PowerFailed(e));
                        }
                        None
                    }
                };
                self.last = Some(GpuSample { load, power });
                self.next_query = Some(now + GPU_QUERY);
            }
            Err(e) => {
                self.handle = None;
                self.last = None;
                self.next_query = None;
                self.next_init = Some(now + GPU_RETRY);
                if !self.warned {
                    self.warned = true;
                    logs.push(GpuLog::QueryFailed(e));
                }
            }
        }
        (self.last, logs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_windows_libraries_are_tried_in_order_until_one_opens() {
        let mut tried = Vec::new();
        let handle = open_first(&WINDOWS_LIBS, |lib| {
            tried.push(lib.to_string());
            if lib.contains("NVSMI") {
                Ok(lib.len())
            } else {
                Err("not found".to_string())
            }
        });
        // The handle comes with the library that opened it.
        assert_eq!(handle, Ok((WINDOWS_LIBS[1].len(), WINDOWS_LIBS[1])));
        assert_eq!(tried, WINDOWS_LIBS[..2]);
        assert_eq!(WINDOWS_LIBS[0], "nvml.dll");
        assert!(WINDOWS_LIBS[2].ends_with(r"System32\nvml.dll"));
    }

    #[test]
    fn when_no_windows_library_opens_the_error_names_each() {
        let err =
            open_first::<()>(&WINDOWS_LIBS, |lib| Err(format!("error of {lib}"))).unwrap_err();
        for lib in WINDOWS_LIBS {
            assert!(err.contains(&format!("{lib}: error of {lib}")), "{err}");
        }
    }

    // The GPU source: fake init and query, injected clock.

    type Query = Result<(u32, Result<f64, String>), String>;

    fn ok(load: u32, watts: f64) -> Query {
        Ok((load, Ok(watts)))
    }

    fn sample(load: u32, power: Option<f64>) -> Option<GpuSample> {
        Some(GpuSample { load, power })
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn no_init() -> Result<(), String> {
        panic!("no init expected")
    }

    fn no_query(_: &()) -> Query {
        panic!("no query expected")
    }

    #[test]
    fn an_init_error_warns_once_and_retries_after_5_min() {
        let t0 = Instant::now();
        let mut p = GpuPoller::<()>::new();
        let lib = "a library is not found".to_string();

        let (s, logs) = p.poll(t0, || Err(lib.clone()), no_query);
        assert_eq!(s, None);
        assert_eq!(logs, [GpuLog::InitFailed(lib.clone())]);

        // No new init before 5 min.
        for t in [2, 30, 298] {
            assert_eq!(p.poll(t0 + secs(t), no_init, no_query), (None, vec![]));
        }

        // The retry at 5 min fails again: no second warning.
        let (s, logs) = p.poll(t0 + secs(300), || Err(lib.clone()), no_query);
        assert_eq!((s, logs), (None, vec![]));
        assert_eq!(p.poll(t0 + secs(598), no_init, no_query), (None, vec![]));

        // The retry at 10 min works: one info line and a sample.
        let (s, logs) = p.poll(t0 + secs(600), || Ok(()), |_| ok(37, 85.3));
        assert_eq!(s, sample(37, Some(85.3)));
        assert_eq!(logs, [GpuLog::Recovered]);
    }

    #[test]
    fn a_working_gpu_is_queried_every_30_s_and_cached_between() {
        let t0 = Instant::now();
        let mut p = GpuPoller::<()>::new();

        let (s, logs) = p.poll(t0, || Ok(()), |_| ok(37, 85.3));
        assert_eq!((s, logs), (sample(37, Some(85.3)), vec![]));

        // Between the queries: the cached sample, no init, no query.
        for t in [2, 16, 28] {
            assert_eq!(
                p.poll(t0 + secs(t), no_init, no_query),
                (sample(37, Some(85.3)), vec![])
            );
        }

        // At 30 s: a new query on the same handle.
        let (s, _) = p.poll(t0 + secs(30), no_init, |_| ok(90, 300.0));
        assert_eq!(s, sample(90, Some(300.0)));
        assert_eq!(
            p.poll(t0 + secs(58), no_init, no_query).0,
            sample(90, Some(300.0))
        );
    }

    #[test]
    fn a_utilization_error_gives_no_sample_and_a_new_init_after_5_min() {
        let t0 = Instant::now();
        let mut p = GpuPoller::<()>::new();
        p.poll(t0, || Ok(()), |_| ok(37, 85.3));

        let err = "GPU is lost".to_string();
        let (s, logs) = p.poll(t0 + secs(30), no_init, |_| Err(err.clone()));
        assert_eq!(s, None);
        assert_eq!(logs, [GpuLog::QueryFailed(err)]);

        // No sample, no init and no query before 5 min.
        assert_eq!(p.poll(t0 + secs(60), no_init, no_query), (None, vec![]));
        assert_eq!(p.poll(t0 + secs(328), no_init, no_query), (None, vec![]));

        let (s, logs) = p.poll(t0 + secs(330), || Ok(()), |_| ok(5, 20.0));
        assert_eq!(s, sample(5, Some(20.0)));
        assert_eq!(logs, [GpuLog::Recovered]);
    }

    #[test]
    fn a_power_error_alone_gives_the_load_without_power() {
        let t0 = Instant::now();
        let mut p = GpuPoller::<()>::new();
        let err = "Not Supported".to_string();

        let (s, logs) = p.poll(t0, || Ok(()), |_| Ok((37, Err(err.clone()))));
        assert_eq!(s, sample(37, None));
        assert_eq!(logs, [GpuLog::PowerFailed(err.clone())]);

        // The next query has the same error: no second warning, same handle.
        let (s, logs) = p.poll(t0 + secs(30), no_init, |_| Ok((40, Err(err.clone()))));
        assert_eq!((s, logs), (sample(40, None), vec![]));

        // Power works again.
        let (s, _) = p.poll(t0 + secs(60), no_init, |_| ok(41, 99.5));
        assert_eq!(s, sample(41, Some(99.5)));
    }
}
