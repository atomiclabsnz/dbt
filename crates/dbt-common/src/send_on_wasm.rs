//! ferrion-wasm: a `Send` assertion for futures that are `!Send` only on wasm.
//!
//! dbt's async seams (`#[async_trait]` traits, `tokio::spawn`) require `Send`
//! futures. On wasm, reqwest is backed by `fetch`, whose futures and responses
//! hold `JsValue`s and are therefore `!Send`; every future that awaits one
//! inherits that. A wasm32 build without the `atomics` target feature cannot
//! create a second thread that shares this memory, so no value can ever be
//! observed from another thread, and asserting `Send` is sound there.
//!
//! [`send_on_wasm`] wraps a future at the boundary that demands `Send`. On
//! native targets it is the identity function and still requires `F: Send`, so
//! the native type checking is exactly what it was.

use std::future::Future;

/// Identity on native targets; the future must already be `Send`.
#[cfg(not(target_arch = "wasm32"))]
#[inline(always)]
pub fn send_on_wasm<F: Future + Send>(future: F) -> F {
    future
}

/// On wasm (without shared-memory threads), assert that `future` is `Send`.
#[cfg(all(target_arch = "wasm32", not(target_feature = "atomics")))]
#[inline(always)]
pub fn send_on_wasm<F: Future>(future: F) -> SendOnWasm<F> {
    SendOnWasm(future)
}

#[cfg(all(target_arch = "wasm32", not(target_feature = "atomics")))]
pub struct SendOnWasm<F>(F);

// SAFETY: see the module docs. Without the `atomics` target feature a wasm32
// module is single-threaded: there is no other thread to send this value to.
#[cfg(all(target_arch = "wasm32", not(target_feature = "atomics")))]
unsafe impl<F> Send for SendOnWasm<F> {}

#[cfg(all(target_arch = "wasm32", not(target_feature = "atomics")))]
impl<F: Future> Future for SendOnWasm<F> {
    type Output = F::Output;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        // SAFETY: structural pinning of the only field; `SendOnWasm` never
        // moves it out and has no `Drop` impl.
        unsafe { self.map_unchecked_mut(|s| &mut s.0) }.poll(cx)
    }
}
