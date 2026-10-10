//! Independent, bounded Linux host observation; never signals workload processes.
use serde::{Deserialize, Serialize};
use std::{fs::File, io::Read};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostSample {
    pub unix_seconds: u64,
    pub loadavg: Option<String>,
    pub memory: Option<String>,
    pub pressure_cpu: Option<String>,
    pub pressure_memory: Option<String>,
    pub pressure_io: Option<String>,
}

fn read(path: &str) -> Option<String> {
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take(8192)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

pub fn sample() -> HostSample {
    HostSample {
        unix_seconds: super::supervisor::now(),
        loadavg: read("/proc/loadavg"),
        memory: read("/proc/meminfo"),
        pressure_cpu: read("/proc/pressure/cpu"),
        pressure_memory: read("/proc/pressure/memory"),
        pressure_io: read("/proc/pressure/io"),
    }
}
