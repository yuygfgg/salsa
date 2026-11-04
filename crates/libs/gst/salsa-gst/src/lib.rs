//! Small helpers shared by the Salsa GStreamer plugins.
//!
//! The plugins keep their state behind a `Mutex`. A panic in one streaming
//! thread would otherwise poison the mutex and turn every later property or
//! state callback into a second panic, so the plugins recover the guard instead
//! of unwrapping the lock result.

use std::sync::{Mutex, MutexGuard};

/// Lock `mutex`, recovering the guard if a previous holder panicked.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_a_poisoned_lock() {
        let mutex = Mutex::new(1);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = lock(&mutex);
            panic!("poison the mutex");
        }));
        assert_eq!(*lock(&mutex), 1);
    }
}
