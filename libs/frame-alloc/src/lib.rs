#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

//! Pure bitmap physical-frame allocator.
//!
//! This crate contains **no** `unsafe` and touches no hardware. It manages a
//! bitmap - one bit per 4 KiB physical frame, `1` = used - that the caller
//! provides, and decides which frame to hand out. The [`hal`](../hal) layer
//! backs the bitmap with real memory through the HHDM and zeroes handed-out
//! frames; keeping the *policy* here, free of `unsafe`, makes it unit-testable
//! on the host. See [ADR 0010](../../docs/adr/0010-bitmap-frame-allocator.md).
//!
//! The allocator serves single 4 KiB frames, plus aligned contiguous runs for
//! the rare boot-time object that needs one (the root task's 64 KiB Untyped,
//! ADR 0019). Large-page and DMA allocation are still deferred (ADR 0010).

/// Bytes per physical frame (standard x86-64 4 KiB page).
pub const FRAME_SIZE: u64 = 4096;

/// A 4 KiB physical frame, identified by its physical base address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PhysFrame(u64);

impl PhysFrame {
    /// The frame containing `addr`.
    #[must_use]
    pub const fn containing(addr: u64) -> Self {
        Self(addr & !(FRAME_SIZE - 1))
    }

    /// The frame at zero-based index `i`.
    #[must_use]
    pub const fn from_index(i: usize) -> Self {
        Self(i as u64 * FRAME_SIZE)
    }

    /// This frame's zero-based index (address divided by [`FRAME_SIZE`]).
    #[must_use]
    pub const fn index(self) -> usize {
        (self.0 / FRAME_SIZE) as usize
    }

    /// This frame's physical base address.
    #[must_use]
    pub const fn addr(self) -> u64 {
        self.0
    }
}

/// A physical memory region as seen by the allocator.
#[derive(Debug, Clone, Copy)]
pub struct Region {
    /// Physical base address.
    pub base: u64,
    /// Length in bytes.
    pub length: u64,
    /// Whether this is free RAM the allocator may hand out. Anything else
    /// (reserved, bootloader-reclaimable, the kernel image, ACPI, MMIO, bad
    /// memory) is left permanently used.
    pub usable: bool,
}

/// Why a [`FrameAllocator::free`] call was rejected. Either case is a kernel
/// bug, not a recoverable condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreeError {
    /// The frame lies outside the range the allocator tracks.
    OutOfRange,
    /// The frame was already free - a double free.
    AlreadyFree,
    /// The frame was never in the pool: frame 0, the bitmap's own storage, a
    /// reserved range, or a hole such as MMIO. Freeing it would let the
    /// allocator hand out memory it does not own (#122).
    NotPoolFrame,
}

/// Number of `u64` words a bitmap needs to track `frames` frames.
#[must_use]
pub const fn words_for(frames: usize) -> usize {
    frames.div_ceil(64)
}

/// The size and placement the bitmap should have, derived from the memory map
/// by [`plan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    /// Number of frames the bitmap tracks: index `0` up to the highest frame in
    /// any usable region.
    pub frames: usize,
    /// Number of `u64` words the bitmap occupies: two bitmaps of
    /// `words_for(frames)` each, the used bits and then the pool bits.
    pub words: usize,
    /// Physical base address chosen to store the bitmap.
    pub bitmap_base: u64,
    /// Bytes the bitmap occupies (`words * 8`).
    pub bitmap_bytes: u64,
}

/// Decide how large the frame bitmap must be and where to put it.
///
/// The bitmap spans frame `0` up to the highest frame in any usable region, so
/// every usable frame has a bit. It is placed at the base of the first usable
/// region large enough to hold it; the caller must then reserve
/// `[bitmap_base, bitmap_base + bitmap_bytes)` so the allocator never hands out
/// its own storage. Returns `None` if there is no usable RAM at all.
#[must_use]
pub fn plan(regions: &[Region]) -> Option<Layout> {
    let mut highest_end = 0u64;
    for r in regions {
        if r.usable {
            highest_end = highest_end.max(r.base.saturating_add(r.length));
        }
    }
    if highest_end == 0 {
        return None;
    }
    let frames = (highest_end / FRAME_SIZE) as usize;
    let words = 2 * words_for(frames);
    let bitmap_bytes = (words as u64) * 8;
    for r in regions {
        if r.usable && r.length >= bitmap_bytes {
            return Some(Layout {
                frames,
                words,
                bitmap_base: r.base,
                bitmap_bytes,
            });
        }
    }
    None
}

