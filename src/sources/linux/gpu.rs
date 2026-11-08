//! The NVML calls of the GPU source. The timing and failure rules are in
//! `rules::GpuPoller`.

use std::sync::Mutex;
use std::time::Instant;

use nvml_wrapper::Nvml;
use tracing::{info, warn};

use super::rules::{GpuLog, GpuPoller};
use crate::core::GpuSample;

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
pub fn check() -> Result<(String, u32, Result<f64, String>), String> {
    let nvml = init()?;
    let name = nvml
        .device_by_index(0)
        .and_then(|d| d.name())
        .map_err(|e| e.to_string())?;
    let (load, power) = query(&nvml)?;
    Ok((name, load, power))
}

fn init() -> Result<Nvml, String> {
    Nvml::init().map_err(|e| e.to_string())
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
