//! The one way this crate gets a tokio reactor.
//!
//! gpui drives its own executor and never installs a tokio reactor, so no
//! async work in this crate can run on the caller's thread — every I/O-owning
//! worker here (a bridge's daemon connection, the machines worker) instead
//! lives on a std thread of its own that builds a runtime and blocks on one
//! future for its whole life. Current-thread on purpose: one worker is one
//! logical actor, so a work-stealing pool would buy nothing and cost threads.
//!
//! Factored out because the *failure* case is the interesting half. Building a
//! runtime fails only when the OS refuses a resource, and at that point the
//! worker cannot start at all — so its owner must be told in whatever way that
//! worker reports failure, which is the only thing the two call sites disagree
//! about (one sends a `ConnectFailed` event, one logs to stderr).

use std::future::Future;

/// Build this thread's runtime and block on `main` until it finishes; the
/// thread ends with it.
///
/// If the runtime cannot be built, `on_runtime_error` is handed the formatted
/// error and `main` never runs — it is dropped un-polled, which is safe
/// because a worker's main future does all its work after the first poll.
pub(crate) fn run_on_tokio_thread(
    main: impl Future<Output = ()>,
    on_runtime_error: impl FnOnce(String),
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            on_runtime_error(format!("tokio runtime: {error}"));

            return;
        }
    };

    runtime.block_on(main);
}
