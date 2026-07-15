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

// --- first in, first out --------------------------------------------------

/// Throw out whatever has been resident longest, and ask nothing else.
///
/// The obvious policy, and the one worth implementing because of how it fails.
/// It ignores the accessed bit entirely, so a page being read a thousand times
/// a second leaves anyway the moment it is the oldest — which is the wrong
/// answer often enough to matter.
///
/// But that is not the interesting failure. **FIFO can serve more page faults
/// when you give it more memory.** Bélády found it in 1969 and it is deeply
/// counter-intuitive: every other resource in a computer gets better when you
/// add more of it. `bench paging` reproduces it on this machine, on real page
/// tables, in about four seconds.
///
/// The reason is that FIFO is not a *stack algorithm* — the set of pages it
/// keeps with three frames is not guaranteed to be a subset of what it keeps
/// with four, so adding a frame can rearrange the whole future. LRU and
/// optimal are stack algorithms and cannot do this. Clock is not one either,
/// strictly, but in practice does not.
///
/// ## Where the order comes from
///
/// Not from the page set — that arrives in address order and has no history in
/// it. Arrival order is only knowable if somebody records it as it happens,
/// which is what `on_resident` is for and the whole reason the trait has
/// notifications rather than just `choose`.
pub struct Fifo {
    address: [u64; TRACKED],
    stamp: [u64; TRACKED],
    next: u64,
}

/// How many pages FIFO remembers the arrival of. A fixed table rather than a
/// `Vec`: this runs inside the page fault path, where allocating to decide how
/// to free memory is a poor sequence of events.
const TRACKED: usize = 128;

impl Fifo {
    pub const fn new() -> Self {
        Self {
            address: [0; TRACKED],
            stamp: [0; TRACKED],
            next: 1,
        }
    }

    fn slot_of(&self, address: u64) -> Option<usize> {
        (0..TRACKED).find(|&i| self.stamp[i] != 0 && self.address[i] == address)
    }
}

impl Default for Fifo {
    fn default() -> Self {
        Self::new()
    }
}

impl Replacer for Fifo {
    fn name(&self) -> &'static str {
        "fifo"
    }

    fn describe(&self) -> &'static str {
        "oldest resident page goes. shows Belady's anomaly"
    }

    fn choose(&mut self, pages: &PageSet) -> Option<usize> {
        let mut best = None;
        let mut oldest = u64::MAX;
        for index in 0..pages.len() {
            if !pages.eligible(index) {
                continue;
            }
            // A page we never saw arrive is treated as older than any we did.
            // That is the safe default: it is either from before this policy
            // was installed, or it overflowed the table.
            let age = match self.slot_of(pages.address(index)) {
                Some(slot) => self.stamp[slot],
                None => 0,
            };
            if age < oldest {
                oldest = age;
                best = Some(index);
            }
        }
        best
    }

    fn on_resident(&mut self, address: u64) {
        // First arrival wins. Re-stamping on every touch would make this LRU,
        // which is a different policy and a better one.
        if self.slot_of(address).is_some() {
            return;
        }
        let free = (0..TRACKED)
            .find(|&i| self.stamp[i] == 0)
            .or_else(|| {
                // Table full: forget the oldest, which is the one most likely
                // to be evicted next anyway.
                (0..TRACKED).min_by_key(|&i| self.stamp[i])
            })
            .unwrap_or(0);
        self.address[free] = address;
        self.stamp[free] = self.next;
        self.next += 1;
    }

    fn on_evicted(&mut self, address: u64) {
        if let Some(slot) = self.slot_of(address) {
            self.stamp[slot] = 0;
        }
    }

    fn reset(&mut self) {
        self.address = [0; TRACKED];
        self.stamp = [0; TRACKED];
        self.next = 1;
    }
}

// --- not recently used ----------------------------------------------------

/// Sort every page into one of four classes and take from the emptiest-first.
///
/// The two bits the hardware gives you, used as a pair rather than one at a
/// time:
///
/// ```text
///   class 0   not accessed, not written   cheapest to lose
///   class 1   not accessed, written
///   class 2   accessed, not written
///   class 3   accessed, written           most likely to be wanted again
/// ```
///
/// Take anything from the lowest non-empty class. It is cruder than the clock —
/// it will happily evict a page that was accessed slightly less recently than
/// another in the same class — and it is one pass with no position to remember.
///
/// ## The part that is not textbook
///
/// Classic NRU depends on *something else* clearing the accessed bits
/// periodically, usually a timer. Without that every page reaches class 2 or 3
/// and stays there, and the policy degenerates into "take the first eligible
/// page" — which is address order, which is nothing.
///
/// This kernel has no such timer, so the reset is folded into the eviction
/// itself: having chosen, clear every accessed bit on the way out. That makes
/// each eviction the start of a fresh observation window. It is a real design
/// decision and it is visible in the numbers — NRU trades the clock's smooth
/// gradient for a sawtooth.
pub struct Nru;