/// A bitmap physical-frame allocator over a caller-provided bitmap slice.
pub struct FrameAllocator<'a> {
    /// One bit per frame, `1` = used.
    bitmap: &'a mut [u64],
    /// One bit per frame, `1` = in the pool (usable and not reserved): the
    /// only frames [`free`](Self::free) accepts. Fixed after construction.
    pool: &'a mut [u64],
    frames: usize,
    /// Next-free search hint (a frame index); `alloc` starts scanning here.
    hint: usize,
    /// Count of currently-free frames (maintained incrementally).
    free: usize,
    /// Count of frames that belong to usable RAM - the managed pool size, fixed
    /// after construction.
    usable: usize,
}

impl<'a> FrameAllocator<'a> {
    /// Build an allocator over `bitmap`, tracking `frames` frames. `bitmap`
    /// holds two bitmaps of `words_for(frames)` words each (as [`plan`] sizes
    /// it): the used bits, then the pool bits.
    ///
    /// Every frame starts **used**; frames fully inside a `usable` region are
    /// then freed; finally every `(base, len)` range in `reserved` is marked
    /// used again. The caller passes the bitmap's own backing storage in
    /// `reserved`, plus any memory that must never be handed out even though
    /// firmware called it usable (e.g. physical frame 0).
    ///
    /// # Panics
    /// If `bitmap` is too short to hold `frames` bits.
    #[must_use]
    pub fn new(
        bitmap: &'a mut [u64],
        frames: usize,
        regions: &[Region],
        reserved: &[(u64, u64)],
    ) -> Self {
        let words = words_for(frames);
        assert!(
            bitmap.len() >= 2 * words,
            "bitmap too small for frame count (it holds the used and pool bits)"
        );
        let (bitmap, rest) = bitmap.split_at_mut(words);
        let pool = &mut rest[..words];
        bitmap.fill(u64::MAX);
        pool.fill(0);
        let mut a = Self {
            bitmap,
            pool,
            frames,
            hint: 0,
            free: 0,
            usable: 0,
        };
        for r in regions {
            if r.usable {
                a.free_region(r.base, r.length);
            }
        }
        // Everything freeable is now free: that is the managed RAM pool.
        a.usable = a.free;
        for &(base, len) in reserved {
            a.reserve_region(base, len);
        }
        // The pool: frames free now, which excludes the reserved ranges.
        for i in 0..frames {
            if !a.is_used(i) {
                a.pool[i / 64] |= 1 << (i % 64);
            }
        }
        a.hint = 0;
        a
    }

    /// Allocate one frame, or `None` if none are free. The returned frame is
    /// marked used; the caller (the HAL) is responsible for zeroing its memory.
    pub fn alloc(&mut self) -> Option<PhysFrame> {
        if self.free == 0 {
            return None;
        }
        let i = self
            .find_free_from(self.hint)
            .or_else(|| self.find_free_from(0))?;
        self.mark_used(i);
        self.hint = i + 1;
        Some(PhysFrame::from_index(i))
    }

    /// Allocate `count` physically contiguous frames whose base is aligned to
    /// `count` frames, or `None` if `count` is not a power of two or no such
    /// run is free. Every frame of the run is marked used; the caller frees
    /// them one by one (or with the HAL's `free_contiguous`).
    pub fn alloc_contiguous(&mut self, count: usize) -> Option<PhysFrame> {
        if !count.is_power_of_two() || count > self.free {
            return None;
        }
        let mut base = 0;
        while base + count <= self.frames {
            match (base..base + count).rfind(|&i| self.is_used(i)) {
                // Skip past the used frame, to the next aligned base.
                Some(used) => base = (used / count + 1) * count,
                None => {
                    for i in base..base + count {
                        self.mark_used(i);
                    }
                    return Some(PhysFrame::from_index(base));
                }
            }
        }
        None
    }

    /// Return a previously-allocated frame to the pool.
    ///
    /// # Errors
    /// [`FreeError::OutOfRange`] if the frame is not tracked,
    /// [`FreeError::NotPoolFrame`] if it was never in the pool, or
    /// [`FreeError::AlreadyFree`] on a double free. All indicate a kernel bug.
    pub fn free(&mut self, frame: PhysFrame) -> Result<(), FreeError> {
        let i = frame.index();
        if i >= self.frames {
            return Err(FreeError::OutOfRange);
        }
        if self.pool[i / 64] & (1 << (i % 64)) == 0 {
            return Err(FreeError::NotPoolFrame);
        }
        if !self.is_used(i) {
            return Err(FreeError::AlreadyFree);
        }
        self.mark_free(i);
        Ok(())
    }

