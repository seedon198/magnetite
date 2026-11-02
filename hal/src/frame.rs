//! Physical frame allocator: the HAL binding for the pure [`frame_alloc`]
//! policy.
//!
//! The [`frame_alloc`] crate decides *which* physical frame to hand out,
//! working only on an abstract bitmap (no `unsafe`, host-tested). This module
//! supplies the two things that need hardware access, both `unsafe` and
//! confined here (ADR 0007):
//!
//! 1. backing the bitmap with real memory, by forming a `&'static mut [u64]`
//!    over a usable RAM region through the Higher-Half Direct Map (HHDM); and
//! 2. zeroing each frame as it is handed out, so a new owner can never read a
//!    previous owner's data (a hardening default - see ADR 0010).
//!
//! The global allocator is guarded by a [`SpinLock`]; the frame-zeroing write
//! happens *after* the lock is released, since the frame is exclusively ours
//! once [`FrameAllocator::alloc`] returns.

use core::sync::atomic::{AtomicU64, Ordering};

use frame_alloc::FrameAllocator;

pub use frame_alloc::{FreeError, PhysFrame, Region, FRAME_SIZE};

use crate::sync::SpinLock;

static ALLOCATOR: SpinLock<Option<FrameAllocator<'static>>> = SpinLock::new(None);
/// HHDM offset captured at [`init`], used to reach frames for zeroing.
static HHDM_OFFSET: AtomicU64 = AtomicU64::new(0);

/// A snapshot of allocator accounting, in frames.
#[derive(Debug, Clone, Copy)]
pub struct Stats {
    /// Total frames tracked (including never-usable ones).
    pub total: usize,
    /// Frames currently free and available.
    pub free: usize,
    /// Frames currently in use (allocated, reserved, or never-usable).
    pub used: usize,
    /// Size of the usable RAM pool (frames that were ever freeable).
    pub usable: usize,
}

/// Initialize the physical frame allocator from the memory map.
///
/// `regions` is the physical memory map (each entry flagged `usable` or not)
/// and `hhdm_offset` is the HHDM base. Returns `false` (leaving the allocator
/// uninitialized) if there is no usable RAM or no region large enough to hold
/// the bitmap.
///
/// Physical frame 0 is always reserved so a real allocation is never confused
/// with a null address.
#[must_use]
pub fn init(regions: &[Region], hhdm_offset: u64) -> bool {
    let Some(layout) = frame_alloc::plan(regions) else {
        return false;
    };

    HHDM_OFFSET.store(hhdm_offset, Ordering::Relaxed);

    let ptr = (hhdm_offset + layout.bitmap_base) as *mut u64;
    // SAFETY: `bitmap_base` is the base of a usable RAM region from the memory
    // map (`plan` picked a region at least `bitmap_bytes` long), mapped by the
    // bootloader in the HHDM at `hhdm_offset`; the base of a firmware region is
    // page-aligned, hence 8-aligned, so `ptr` is aligned for `u64`. We take
    // exclusive ownership of these `layout.words` words for the lifetime of the
    // kernel: nothing else references them, and we reserve them below so the
    // allocator never hands them back out. The `'static` slice therefore
    // aliases nothing.
    let bitmap: &'static mut [u64] = unsafe { core::slice::from_raw_parts_mut(ptr, layout.words) };

    // Reserve the bitmap's own storage and physical frame 0.
    let reserved = [(layout.bitmap_base, layout.bitmap_bytes), (0, FRAME_SIZE)];
    let alloc = FrameAllocator::new(bitmap, layout.frames, regions, &reserved);
    *ALLOCATOR.lock() = Some(alloc);
    true
}

/// Allocate one zeroed physical frame, or `None` if none are free or the
/// allocator is not initialized.
pub fn alloc_frame() -> Option<PhysFrame> {
    let frame = ALLOCATOR.lock().as_mut()?.alloc()?;
    zero_frame(frame);
    Some(frame)
}

/// Allocate `count` contiguous zeroed frames, aligned to `count` frames
/// (`count` a power of two). Returns the first frame, or `None`.
pub fn alloc_contiguous(count: usize) -> Option<PhysFrame> {
    let base = ALLOCATOR.lock().as_mut()?.alloc_contiguous(count)?;
    for i in 0..count {
        zero_frame(PhysFrame::from_index(base.index() + i));
    }
    Some(base)
}

/// Return a run from [`alloc_contiguous`] to the pool.
pub fn free_contiguous(base: PhysFrame, count: usize) {
    for i in 0..count {
        free_frame(PhysFrame::from_index(base.index() + i));
    }
}

/// Return a frame to the pool.
///
/// # Panics
/// On a double free, or a free of a frame outside the pool (untracked,
/// reserved, or never usable): all are kernel bugs (see [`FreeError`]).
pub fn free_frame(frame: PhysFrame) {
    match ALLOCATOR.lock().as_mut() {
        Some(alloc) => {
            if let Err(e) = alloc.free(frame) {
                panic!("frame: invalid free of {:#x}: {:?}", frame.addr(), e);
            }
        }
        None => panic!("frame: free before init"),
    }
}

/// A snapshot of current allocator accounting, or `None` before [`init`].
pub fn stats() -> Option<Stats> {
    let guard = ALLOCATOR.lock();
    let a = guard.as_ref()?;
    Some(Stats {
        total: a.total_frames(),
        free: a.free_frames(),
        used: a.used_frames(),
        usable: a.usable_frames(),
    })
}

/// Zero the 4 KiB of physical memory behind `frame`, reached through the HHDM.
fn zero_frame(frame: PhysFrame) {
    let hhdm = HHDM_OFFSET.load(Ordering::Relaxed);
    let ptr = (hhdm + frame.addr()) as *mut u8;
    // SAFETY: `frame` was just returned by the allocator, so it is a 4 KiB
    // usable RAM frame now exclusively ours. It is mapped in the HHDM at
    // `hhdm`, so `ptr .. ptr + FRAME_SIZE` is valid, writable, and contained in
    // that frame.
    unsafe {
        core::ptr::write_bytes(ptr, 0, FRAME_SIZE as usize);
    }
}
