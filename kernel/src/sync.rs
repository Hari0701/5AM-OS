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
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

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

// --- semaphore -----------------------------------------------------------

/// A counter you can wait on, built on the scheduler's blocked state.
///
/// This is the primitive [`SpinLock`] and [`Claim`] could not be. A spinlock
/// burns the CPU while it waits and a claim refuses instead of waiting; a
/// semaphore *sleeps*, which is only expressible once a task can be told "not
/// runnable until somebody says so".
///
/// With a count of one it is a mutex that blocks rather than spins -- the right
/// thing to hold across a disk read. With a count of N it is a permit pool. And
/// the classic pairing of two, one counting full slots and one counting empty
/// ones, is a bounded producer-consumer queue.
///
/// ## Why the loop around the wait
///
/// `wake_all` wakes every waiter, and only one of them can win the count. The
/// others must re-test rather than assume, which is the same reason condition
/// variables are always used in a `while` and never an `if`. A wakeup is a hint
/// that the world may have changed, never a promise that it changed for you.
pub struct Semaphore {
    /// Atomic, and that is not decoration on a single-core kernel.
    ///
    /// The first version was a plain `u32` in an `UnsafeCell`, read and written
    /// with interrupts disabled -- which is correct mutual exclusion and still
    /// completely broken. `wait` re-tests this value in a loop, and nothing in
    /// a plain load tells the compiler it can change: no other thread it can
    /// see writes it, so LLVM hoists the load out of the loop and the waiter
    /// spins forever on a register holding zero, while the real count sits at
    /// one.
    ///
    /// Disabling interrupts stops another *task* from running. It says nothing
    /// to the *compiler*. Those are different problems and they need different
    /// tools, which is the entire lesson of this field.
    count: AtomicU32,
}

impl Semaphore {
    pub const fn new(count: u32) -> Self {
        Self {
            count: AtomicU32::new(count),
        }
    }

    /// The address of this semaphore, used as the wait channel. Two different
    /// semaphores can never collide, and no registry is needed to say so.
    fn channel(&self) -> u64 {
        self as *const _ as u64
    }

    /// Take one, sleeping until one is available.
    ///
    /// The test and the block are a single uninterruptible step -- see
    /// `block_until`. Separating them is the lost wakeup, and it deadlocked
    /// this exact demo the first time it ran.
    pub fn wait(&self) {
        crate::task::block_until(self.channel(), || self.try_take())
    }

    /// Decrement if positive, atomically.
    ///
    /// A load, a compare and a store as one indivisible step. Written as three
    /// separate operations it would be the textbook lost-update race, which is
    /// the very thing this type exists to prevent.
    fn try_take(&self) -> Option<()> {
        let mut current = self.count.load(Ordering::Acquire);
        loop {
            if current == 0 {
                return None;
            }
            match self.count.compare_exchange_weak(
                current,
                current - 1,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(()),
                // Somebody moved it under us. Re-read and try again rather
                // than assuming the value we last saw.
                Err(actual) => current = actual,
            }
        }
    }

    /// Take one only if it is free.
    pub fn try_wait(&self) -> bool {
        self.try_take().is_some()
    }

    /// Give one back and wake anybody waiting.
    ///
    /// The count goes up before the wake, never after: a waiter that runs the
    /// instant it is made runnable must find the resource already there.
    pub fn signal(&self) {
        self.count.fetch_add(1, Ordering::Release);
        interrupts::without_interrupts(|| {
            crate::task::wake_all(self.channel());
        })
    }

    pub fn count(&self) -> u32 {
        self.count.load(Ordering::Acquire)
    }
}
