//! Where the time comes from, so a test of a bound does not wait for it.

pub trait Clock: Send + Sync {
    fn now_ns(&self) -> i64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ns(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos() as i64)
            .unwrap_or_default()
    }
}

pub const SECOND_NS: i64 = 1_000_000_000;
pub const MINUTE_NS: i64 = 60 * SECOND_NS;
pub const HOUR_NS: i64 = 60 * MINUTE_NS;
