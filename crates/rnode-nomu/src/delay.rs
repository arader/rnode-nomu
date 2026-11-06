use std::time::Duration;

/// `embedded-hal` delay provider backed by `std::thread::sleep`.
#[derive(Debug, Default, Clone, Copy)]
pub struct StdDelay;

impl embedded_hal::delay::DelayNs for StdDelay {
    fn delay_ns(&mut self, ns: u32) {
        if ns > 0 {
            std::thread::sleep(Duration::from_nanos(u64::from(ns)));
        }
    }
}
