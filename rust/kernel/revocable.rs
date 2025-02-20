// SPDX-License-Identifier: GPL-2.0

//! Revocable objects.
//!
//! The [`Revocable`] type wraps other types and allows access to them to be revoked. The existence
//! of a [`RevocableGuard`] ensures that objects remain valid.

use crate::bindings;
use core::{
    cell::UnsafeCell,
    marker::PhantomData,
    mem::ManuallyDrop,
    ops::Deref,
    ptr::drop_in_place,
    sync::atomic::{AtomicBool, Ordering},
};

/// An object that can become inaccessible at runtime.
///
/// Once access is revoked and all concurrent users complete (i.e., all existing instances of
/// [`RevocableGuard`] are dropped), the wrapped object is also dropped.
///
/// # Examples
///
/// ```
/// # use kernel::revocable::Revocable;
///
/// struct Example {
///     a: u32,
///     b: u32,
/// }
///
/// fn add_two(v: &Revocable<Example>) -> Option<u32> {
///     let guard = v.try_access()?;
///     Some(guard.a + guard.b)
/// }
///
/// fn example() {
///     let v = Revocable::new(Example { a: 10, b: 20 });
///     assert_eq!(add_two(&v), Some(30));
///     v.revoke();
///     assert_eq!(add_two(&v), None);
/// }
/// ```
pub struct Revocable<T: ?Sized> {
    is_available: AtomicBool,
    data: ManuallyDrop<UnsafeCell<T>>,
}

// SAFETY: `Revocable` is `Send` if the wrapped object is also `Send`. This is because while the
// functionality exposed by `Revocable` can be accessed from any thread/CPU, it is possible that
// this isn't supported by the wrapped object.
unsafe impl<T: ?Sized + Send> Send for Revocable<T> {}

// SAFETY: `Revocable` is `Sync` if the wrapped object is both `Send` and `Sync`. We require `Send`
// from the wrapped object as well because  of `Revocable::revoke`, which can trigger the `Drop`
// implementation of the wrapped object from an arbitrary thread.
unsafe impl<T: ?Sized + Sync + Send> Sync for Revocable<T> {}

impl<T> Revocable<T> {
    /// Creates a new revocable instance of the given data.
    pub fn new(data: T) -> Self {
        Self {
            is_available: AtomicBool::new(true),
            data: ManuallyDrop::new(UnsafeCell::new(data)),
        }
    }

    /// Tries to access the \[revocable\] wrapped object.
    ///
    /// Returns `None` if the object has been revoked and is therefore no longer accessible.
    ///
    /// Returns a guard that gives access to the object otherwise; the object is guaranteed to
    /// remain accessible while the guard is alive. In such cases, callers are not allowed to sleep
    /// because another CPU may be waiting to complete the revocation of this object.
    pub fn try_access(&self) -> Option<RevocableGuard<'_, T>> {
        let guard = RevocableGuard::new(self.data.get());
        if self.is_available.load(Ordering::Relaxed) {
            Some(guard)
        } else {
            None
        }
    }

    /// Revokes access to and drops the wrapped object.
    ///
    /// Access to the object is revoked immediately to new callers of [`Revocable::try_access`]. If
    /// there are concurrent users of the object (i.e., ones that called [`Revocable::try_access`]
    /// beforehand and still haven't dropped the returned guard), this function waits for the
    /// concurrent access to complete before dropping the wrapped object.
    pub fn revoke(&self) {
        if self
            .is_available
            .compare_exchange(true, false, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            // SAFETY: Just an FFI call, there are no further requirements.
            unsafe { bindings::synchronize_rcu() };

            // SAFETY: We know `self.data` is valid because only one CPU can succeed the
            // `compare_exchange` above that takes `is_available` from `true` to `false`.
            unsafe { drop_in_place(self.data.get()) };
        }
    }
}

impl<T: ?Sized> Drop for Revocable<T> {
    fn drop(&mut self) {
        // Drop only if the data hasn't been revoked yet (in which case it has already been
        // dropped).
        if *self.is_available.get_mut() {
            // SAFETY: We know `self.data` is valid because no other CPU has changed
            // `is_available` to `false` yet, and no other CPU can do it anymore because this CPU
            // holds the only reference (mutable) to `self` now.
            unsafe { drop_in_place(self.data.get()) };
        }
    }
}

/// A guard that allows access to a revocable object and keeps it alive.
///
/// CPUs may not sleep while holding on to [`RevocableGuard`] because it's in atomic context
/// holding the RCU read-side lock.
///
/// # Invariants
///
/// The RCU read-side lock is held while the guard is alive.
pub struct RevocableGuard<'a, T> {
    data_ref: *const T,
    _p: PhantomData<&'a ()>,
}

impl<T> RevocableGuard<'_, T> {
    fn new(data_ref: *const T) -> Self {
        // SAFETY: Just an FFI call, there are no further requirements.
        unsafe { bindings::rcu_read_lock() };

        // INVARIANTS: The RCU read-side lock was just acquired.
        Self {
            data_ref,
            _p: PhantomData,
        }
    }
}

impl<T> Drop for RevocableGuard<'_, T> {
    fn drop(&mut self) {
        // SAFETY: By the type invariants, we know that we hold the RCU read-side lock.
        unsafe { bindings::rcu_read_unlock() };
    }
}

impl<T> Deref for RevocableGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: By the type invariants, we hold the rcu read-side lock, so the object is
        // guaranteed to remain valid.
        unsafe { &*self.data_ref }
    }
}
