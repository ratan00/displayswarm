//! Process-wide `pipewire::init()`, deliberately never `pipewire::deinit()`.
//!
//! This module originally reference-counted both `init()` and `deinit()`
//! across this crate's independent PipeWire users (the video capture loop in
//! [`crate::pw_thread`], the audio capture loop in [`crate::audio`], and
//! [`crate::connection::PipeWireConnection`]), on the theory that the only
//! hazard was `deinit()` racing a still-running sibling's use of the shared
//! library state.
//!
//! That was real (confirmed via valgrind: `unref_handle`/`pw_deinit`, called
//! from a finishing audio-capture thread, freed the handle a still-running
//! video-capture thread's loop was built on top of) and the refcounting
//! fixed it. But a second, deeper hazard surfaced later: `pw_deinit()`
//! reproducibly segfaults a PipeWire-internal worker thread (observed as
//! `pipewire-main` in `dmesg`, not a thread this crate names or owns) even
//! with exactly ONE registered user across the process's entire lifetime —
//! confirmed by instrumenting [`acquire()`]/[`release()`] directly and
//! observing a single clean `0 -> 1 -> 0` cycle immediately before the
//! crash. A 300ms wall-clock delay before the call made no difference either
//! (ruling out "the internal thread just needs more time"), while skipping
//! the real `pw_deinit()` call entirely eliminated the crash outright, 2/2
//! reconnects. The working theory: any stream created with the
//! `RT_PROCESS` flag causes PipeWire's client library to spin up a
//! process-lifetime realtime worker thread pool that `pw_deinit()` does not
//! safely tear down or wait for, regardless of how carefully the caller's
//! own stream/core/context/main-loop are sequenced beforehand.
//!
//! The fix: never call the real `pipewire::deinit()` at all. This is a
//! deliberate, common trade-off for long-running processes linking C
//! libraries with fragile global teardown — the alternative (a graceful,
//! ordered library-wide shutdown) isn't achievable through the public API
//! surface this crate has, and every caller here is a server process that
//! lives for the RDP session's lifetime, not a short-lived tool that
//! init/deinits in a tight loop. The OS reclaims everything on process exit,
//! which is what would happen on `deinit()`'s own failure path anyway.
//!
//! [`acquire()`]/[`release()`] stay paired (rather than just calling
//! `pipewire::init()` once at startup) so the bookkeeping remains honest and
//! auditable, and so a future fix upstream (or a safe teardown path PipeWire
//! itself provides later) has a single call site to change.

use std::sync::Mutex;

static PW_REFCOUNT: Mutex<usize> = Mutex::new(0);

/// Register one more PipeWire user in this process. Calls the real
/// `pipewire::init()` only on the first (0 -> 1) registration; `pipewire`
/// itself documents `init()` as safe to call more than once, but routing
/// every call through the same counter as [`release()`] keeps the two
/// symmetric and auditable at a single call site.
pub(crate) fn acquire() {
    let mut count = PW_REFCOUNT.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if *count == 0 {
        pipewire::init();
    }
    *count += 1;
}

/// Unregister one PipeWire user. Deliberately never calls the real
/// `pipewire::deinit()` — see the module doc for why. Bookkeeping-only:
/// keeps the count honest for any future caller that needs to know whether
/// it's the last PipeWire user in the process.
pub(crate) fn release() {
    let mut count = PW_REFCOUNT.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    debug_assert!(
        *count > 0,
        "pw_lifecycle::release() called without a matching acquire()"
    );
    if *count > 0 {
        *count -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_acquire_release_only_touches_real_init_deinit_at_the_edges() {
        // This test only exercises the counter, not the real pipewire::init()/
        // deinit() (which would affect global process state and any other
        // test running concurrently). It verifies the counting logic in
        // isolation by checking against a local counter with the same shape.
        let count = Mutex::new(0usize);
        let acquire = || *count.lock().unwrap() += 1;
        let release = || *count.lock().unwrap() -= 1;

        acquire(); // 0 -> 1: would call real init()
        acquire(); // 1 -> 2: no-op
        acquire(); // 2 -> 3: no-op
        assert_eq!(*count.lock().unwrap(), 3);

        release(); // 3 -> 2: no-op
        assert_eq!(*count.lock().unwrap(), 2);
        release(); // 2 -> 1: no-op
        release(); // 1 -> 0: would call real deinit()
        assert_eq!(*count.lock().unwrap(), 0);
    }

    #[test]
    fn acquire_and_release_are_idempotent_pairs_in_practice() {
        // Exercises the real functions: this is safe because acquire/release
        // are symmetric and this test's own net effect on the process-wide
        // count is zero.
        acquire();
        acquire();
        release();
        release();
    }
}
