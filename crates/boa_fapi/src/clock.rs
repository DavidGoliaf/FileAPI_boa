//! Injectable clock used for the `File.lastModified` default.

use std::time::{SystemTime, UNIX_EPOCH};

/// A source of the current time, injectable for deterministic tests.
///
/// The extension uses the clock only when a `File` constructor call omits
/// `lastModified`; a supplied value never consults the clock.
pub trait Clock: Send + Sync + 'static {
    /// Returns the current UNIX time in milliseconds.
    fn now_unix_millis(&self) -> i64;
}

/// The default [`Clock`], reading the host system time.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix_millis(&self) -> i64 {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(delta) => i64::try_from(delta.as_millis()).unwrap_or(i64::MAX),
            Err(delta) => {
                i64::try_from(delta.duration().as_millis()).map_or(i64::MIN, |millis| -millis)
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn system_clock_returns_plausible_unix_millis() {
        let now = SystemClock.now_unix_millis();
        // Between 2020-01-01 and 2100-01-01.
        assert!(now > 1_577_836_800_000, "clock behind 2020: {now}");
        assert!(now < 4_102_344_000_000, "clock past 2100: {now}");
    }
}
