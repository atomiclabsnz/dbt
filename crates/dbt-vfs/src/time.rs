//! ferrion-wasm: the clock and timer seam.
//!
//! On `wasm32-unknown-unknown`, `std::time::Instant::now()` and
//! `SystemTime::now()` compile and then panic ("time not implemented on this
//! platform"), and tokio's timer driver reads `std::time::Instant`, so a
//! runtime with timers enabled panics as it is built. `scripts/wasm-runtime-rewrite.py`
//! routes the run-path crates through this module instead.
//!
//! | item                     | native            | wasm32                                 |
//! |--------------------------|-------------------|----------------------------------------|
//! | [`Instant`]              | `std` `Instant`   | `web_time::Instant` (`performance.now`) |
//! | [`system_now`]           | `SystemTime::now` | `std` `SystemTime` from `Date.now`     |
//! | [`sleep`]                | `tokio::time`     | yields once; never waits               |
//! | [`timeout`]              | `tokio::time`     | awaits the future; never times out     |
//! | [`interval`]             | `tokio::time`     | never ticks                            |
//!
//! [`SystemTime`] stays `std::time::SystemTime` on every target (only `now()`
//! is missing on wasm, and `fs::Metadata`, chrono and the telemetry protos all
//! speak the std type), so the rewrite only replaces the *call*
//! `SystemTime::now()` with [`system_now`]. `Instant` has no public constructor
//! but `now()`, so the *type* changes on wasm.
//!
//! Why the wasm timers never wait: a browser host runs dbt on a current-thread
//! tokio runtime with no time driver. A future that waits on a timer would leave
//! the runtime with nothing ready, and an idle current-thread runtime parks the
//! thread, which on wasm32 panics or never returns. So waits become yields: a
//! retry retries at once, a poll loop polls again on its next turn, and a
//! timeout is never the reason a future stops.
//!
//! On `wasm32-wasip1` std has a clock and `web_time` re-exports it; the
//! timers follow the same no-wait rule on every wasm32 target, matching the
//! runtime `dbt-main` builds there.

pub use std::time::*;

#[cfg(target_arch = "wasm32")]
pub use web_time::Instant;

/// `SystemTime::now()`, on every target. On wasm32-unknown-unknown the std
/// type is built from the JS clock (`Date.now()` via `web_time`).
#[cfg(not(target_arch = "wasm32"))]
#[inline]
pub fn system_now() -> SystemTime {
    SystemTime::now()
}

/// `SystemTime::now()`, on every target. On wasm32-unknown-unknown the std
/// type is built from the JS clock (`Date.now()` via `web_time`).
#[cfg(target_arch = "wasm32")]
pub fn system_now() -> SystemTime {
    let since = web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .unwrap_or_default();
    UNIX_EPOCH + since
}

#[cfg(not(target_arch = "wasm32"))]
pub use tokio::time::{Interval, MissedTickBehavior, error::Elapsed, interval, sleep, timeout};

#[cfg(target_arch = "wasm32")]
pub use self::nowait::{Elapsed, Interval, MissedTickBehavior, interval, sleep, timeout};

#[cfg(any(target_arch = "wasm32", test))]
mod nowait {
    use std::future::Future;
    use std::time::Duration;

    /// Yields once and returns: a wasm host has no timer to wait on.
    pub async fn sleep(_duration: Duration) {
        tokio::task::yield_now().await;
    }

    /// Awaits `future` to completion: a wasm host has no timer to race it with.
    pub async fn timeout<F: Future>(_duration: Duration, future: F) -> Result<F::Output, Elapsed> {
        Ok(future.await)
    }

    /// `tokio::time::error::Elapsed`'s shape; never produced on wasm.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Elapsed(());

    impl std::fmt::Display for Elapsed {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("deadline has elapsed")
        }
    }

    impl std::error::Error for Elapsed {}

    /// `tokio::time::MissedTickBehavior`'s shape; ignored on wasm.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum MissedTickBehavior {
        #[default]
        Burst,
        Delay,
        Skip,
    }

    /// An interval that never ticks: on wasm a periodic wake-up would need a
    /// timer, so whatever an interval flushes is flushed by its other triggers.
    #[derive(Debug)]
    pub struct Interval {
        _period: Duration,
    }

    pub fn interval(period: Duration) -> Interval {
        Interval { _period: period }
    }

    impl Interval {
        pub async fn tick(&mut self) -> super::Instant {
            std::future::pending().await
        }

        pub fn set_missed_tick_behavior(&mut self, _behavior: MissedTickBehavior) {}
    }
}

#[cfg(test)]
mod tests {
    use super::nowait;
    use std::time::Duration;

    #[test]
    fn system_now_is_after_2020() {
        let secs = super::system_now()
            .duration_since(super::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(secs > 1_577_836_800, "{secs}");
    }

    #[tokio::test]
    async fn nowait_timers_do_not_wait() {
        let t0 = super::Instant::now();
        nowait::sleep(Duration::from_secs(3600)).await;
        let out = nowait::timeout(Duration::from_nanos(1), async { 7 }).await;
        assert_eq!(out, Ok(7));
        assert!(t0.elapsed() < Duration::from_secs(60));
        let mut iv = nowait::interval(Duration::from_millis(1));
        iv.set_missed_tick_behavior(nowait::MissedTickBehavior::Delay);
        tokio::select! {
            _ = iv.tick() => panic!("a wasm interval never ticks"),
            _ = tokio::task::yield_now() => {}
        }
    }
}
