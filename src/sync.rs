//! Small synchronization helpers shared across the library.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Locks `mutex`, recovering the guard if a previous holder panicked.
///
/// The cache and IP tracker hold soft state: if a request's task panics mid-update, every
/// later request is better served by the possibly stale state than by a cascade of panics
/// from a poisoned lock.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
