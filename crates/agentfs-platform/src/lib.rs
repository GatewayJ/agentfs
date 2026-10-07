//! Native mounting, confined host directories and validation processes.
mod directories;
mod native;
mod validation;

pub use directories::HostDirectories;
pub use native::NativeMountDriver;
pub use validation::ContainerValidation;

use agentfs_model::LocalIdentity;
use agentfs_ports::Clock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub struct SystemClock {
    start: Instant,
}
impl Default for SystemClock {
    fn default() -> Self {
        Self {
            start: Instant::now(),
        }
    }
}
impl Clock for SystemClock {
    fn now_ns(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|time| time.as_nanos().min(i64::MAX as u128) as i64)
            .unwrap_or(0)
    }
    fn monotonic_ms(&self) -> u64 {
        self.start.elapsed().as_millis().min(u64::MAX as u128) as u64
    }
}

#[cfg(unix)]
pub fn local_identity() -> LocalIdentity {
    use rustix::process::{getegid, geteuid};
    LocalIdentity {
        uid: geteuid().as_raw(),
        gid: getegid().as_raw(),
    }
}
#[cfg(windows)]
pub fn local_identity() -> LocalIdentity {
    LocalIdentity { uid: 1, gid: 1 }
}
