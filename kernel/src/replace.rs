//! Which page leaves memory, separated from the business of making it leave.
//!
//! `memory.rs` knows how to evict a page: allocate a swap slot, write the frame
//! out, rewrite the entry so it names the slot instead of the frame, flush the
//! TLB. None of that depends on *which* page was chosen. It is the same work
//! whichever answer comes back.
//!
//! The choosing is the part that is a design decision, and it is a famously
//! unsolved one. The best page to evict is the one that will be needed furthest
//! in the future, which is unknowable, so every real algorithm is a guess at
//! "least recently used" assembled from the little the hardware records.
//!
//! ## What the hardware records
//!
//! One bit. The CPU sets `accessed` in a page table entry whenever it
//! translates through it, and never clears it. That is the entire input. Every
//! policy below is a different answer to "how do I make one bit enough?", and
//! the whole field exists because one bit is not very much.
//!
//! ## The seam
//!
//! A policy is handed a [`PageSet`] — the resident user pages of one address
//! space — and returns an index into it. It may read the accessed bit and it
//! may *clear* it, which is the one mutation permitted here because clearing is
//! harmless: the CPU simply sets it again on the next touch, and a policy that
//! could not clear it would have no way to measure the passage of time.
//!
//! Everything unsafe stays on the mechanism's side. A page that is shared with
//! another address space, or already out on disk, is marked ineligible before
//! the policy ever sees it, and the mechanism re-checks the answer it gets
//! back. A brick cannot cost you a frame another process is still reading.

pub use crate::memory::PageSet;

/// One way of deciding which page to throw out.
pub trait Replacer {
    fn name(&self) -> &'static str;

    /// One line, shown by `paging`.
    fn describe(&self) -> &'static str {
        ""
    }

    /// Choose a victim, as an index into `pages`. `None` means "nothing here
    /// can be taken", which is a legitimate answer: every candidate may be
    /// shared, or already swapped.
    ///
    /// Returning an ineligible index is a bug. The mechanism refuses it rather
    /// than acting on it, and the conformance suite says so out loud.
    fn choose(&mut self, pages: &PageSet) -> Option<usize>;

    /// A user page became resident — mapped, faulted in, or brought back from
    /// disk. Policies that care about arrival order have no other way to learn
    /// it: a snapshot of what is resident has no history in it.
    fn on_resident(&mut self, _address: u64) {}

    /// A page left memory.
    fn on_evicted(&mut self, _address: u64) {}

    /// Forget everything.
    fn reset(&mut self) {}
}

// --- the clock ------------------------------------------------------------

/// Second chance, swept in a circle. The one this kernel has always used.
///
/// Look at a page. If the CPU has touched it since the hand last passed, clear
/// that record and move on — one reprieve. If it has not, take it. A page
/// survives exactly as long as it keeps being used between two passes of the
/// hand.
///
/// That is the cheapest usable approximation of "least recently used" and the
/// reason it is in everything: the cost is one bit per page and a position, and
/// the position is the only state the algorithm has.
///
/// ## Why two laps
///
/// The first lap clears accessed bits. If every page was in use, the second
/// finds them all cleared and takes the first eligible one — so the sweep
/// always terminates. A single lap does not guarantee that, and a hand that
/// goes round forever taking nothing is a machine that reports being out of
/// memory while holding pages it was about to release.
pub struct Clock {
    /// Where the hand stopped. Deliberately kept across calls: restarting the
    /// sweep at zero every time would make the first few pages of an address
    /// space bear all the evictions.
    hand: usize,
}

impl Clock {
    pub const fn new() -> Self {
        Self { hand: 0 }
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

impl Replacer for Clock {
    fn name(&self) -> &'static str {
        "clock"
    }

    fn describe(&self) -> &'static str {
        "second chance, swept in a circle. one bit, made to be enough"
    }

    fn choose(&mut self, pages: &PageSet) -> Option<usize> {
        if pages.is_empty() {
            return None;
        }
        let start = self.hand;
        for step in 0..pages.len() * 2 {
            let index = (start + step) % pages.len();
            if !pages.eligible(index) {
                continue;
            }
            if pages.accessed(index) {
                // Used since the last sweep. Clear the record and give it a lap.
                pages.clear_accessed(index);
                continue;
            }
            self.hand = (index + 1) % pages.len();
            return Some(index);
        }
        None
    }

    fn reset(&mut self) {
        self.hand = 0;
    }
}

// --- the registry ---------------------------------------------------------

static mut CLOCK: Clock = Clock::new();

/// How many bricks are registered.
pub const COUNT: usize = 1;

static mut ACTIVE: usize = 0;

fn replacer_at(index: usize) -> &'static mut dyn Replacer {
    unsafe {
        match index {
            _ => &mut *core::ptr::addr_of_mut!(CLOCK),
        }
    }
}

fn active_index() -> usize {
    unsafe { core::ptr::read_volatile(core::ptr::addr_of!(ACTIVE)) }
}

pub fn active_name() -> &'static str {
    replacer_at(active_index()).name()
}

pub fn active_description() -> &'static str {
    replacer_at(active_index()).describe()
}

pub fn name_at(index: usize) -> &'static str {
    replacer_at(index.min(COUNT - 1)).name()
}

pub fn describe_at(index: usize) -> &'static str {
    replacer_at(index.min(COUNT - 1)).describe()
}

/// The mechanism's single call into policy.
pub fn choose(pages: &PageSet) -> Option<usize> {
    replacer_at(active_index()).choose(pages)
}

/// A user page became resident.
pub fn note_resident(address: u64) {
    replacer_at(active_index()).on_resident(address);
}

/// A user page went out to disk.
pub fn note_evicted(address: u64) {
    replacer_at(active_index()).on_evicted(address);
}

/// Install a different policy.
///
/// Unlike the scheduler's, this needs no state handover: a replacer's input is
/// the set of resident pages, which it is handed fresh on every call. The only
/// thing to discard is whatever the outgoing brick remembered.
pub fn install(index: usize) -> bool {
    if index >= COUNT {
        return false;
    }
    crate::interrupts::without_interrupts(|| {
        replacer_at(index).reset();
        unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!(ACTIVE), index) };
    });
    true
}

pub fn install_by_name(name: &str) -> bool {
    for index in 0..COUNT {
        if name_at(index) == name {
            return install(index);
        }
    }
    false
}