    /// Whether `frame` is a pool frame that is allocated now: what memory the
    /// kernel may treat as its own object storage (#106).
    #[must_use]
    pub fn is_allocated(&self, frame: PhysFrame) -> bool {
        let i = frame.index();
        i < self.frames && self.pool[i / 64] & (1 << (i % 64)) != 0 && self.is_used(i)
    }

    /// Total number of frames tracked (including never-usable ones).
    #[must_use]
    pub const fn total_frames(&self) -> usize {
        self.frames
    }

    /// Frames currently free and available.
    #[must_use]
    pub const fn free_frames(&self) -> usize {
        self.free
    }

    /// Frames currently in use (allocated, reserved, or never-usable).
    #[must_use]
    pub const fn used_frames(&self) -> usize {
        self.frames - self.free
    }

    /// Size of the usable RAM pool (the frames that were ever freeable), fixed
    /// at construction.
    #[must_use]
    pub const fn usable_frames(&self) -> usize {
        self.usable
    }

    // --- internal bit manipulation; maintains `free` and `hint` ---

    fn is_used(&self, i: usize) -> bool {
        self.bitmap[i / 64] & (1u64 << (i % 64)) != 0
    }

    fn mark_used(&mut self, i: usize) {
        if i >= self.frames {
            return;
        }
        let bit = 1u64 << (i % 64);
        if self.bitmap[i / 64] & bit == 0 {
            self.bitmap[i / 64] |= bit;
            self.free -= 1;
        }
    }

    fn mark_free(&mut self, i: usize) {
        if i >= self.frames {
            return;
        }
        let bit = 1u64 << (i % 64);
        if self.bitmap[i / 64] & bit != 0 {
            self.bitmap[i / 64] &= !bit;
            self.free += 1;
            if i < self.hint {
                self.hint = i;
            }
        }
    }

    /// Free whole frames fully contained in `[base, base + len)`.
    fn free_region(&mut self, base: u64, len: u64) {
        if len == 0 {
            return;
        }
        let start = base.div_ceil(FRAME_SIZE);
        let end = (base + len) / FRAME_SIZE; // exclusive, floor: no partial frames
        for i in start..end {
            self.mark_free(i as usize);
        }
    }

    /// Reserve every frame touched by `[base, base + len)` (conservative).
    fn reserve_region(&mut self, base: u64, len: u64) {
        if len == 0 {
            return;
        }
        let start = base / FRAME_SIZE;
        let end = (base + len).div_ceil(FRAME_SIZE); // exclusive, ceil: whole frames
        for i in start..end {
            self.mark_used(i as usize);
        }
    }