impl Nru {
    pub const fn new() -> Self {
        Self
    }
}

impl Default for Nru {
    fn default() -> Self {
        Self::new()
    }
}

impl Replacer for Nru {
    fn name(&self) -> &'static str {
        "nru"
    }

    fn describe(&self) -> &'static str {
        "four classes from two bits, lowest non-empty wins"
    }

    fn choose(&mut self, pages: &PageSet) -> Option<usize> {
        let mut best: Option<(u8, usize)> = None;
        for index in 0..pages.len() {
            if !pages.eligible(index) {
                continue;
            }
            let class = (pages.accessed(index) as u8) * 2 + pages.dirty(index) as u8;
            if best.is_none_or(|(c, _)| class < c) {
                best = Some((class, index));
            }
            if class == 0 {
                break; // nothing can beat it
            }
        }
        let (_, chosen) = best?;

        // Start a fresh observation window. Without this the classes fill up
        // and never empty -- see the note above.
        for index in 0..pages.len() {
            if pages.accessed(index) {
                pages.clear_accessed(index);
            }
        }
        Some(chosen)
    }
}

// --- random ---------------------------------------------------------------

/// Pick one at random.
///
/// Here as a control, and it is not a joke. Random has no state, no scan, no
/// bits to maintain, and it cannot be defeated by an access pattern designed to
/// defeat it — which is more than the clock can say. It also cannot suffer
/// Bélády's anomaly in any systematic way.
///
/// Run `bench paging` and see how far off it is. The gap between random and a
/// carefully reasoned policy is usually much smaller than the effort spent on
/// the policy would suggest, and knowing the size of that gap is what stops you
/// spending a month on the fifth refinement of a replacement algorithm.
///
/// The generator is xorshift64 from a fixed seed, so a run is reproducible.
/// That is what a benchmark wants and the opposite of what anything security
/// shaped wants -- a real one would take entropy from somewhere the caller
/// cannot predict, and this kernel has nowhere like that yet.
pub struct Random {
    state: u64,
}

/// Fixed on purpose: the same run twice gives the same table.
const SEED: u64 = 0x2545_F491_4F6C_DD1D;

impl Random {
    pub const fn new() -> Self {
        Self { state: 0 }
    }

    fn next(&mut self) -> u64 {
        if self.state == 0 {
            self.state = SEED;
        }
        // xorshift64. Three shifts and three xors, and good enough for anything
        // that is not trying to keep a secret.
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        self.state
    }
}

impl Default for Random {
    fn default() -> Self {
        Self::new()
    }
}

impl Replacer for Random {
    fn name(&self) -> &'static str {
        "random"
    }

    fn describe(&self) -> &'static str {
        "no state, no scan, no bits. the control you compare against"
    }

    fn choose(&mut self, pages: &PageSet) -> Option<usize> {
        let count = (0..pages.len()).filter(|&i| pages.eligible(i)).count();
        if count == 0 {
            return None;
        }
        let mut wanted = (self.next() % count as u64) as usize;
        for index in 0..pages.len() {
            if !pages.eligible(index) {
                continue;
            }
            if wanted == 0 {
                return Some(index);
            }
            wanted -= 1;
        }
        None
    }

    fn reset(&mut self) {
        self.state = 0;
    }
}

// --- the registry ---------------------------------------------------------

static mut CLOCK: Clock = Clock::new();
static mut FIFO: Fifo = Fifo::new();
static mut NRU: Nru = Nru::new();
static mut RANDOM: Random = Random::new();

/// How many bricks are registered.
pub const COUNT: usize = 4;

static mut ACTIVE: usize = 0;

fn replacer_at(index: usize) -> &'static mut dyn Replacer {
    unsafe {
        match index {
            1 => &mut *core::ptr::addr_of_mut!(FIFO),
            2 => &mut *core::ptr::addr_of_mut!(NRU),
            3 => &mut *core::ptr::addr_of_mut!(RANDOM),
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

// --- the conformance surface ---------------------------------------------
//
// A replacer is a pure function of the page set, so it can be interrogated
// without a single page being evicted. `selftest replace` uses these to put
// every registered brick -- including one somebody has just written -- through
// states that would be tedious and slow to arrange on a live machine.

/// Ask a policy to choose, against a page set that is not a real address space.
pub fn test_choose(index: usize, pages: &PageSet) -> Option<usize> {
    replacer_at(index.min(COUNT - 1)).choose(pages)
}

/// Wipe a policy's memory, so one probe cannot make the next one's answer look
/// right -- and so the installed brick is not left believing something about a
/// page that never existed.
pub fn test_reset(index: usize) {
    replacer_at(index.min(COUNT - 1)).reset();
}
