//! Minimal synchronization primitives.
//!
//! For now this is a basic spinlock, sufficient for the single-core kernel.
//! It is **interrupt-safe**: holding the lock disables interrupts on the local
//! core (restoring their prior state on release), so a handler cannot preempt a
//! holder and deadlock re-acquiring the same lock (#62). The fuller set of
//! primitives - mutexes, wait queues - arrives with the scheduler in M4.
//!
//! Lock ordering: locks taken under another lock must sit strictly below it in
//! a fixed hierarchy. In particular the frame-allocator and page-table locks
//! are below the heap lock, and code holding them must not allocate. The
//! kernel's own locks nest in this order, outermost first: `IPC`, `CSPACES`,
//! `SCHED`, the thread table's `SPACES`, `KERNEL_SPACE`, then the frame
//! allocator. No lock is held across a context switch.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

/// A mutual-exclusion spinlock protecting a value of type `T`.
pub struct SpinLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: all access to the inner value goes through `lock()`, which serializes
// callers via the `locked` flag, so a `SpinLock<T>` can be shared across cores
// when `T` is `Send`.
unsafe impl<T: Send> Sync for SpinLock<T> {}
// SAFETY: moving the lock moves its `T`; sound whenever `T` is `Send`.
unsafe impl<T: Send> Send for SpinLock<T> {}

impl<T> SpinLock<T> {
    /// Create a new, unlocked spinlock wrapping `value`.
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    /// Acquire the lock, spinning until it is free. The returned guard releases
    /// the lock when dropped.
    ///
    /// Interrupts are disabled on this core for as long as the guard is held
    /// (and restored to their prior state on drop). This makes the lock safe to
    /// take in both ordinary and interrupt context: a handler on the same core
    /// cannot preempt a lock holder and deadlock trying to re-acquire it.
    pub fn lock(&self) -> SpinGuard<'_, T> {
        // Save-and-disable interrupts *before* acquiring, so that once we hold
        // the lock no interrupt on this core can run and try to take it.
        // `cli` is a compiler barrier (not `nomem`, see `hal::cpu`), so the
        // acquire cannot be scheduled ahead of it (#110).
        let saved_if = crate::cpu::interrupts_enabled();
        crate::cpu::disable_interrupts();
        while self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            // Spin read-only until the lock looks free, to avoid hammering the
            // cache line with atomic RMW operations.
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
        SpinGuard {
            lock: self,
            saved_if,
        }
    }
}

/// RAII guard that releases its [`SpinLock`] when dropped and derefs to the
/// protected value.
pub struct SpinGuard<'a, T> {
    lock: &'a SpinLock<T>,
    /// Whether interrupts were enabled when the lock was taken; restored on drop.
    saved_if: bool,
}

impl<T> Deref for SpinGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: holding this guard means we hold the lock, so we have
        // exclusive access to the inner value for the guard's lifetime.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for SpinGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as in `deref`; exclusive access is guaranteed while the guard
        // is alive.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for SpinGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
        // Restore the interrupt state we found on acquisition. We only ever
        // re-enable; we never enable interrupts that were disabled before.
        // `sti` is a compiler barrier too, so the release store above stays
        // ahead of it.
        if self.saved_if {
            crate::cpu::enable_interrupts();
        }
    }
}

/// A lock that is only ever **tried**, and that leaves interrupts alone: for
/// state a single drainer owns, such as the framebuffer console, which the
/// logger writes with interrupts on (#116). A failed [`try_lock`] means
/// another holder is active; the caller skips its work instead of waiting.
///
/// Never take a `TryLock` in interrupt context: an interrupt can arrive while
/// it is held, and the handler would fail to get it.
///
/// [`try_lock`]: Self::try_lock
pub struct TryLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: all access to the inner value goes through `try_lock()`, which grants
// it to one holder at a time via the `locked` flag.
unsafe impl<T: Send> Sync for TryLock<T> {}
// SAFETY: moving the lock moves its `T`; sound whenever `T` is `Send`.
unsafe impl<T: Send> Send for TryLock<T> {}

impl<T> TryLock<T> {
    /// Create a new, unlocked `TryLock` wrapping `value`.
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    /// Take the lock if it is free, without spinning or touching interrupts.
    pub fn try_lock(&self) -> Option<TryGuard<'_, T>> {
        self.locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| TryGuard { lock: self })
    }
}

/// Holds a [`TryLock`] until dropped, and derefs to its value.
pub struct TryGuard<'a, T> {
    lock: &'a TryLock<T>,
}

impl<T> Deref for TryGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: holding the guard means holding the lock: exclusive access.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for TryGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as in `deref`.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for TryGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}