    /// Scan from frame `start` for the first free frame, skipping full words.
    fn find_free_from(&self, start: usize) -> Option<usize> {
        let mut w = start / 64;
        let words = words_for(self.frames);
        while w < words {
            let word = self.bitmap[w];
            if word != u64::MAX {
                let first_bit = if w == start / 64 { start % 64 } else { 0 };
                for b in first_bit..64 {
                    if word & (1u64 << b) == 0 {
                        let idx = w * 64 + b;
                        return if idx < self.frames { Some(idx) } else { None };
                    }
                }
            }
            w += 1;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    fn usable(base: u64, length: u64) -> Region {
        Region {
            base,
            length,
            usable: true,
        }
    }

    #[test]
    fn plan_sizes_and_places_the_bitmap() {
        // 4 MiB of RAM starting at 0; one reserved hole high up.
        let regions = [
            usable(0, 4 * MIB),
            Region {
                base: 0xfee0_0000,
                length: 4 * MIB,
                usable: false,
            },
        ];
        let layout = plan(&regions).unwrap();
        assert_eq!(layout.frames, (4 * MIB / FRAME_SIZE) as usize); // 1024 frames
        assert_eq!(layout.words, 32); // 1024 / 64, twice: used bits and pool bits
        assert_eq!(layout.bitmap_base, 0); // first usable region
        assert_eq!(layout.bitmap_bytes, 256);
    }

    #[test]
    fn free_rejects_frames_outside_the_pool() {
        // Frames 0..8 usable, 8..16 a hole; frame 1 reserved (e.g. the bitmap).
        let mut bitmap = [0u64; 2];
        let regions = [
            usable(0, 8 * FRAME_SIZE),
            Region {
                base: 8 * FRAME_SIZE,
                length: 8 * FRAME_SIZE,
                usable: false,
            },
            usable(16 * FRAME_SIZE, 16 * FRAME_SIZE),
        ];
        let mut a = FrameAllocator::new(&mut bitmap, 32, &regions, &[(FRAME_SIZE, FRAME_SIZE)]);
        let free = a.free_frames();
        let reserved = PhysFrame::from_index(1);
        let hole = PhysFrame::from_index(9);
        assert_eq!(a.free(reserved), Err(FreeError::NotPoolFrame));
        assert_eq!(a.free(hole), Err(FreeError::NotPoolFrame));
        assert_eq!(a.free_frames(), free, "a rejected free changed nothing");
        // A frame from the pool still round-trips.
        let f = a.alloc().unwrap();
        assert_eq!(a.free(f), Ok(()));
        assert_eq!(a.free(f), Err(FreeError::AlreadyFree));
    }

    #[test]
    fn is_allocated_means_in_the_pool_and_in_use() {
        let mut bitmap = [0u64; 2];
        let regions = [usable(0, 32 * FRAME_SIZE)];
        let mut a = FrameAllocator::new(&mut bitmap, 32, &regions, &[(0, FRAME_SIZE)]);
        let f = a.alloc().unwrap();
        assert!(a.is_allocated(f));
        a.free(f).unwrap();
        assert!(!a.is_allocated(f), "a free frame");
        assert!(
            !a.is_allocated(PhysFrame::from_index(0)),
            "a reserved frame"
        );
        assert!(
            !a.is_allocated(PhysFrame::from_index(40)),
            "an untracked frame"
        );
    }

    #[test]
    #[should_panic(expected = "bitmap too small")]
    fn new_needs_room_for_both_bitmaps() {
        let mut bitmap = [0u64; 1]; // one word: the used bits only
        let regions = [usable(0, 64 * FRAME_SIZE)];
        let _ = FrameAllocator::new(&mut bitmap, 64, &regions, &[]);
    }

    #[test]
    fn plan_returns_none_without_usable_ram() {
        let regions = [Region {
            base: 0,
            length: 16 * MIB,
            usable: false,
        }];
        assert_eq!(plan(&regions), None);
    }

    #[test]
    fn new_accounts_usable_and_reserved() {
        // 1024 frames; all usable; reserve the first 2 frames (e.g. a bitmap).
        let mut bitmap = [0u64; 32];
        let regions = [usable(0, 4 * MIB)];
        let a = FrameAllocator::new(&mut bitmap, 1024, &regions, &[(0, 2 * FRAME_SIZE)]);
        assert_eq!(a.total_frames(), 1024);
        assert_eq!(a.usable_frames(), 1024); // pool size unaffected by reservation
        assert_eq!(a.used_frames(), 2); // the two reserved frames
        assert_eq!(a.free_frames(), 1022);
    }

    #[test]
    fn non_usable_frames_are_never_handed_out() {
        // Two 64 KiB usable windows with a gap; everything else used.
        let mut bitmap = [0u64; 32];
        let regions = [usable(0, 64 * 1024), usable(512 * 1024, 64 * 1024)];
        let mut a = FrameAllocator::new(&mut bitmap, 1024, &regions, &[]);
        // 32 frames total usable (16 + 16).
        assert_eq!(a.usable_frames(), 32);
        for _ in 0..32 {
            let f = a.alloc().expect("pool not exhausted");
            // Every handed-out frame is inside one of the usable windows.
            let addr = f.addr();
            assert!(addr < 64 * 1024 || (512 * 1024..512 * 1024 + 64 * 1024).contains(&addr));
        }
        assert_eq!(a.alloc(), None); // exhausted
    }

    #[test]
    fn alloc_free_roundtrip_and_reuse() {
        let mut bitmap = [0u64; 32];
        let regions = [usable(0, 4 * MIB)];
        let mut a = FrameAllocator::new(&mut bitmap, 1024, &regions, &[]);
        let first = a.alloc().unwrap();
        let second = a.alloc().unwrap();
        assert_ne!(first, second);
        assert_eq!(a.free_frames(), 1022);
        a.free(first).unwrap();
        assert_eq!(a.free_frames(), 1023);
        // The hint moved back, so the freed frame is reused next.
        assert_eq!(a.alloc().unwrap(), first);
    }

    #[test]
    fn double_free_and_out_of_range_are_rejected() {
        let mut bitmap = [0u64; 32];
        let regions = [usable(0, 4 * MIB)];
        let mut a = FrameAllocator::new(&mut bitmap, 1024, &regions, &[]);
        let f = a.alloc().unwrap();
        assert_eq!(a.free(f), Ok(()));
        assert_eq!(a.free(f), Err(FreeError::AlreadyFree));
        assert_eq!(
            a.free(PhysFrame::from_index(9999)),
            Err(FreeError::OutOfRange)
        );
    }

    #[test]
    fn exhaustion_reports_none() {
        let mut bitmap = [0u64; 2];
        let regions = [usable(0, 8 * FRAME_SIZE)]; // 8 frames
        let mut a = FrameAllocator::new(&mut bitmap, 8, &regions, &[]);
        for _ in 0..8 {
            assert!(a.alloc().is_some());
        }
        assert_eq!(a.alloc(), None);
        assert_eq!(a.free_frames(), 0);
    }

    #[test]
    fn partial_frames_are_not_freed() {
        // A usable region misaligned by half a frame on each end yields only
        // the whole frames strictly inside it.
        let mut bitmap = [0u64; 2];
        let regions = [usable(FRAME_SIZE / 2, 3 * FRAME_SIZE)];
        let a = FrameAllocator::new(&mut bitmap, 8, &regions, &[]);
        // [2048, 14336) contains whole frames 1 and 2 only.
        assert_eq!(a.usable_frames(), 2);
    }

    #[test]
    fn contiguous_run_is_aligned_and_marked() {
        let mut bitmap = [0u64; 32];
        let regions = [usable(0, 4 * MIB)];
        let mut a = FrameAllocator::new(&mut bitmap, 1024, &regions, &[]);
        a.alloc().unwrap(); // frame 0 taken: the run cannot start there
        let base = a.alloc_contiguous(16).unwrap();
        assert_eq!(base.addr() % (16 * 4096), 0);
        assert_eq!(base.index(), 16);
        assert_eq!(a.free_frames(), 1024 - 1 - 16);
        // None of the run is handed out singly.
        for _ in 0..1024 - 17 {
            let f = a.alloc().unwrap();
            assert!(!(16..32).contains(&f.index()));
        }
        assert!(a.alloc().is_none());
    }

    #[test]
    fn contiguous_skips_fragmented_runs() {
        let mut bitmap = [0u64; 2];
        let regions = [usable(0, 64 * 4096)];
        let mut a = FrameAllocator::new(&mut bitmap, 64, &regions, &[]);
        // Take one frame in each of the first three aligned runs of 16.
        let taken: [usize; 3] = [3, 20, 47];
        let mut all = [PhysFrame::from_index(0); 64];
        for (i, slot) in all.iter_mut().enumerate() {
            *slot = a.alloc().unwrap();
            assert_eq!(slot.index(), i);
        }
        for (i, f) in all.iter().enumerate() {
            if !taken.contains(&i) {
                a.free(*f).unwrap();
            }
        }
        assert_eq!(a.alloc_contiguous(16).unwrap().index(), 48);
    }

    #[test]
    fn contiguous_none_when_no_run() {
        let mut bitmap = [0u64; 2];
        let regions = [usable(0, 32 * 4096)];
        let mut a = FrameAllocator::new(&mut bitmap, 32, &regions, &[]);
        let _ = a.alloc(); // frame 0
        let f = PhysFrame::from_index(16);
        a.reserve_region(f.addr(), 4096); // frame 16
        let free = a.free_frames();
        assert!(a.alloc_contiguous(16).is_none());
        assert_eq!(a.free_frames(), free);
    }

    #[test]
    fn non_power_of_two_is_refused() {
        let mut bitmap = [0u64; 32];
        let regions = [usable(0, 4 * MIB)];
        let mut a = FrameAllocator::new(&mut bitmap, 1024, &regions, &[]);
        assert!(a.alloc_contiguous(3).is_none());
        assert!(a.alloc_contiguous(0).is_none());
        assert_eq!(a.free_frames(), 1024);
    }
}
