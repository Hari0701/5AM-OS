//! A lock, and the reason a kernel's lock is not a userspace lock.
//!
//! For most of this project's life there was not a single lock in it. That was
//! not an oversight so much as a fact that stopped being true: with one core
//! and a kernel that never got preempted, `without_interrupts` was a complete
//! critical section. Preemption ended that, and running two things at once made
//! it visible — two tasks printing at the same time produced output interleaved
//! mid-word, and two generations would have written over each other's
//! activations.
//!
//! ## Why a plain spinlock deadlocks a kernel
//!
//! Take a lock in ordinary code. The timer fires. The handler runs, on the same
//! core, and tries to take the same lock. It spins waiting for a holder that
//! cannot possibly release it, because the only core that could is the one now
//! spinning inside the interrupt handler.
//!
//! This is not a rare interleaving. It is a guaranteed hang the moment an
//! interrupt handler touches the same data as ordinary code, and it happens
//! whether or not the machine has more than one core — the bug is *re-entrancy*,
//! not parallelism.
//!
//! The fix is that acquiring a kernel lock disables interrupts, and releasing it
//! restores them. That is what [`SpinLock`] does, and it is why the guard has to
//! remember the previous interrupt state rather than blindly enabling them:
//! locks nest, and the inner release must not turn interrupts back on while the
//! outer critical section is still running.
//!
//! ## The atomic is still necessary
//!
//! With interrupts off on a single core, nothing else can run, so the
//! `AtomicBool` looks like ceremony. It is doing two real jobs. It catches
//! *self*-deadlock — code that takes a lock it already holds — turning a silent
//! hang into something [`try_lock`](SpinLock::try_lock) can report. And it is
//! the part that will still be correct when a second core exists, where
//! disabling interrupts protects nothing at all.

use crate::interrupts;
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

pub struct SpinLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

/// # Safety
/// The lock is what makes the shared access safe: only one holder at a time,
/// and interrupts are off for the duration so nothing on this core can reenter.
unsafe impl<T: Send> Sync for SpinLock<T> {}
unsafe impl<T: Send> Send for SpinLock<T> {}

pub struct Guard<'a, T> {
    lock: &'a SpinLock<T>,
    /// Whether interrupts were on before we took the lock. Restoring this,
    /// rather than simply enabling, is what makes nested locks safe.
    interrupts_were_enabled: bool,
}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    /// Take the lock, waiting if necessary.
    ///
    /// The wait is a spin, which is only defensible because every critical
    /// section under it is short and interrupts are off. A lock held across
    /// something slow -- a disk read, a neural network -- must not use this;
    /// see `try_lock`.
    pub fn lock(&self) -> Guard<'_, T> {
        loop {
            if let Some(guard) = self.try_lock() {
                return guard;
            }
            // Tell the CPU this is a spin loop. On a single core with
            // interrupts disabled we can never actually get here, which is
            // itself the argument for try_lock being the honest interface.
            core::hint::spin_loop();
        }
    }

    /// Take the lock if it is free, and say so if it is not.
    ///
    /// Preferred over `lock` anywhere a caller can do something sensible with
    /// "busy", because on one core a failed `lock()` never succeeds.
    pub fn try_lock(&self) -> Option<Guard<'_, T>> {
        let interrupts_were_enabled = interrupts::are_enabled();
        interrupts::disable();

        // compare_exchange, not "read then write": those are two instructions
        // with a window between them, which is the entire class of bug locks
        // exist to prevent.
        match self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        {
            Ok(_) => Some(Guard {
                lock: self,
                interrupts_were_enabled,
            }),
            Err(_) => {
                if interrupts_were_enabled {
                    interrupts::enable();
                }
                None
            }
        }
    }

    /// Break the lock open regardless of who holds it.
    ///
    /// Only for the fault handlers. A kernel that has decided to print a panic
    /// and stop has one remaining duty -- say what happened -- and blocking on
    /// a lock held by the code that just died fails it silently. Linux does the
    /// same thing for the same reason.
    ///
    /// # Safety
    /// Whoever held the lock still believes they do.
    pub unsafe fn force_unlock(&self) {
        self.locked.store(false, Ordering::Release);
    }

    /// Reach the data without locking, for the same panic path.
    ///
    /// # Safety
    /// No exclusion at all. Correct only when nothing else will run again.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn get_unchecked(&self) -> &mut T {
        unsafe { &mut *self.value.get() }
    }
}

impl<T> Deref for Guard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for Guard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for Guard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
        // Restore, do not enable. If this lock was taken inside another one,
        // turning interrupts on here would end the outer critical section
        // early -- a bug that only shows up under load and looks like memory
        // corruption rather than a locking mistake.
        if self.interrupts_were_enabled {
            interrupts::enable();
        }
    }
}

// --- claims --------------------------------------------------------------

/// Exclusive ownership of something slow, without disabling interrupts.
///
/// [`SpinLock`] is wrong for a long critical section. It turns interrupts off
/// for as long as it is held, and holding it across a fifteen-second neural
/// network run would suspend the timer for fifteen seconds -- no preemption, no
/// scheduler, a frozen machine that technically has one.
///
/// So this is the other half of the pair, and the distinction is one every
/// kernel makes: a spinlock for short things you must not be interrupted
/// during, and something that *yields or refuses* for long things. A `Claim`
/// refuses. The holder can be preempted freely; a second caller is simply told
/// no, which is a far better outcome than two tasks writing into one set of
/// activations and producing plausible nonsense.
///
/// Refusing rather than waiting is only honest because there is nothing useful
/// to wait *on* yet. A blocked state and a wait queue are what turn this into
/// "sleep until the model is free", and they do not exist here.
pub struct Claim {
    taken: AtomicBool,
}

pub struct ClaimGuard<'a> {
    claim: &'a Claim,
}

impl Claim {
    pub const fn new() -> Self {
        Self {
            taken: AtomicBool::new(false),
        }
    }

    /// Take ownership, or return None if someone else already has it.
    pub fn try_take(&self) -> Option<ClaimGuard<'_>> {
        // Interrupts stay on. The whole point is that the holder is
        // interruptible -- and the exchange is atomic, so being interrupted
        // between the test and the set is not a window that exists.
        match self
            .taken
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        {
            Ok(_) => Some(ClaimGuard { claim: self }),
            Err(_) => None,
        }
    }

    pub fn is_taken(&self) -> bool {
        self.taken.load(Ordering::Relaxed)
    }
}

impl Default for Claim {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ClaimGuard<'_> {
    fn drop(&mut self) {
        // Released on every exit path, including the early returns in the
        // middle of generate(). That is the entire reason this is a guard and
        // not a pair of functions.
        self.claim.taken.store(false, Ordering::Release);
    }
}
