use std::sync::{Mutex, MutexGuard};

/// Lock a mutex, recovering from poisoning. If a thread panicked while holding
/// the lock, we still want to access the data rather than propagating the panic.
pub fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
