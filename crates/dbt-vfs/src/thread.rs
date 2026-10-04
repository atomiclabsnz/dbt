//! ferrion-wasm: the thread seam.
//!
//! On `wasm32-unknown-unknown` (no atomics) there is one thread: `std::thread::spawn`
//! compiles and panics, `thread::sleep` panics or blocks the only thread, and
//! tokio's `spawn_blocking` cannot start its pool. `scripts/wasm-runtime-rewrite.py`
//! routes the run-path crates through this module instead.
//!
//! | item                 | native                           | wasm32                               |
//! |----------------------|----------------------------------|--------------------------------------|
//! | [`sleep`]            | `std::thread::sleep`             | returns at once (never blocks)       |
//! | [`spawn_blocking`]   | `tokio::task::spawn_blocking`    | `tokio::spawn` of the closure        |
//! | [`spawn`]            | `std::thread::spawn`             | the closure runs when it is joined   |
//! | [`scope`]            | `std::thread::scope`             | each scoped closure runs inline      |
//!
//! [`spawn`] on wasm is for the shape dbt uses it for: a consumer that drains a
//! channel until a shutdown message and is then joined (the telemetry/log
//! writers). With one thread nothing can drain the channel concurrently, so the
//! body runs at `join()`, when its shutdown message is already queued; until
//! then sends just queue. A thread that is never joined never runs on wasm.

#[cfg(not(target_arch = "wasm32"))]
pub use std::thread::{JoinHandle, Scope, ScopedJoinHandle, scope, sleep, spawn};
#[cfg(not(target_arch = "wasm32"))]
pub use tokio::task::spawn_blocking;

#[cfg(target_arch = "wasm32")]
pub use self::single::{JoinHandle, Scope, ScopedJoinHandle, scope, sleep, spawn, spawn_blocking};

#[cfg(any(target_arch = "wasm32", test))]
mod single {
    use std::marker::PhantomData;
    use std::time::Duration;

    /// Returns at once: blocking the only thread would block the whole host.
    pub fn sleep(_duration: Duration) {}

    /// Runs `f` as an ordinary task on the current runtime (same `JoinHandle`
    /// type as `tokio::task::spawn_blocking`).
    pub fn spawn_blocking<F, R>(f: F) -> tokio::task::JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        tokio::spawn(async move { f() })
    }

    /// A "thread" that runs when it is joined (see the module docs). The
    /// `Mutex` only makes it `Sync`, as `std::thread::JoinHandle` is.
    pub struct JoinHandle<T>(std::sync::Mutex<Box<dyn FnOnce() -> T + Send + 'static>>);

    pub fn spawn<F, T>(f: F) -> JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        JoinHandle(std::sync::Mutex::new(Box::new(f)))
    }

    impl<T> JoinHandle<T> {
        pub fn join(self) -> std::thread::Result<T> {
            let f = self.0.into_inner().unwrap_or_else(|p| p.into_inner());
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
        }

        pub fn is_finished(&self) -> bool {
            false
        }
    }

    impl<T> std::fmt::Debug for JoinHandle<T> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("JoinHandle(deferred)")
        }
    }

    /// `std::thread::Scope`'s shape: every spawned closure runs inline.
    pub struct Scope<'scope, 'env: 'scope> {
        _scope: PhantomData<&'scope mut &'scope ()>,
        _env: PhantomData<&'env mut &'env ()>,
    }

    pub struct ScopedJoinHandle<'scope, T>(std::thread::Result<T>, PhantomData<&'scope ()>);

    pub fn scope<'env, F, T>(f: F) -> T
    where
        F: for<'scope> FnOnce(&'scope Scope<'scope, 'env>) -> T,
    {
        let scope = Scope {
            _scope: PhantomData,
            _env: PhantomData,
        };
        f(&scope)
    }

    impl<'scope, 'env> Scope<'scope, 'env> {
        pub fn spawn<F, T>(&'scope self, f: F) -> ScopedJoinHandle<'scope, T>
        where
            F: FnOnce() -> T + Send + 'scope,
            T: Send + 'scope,
        {
            ScopedJoinHandle(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)),
                PhantomData,
            )
        }
    }

    impl<T> ScopedJoinHandle<'_, T> {
        pub fn join(self) -> std::thread::Result<T> {
            self.0
        }

        pub fn is_finished(&self) -> bool {
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::single;
    use std::sync::mpsc;

    #[test]
    fn deferred_spawn_drains_its_channel_at_join() {
        let (tx, rx) = mpsc::channel::<Option<u32>>();
        let h = single::spawn(move || {
            let mut sum = 0;
            while let Ok(Some(v)) = rx.recv() {
                sum += v;
            }
            sum
        });
        assert!(!h.is_finished());
        for v in [1, 2, 3] {
            tx.send(Some(v)).unwrap();
        }
        tx.send(None).unwrap();
        assert_eq!(h.join().unwrap(), 6);
    }

    #[test]
    fn scope_runs_inline_and_borrows() {
        let data = [1, 2, 3];
        let total = single::scope(|s| {
            let a = s.spawn(|| data.iter().sum::<i32>());
            let b = s.spawn(|| data.len());
            (a.join().unwrap(), b.join().unwrap())
        });
        assert_eq!(total, (6, 3));
    }

    #[test]
    fn sleep_returns_at_once() {
        let t0 = std::time::Instant::now();
        single::sleep(std::time::Duration::from_secs(3600));
        assert!(t0.elapsed() < std::time::Duration::from_secs(60));
    }

    #[tokio::test]
    async fn spawn_blocking_runs_as_a_task() {
        let v = single::spawn_blocking(|| 41 + 1).await.unwrap();
        assert_eq!(v, 42);
    }
}
