//! Kernel heap: a growable first-fit free-list allocator.
//!
//! A hand-rolled general-purpose allocator wired to [`GlobalAlloc`], so `Box`,
//! `Vec`, and friends work in the kernel. Free memory is threaded as an
//! address-sorted singly linked list of [`FreeNode`]s stored *inside* the free
//! blocks; allocation is first-fit, freeing **coalesces** adjacent blocks, and
//! an [`AllocHeader`] stored just before each allocation records the block to
//! return on `dealloc` (so arbitrary alignments are handled without guesswork).
//!
//! Free memory holds no stale data: a block is **zeroed when it is freed**,
//! and a free-list header swallowed by a merge is zeroed too, so the only
//! non-zero bytes in free memory are the live list headers, and a later
//! allocation never sees an earlier one's contents (#69).
//!
//! The heap is **growable**: when no block fits, it calls a `grow` hook
//! (installed by the kernel) that maps fresh frames and hands back a new,
//! address-contiguous region, which coalesces onto the existing pool.
//!
//! Because the free list lives in the managed memory, the bookkeeping here is
//! inherently `unsafe` and so lives in `hal` (unlike the pure `frame-alloc` /
//! `page-table` crates); it is verified by an in-kernel self-test. See
//! [ADR 0013](../../docs/adr/0013-kernel-heap.md).

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;

use crate::sync::SpinLock;

/// A node in the address-sorted free list, stored at the start of a free block.
#[repr(C)]
struct FreeNode {
    /// Total bytes of this free block, including this header.
    size: usize,
    /// Next free block (strictly higher address), or null.
    next: *mut FreeNode,
}

/// Stored immediately before each allocation so `dealloc` can recover the exact
/// block to return, independent of the alignment padding in front of it.
#[repr(C)]
struct AllocHeader {
    block_start: usize,
    block_size: usize,
}

const NODE_SIZE: usize = core::mem::size_of::<FreeNode>();
const NODE_ALIGN: usize = core::mem::align_of::<FreeNode>();
const HEADER_SIZE: usize = core::mem::size_of::<AllocHeader>();
/// Minimum allocation alignment: keeps the [`AllocHeader`] (placed at
/// `user - HEADER_SIZE`) and any folded [`FreeNode`] naturally aligned.
const MIN_ALIGN: usize = if NODE_ALIGN > core::mem::align_of::<AllocHeader>() {
    NODE_ALIGN
} else {
    core::mem::align_of::<AllocHeader>()
};

const fn align_up(addr: usize, align: usize) -> usize {
    (addr + align - 1) & !(align - 1)
}

/// [`align_up`], or `None` if it would overflow (#121).
const fn checked_align_up(addr: usize, align: usize) -> Option<usize> {
    match addr.checked_add(align - 1) {
        Some(a) => Some(a & !(align - 1)),
        None => None,
    }
}

/// A hook that maps at least `min` more bytes of heap memory and returns the
/// new `(base, size)` region (address-contiguous with prior ones), or `None`
/// if it cannot.
pub type GrowFn = fn(min: usize) -> Option<(usize, usize)>;

/// The inner, lock-protected allocator state.
struct Heap {
    head: *mut FreeNode,
    grow: Option<GrowFn>,
    allocated: usize,
    total: usize,
}

// SAFETY: all access is serialized by the `SpinLock` in `KernelHeap`; the raw
// pointers only ever address the heap's own mapped memory.
unsafe impl Send for Heap {}

impl Heap {
    const fn empty() -> Self {
        Self {
            head: ptr::null_mut(),
            grow: None,
            allocated: 0,
            total: 0,
        }
    }

    /// Add `[start, start + size)` to the pool. The caller guarantees the range
    /// is mapped, writable, and not otherwise in use.
    unsafe fn add_region(&mut self, start: usize, size: usize) {
        if size < NODE_SIZE {
            return;
        }
        self.total += size;
        self.insert_free(start, size);
    }

    /// Call the grow hook for at least `min` bytes and fold the result in.
    fn grow_now(&mut self, min: usize) -> bool {
        let Some(grow) = self.grow else {
            return false;
        };
        match grow(min) {
            Some((start, size)) => {
                // SAFETY: the grow hook just mapped `[start, start+size)` as
                // writable kernel memory for our exclusive use.
                unsafe { self.add_region(start, size) };
                true
            }
            None => false,
        }
    }

    /// Insert a free block, coalescing with an adjacent predecessor/successor.
    fn insert_free(&mut self, start: usize, size: usize) {
        let mut prev: *mut FreeNode = ptr::null_mut();
        let mut cur = self.head;
        while !cur.is_null() && (cur as usize) < start {
            prev = cur;
            // SAFETY: `cur` is a live free node in our list.
            cur = unsafe { (*cur).next };
        }

        // Merge into the predecessor if the blocks touch.
        if !prev.is_null() {
            // SAFETY: `prev` is a live free node.
            let prev_size = unsafe { (*prev).size };
            if prev as usize + prev_size == start {
                // SAFETY: `prev` is live; extend it to swallow the new span.
                unsafe { (*prev).size = prev_size + size };
                // SAFETY: `prev` is live; re-read its grown size.
                let grown = unsafe { (*prev).size };
                if !cur.is_null() && prev as usize + grown == cur as usize {
                    // SAFETY: `prev` now abuts `cur`; splice `cur` out, then
                    // clear its header, which is now interior free memory.
                    unsafe {
                        (*prev).size = grown + (*cur).size;
                        (*prev).next = (*cur).next;
                        ptr::write_bytes(cur.cast::<u8>(), 0, NODE_SIZE);
                    }
                }
                return;
            }
        }

        // Otherwise create a node at `start`.
        let node = start as *mut FreeNode;
        // SAFETY: `start` is 8-aligned, within mapped heap memory, and not part
        // of any live block (callers guarantee it).
        unsafe {
            (*node).size = size;
            (*node).next = cur;
        }
        if prev.is_null() {
            self.head = node;
        } else {
            // SAFETY: `prev` is a live node.
            unsafe { (*prev).next = node };
        }
        // Merge forward into `cur` if they touch.
        if !cur.is_null() && start + size == cur as usize {
            // SAFETY: `node` and `cur` are live and adjacent; `cur`'s header
            // becomes interior free memory and is cleared.
            unsafe {
                (*node).size += (*cur).size;
                (*node).next = (*cur).next;
                ptr::write_bytes(cur.cast::<u8>(), 0, NODE_SIZE);
            }
        }
    }

    /// First-fit allocation. Returns null if nothing fits (the caller may grow
    /// and retry). Every address sum is checked: a `Layout` too large for the
    /// address space finds no block, instead of wrapping to a small one (#121).
    unsafe fn alloc(&mut self, layout: Layout) -> *mut u8 {
        let align = layout.align().max(MIN_ALIGN);
        let size = layout.size().max(1);

        let mut prev: *mut FreeNode = ptr::null_mut();
        let mut cur = self.head;
        while !cur.is_null() {
            let cs = cur as usize;
            // SAFETY: `cur` is a live free node.
            let bsz = unsafe { (*cur).size };
            let fits = checked_align_up(cs + HEADER_SIZE, align)
                .and_then(|user| Some((user, user.checked_add(size)?)));
            let Some((user, end)) = fits else {
                return ptr::null_mut(); // larger than any block can be
            };
            if end <= cs + bsz {
                let carved_end = align_up(end, NODE_ALIGN).min(cs + bsz);
                let remainder = cs + bsz - carved_end;

                // Unlink this block from the free list.
                // SAFETY: `cur` is a live node.
                let next = unsafe { (*cur).next };
                if prev.is_null() {
                    self.head = next;
                } else {
                    // SAFETY: `prev` is a live node.
                    unsafe { (*prev).next = next };
                }

                // Return any sufficiently large tail to the pool; otherwise fold
                // it into this allocation.
                let block_size = if remainder >= NODE_SIZE {
                    self.insert_free(carved_end, remainder);
                    carved_end - cs
                } else {
                    bsz
                };

                // Record the block for dealloc, just before the user pointer.
                let hdr = (user - HEADER_SIZE) as *mut AllocHeader;
                // SAFETY: `[user - HEADER_SIZE, user)` lies within the carved
                // block (`user >= cs + HEADER_SIZE`) and is now ours.
                unsafe {
                    (*hdr).block_start = cs;
                    (*hdr).block_size = block_size;
                }
                self.allocated += block_size;
                return user as *mut u8;
            }
            prev = cur;
            // SAFETY: `cur` is a live node.
            cur = unsafe { (*cur).next };
        }
        ptr::null_mut()
    }

    /// Free a block previously returned by [`alloc`](Self::alloc).
    unsafe fn dealloc(&mut self, ptr: *mut u8) {
        // SAFETY: `ptr` came from `alloc`, which wrote an `AllocHeader`
        // immediately before it.
        let hdr = unsafe { &*((ptr as usize - HEADER_SIZE) as *const AllocHeader) };
        let (start, size) = (hdr.block_start, hdr.block_size);
        self.allocated -= size;
        // SAFETY: `[start, start + size)` is the block `alloc` carved, which
        // the caller is done with; clearing it (header included) leaves no
        // data behind for the next owner (#69).
        unsafe { ptr::write_bytes(start as *mut u8, 0, size) };
        self.insert_free(start, size);
    }
}

/// The kernel's global heap: a [`Heap`] behind a spinlock, implementing
/// [`GlobalAlloc`].
pub struct KernelHeap {
    inner: SpinLock<Heap>,
}

impl KernelHeap {
    /// Create an empty heap. Must be [`init`](Self::init)ed before use.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            inner: SpinLock::new(Heap::empty()),
        }
    }

    /// Install the `grow` hook (which maps and returns new heap memory) and
    /// seed the first region. Returns whether the initial region was mapped.
    pub fn init(&self, grow: GrowFn) -> bool {
        let mut h = self.inner.lock();
        h.grow = Some(grow);
        h.grow_now(0)
    }

    /// Self-test zero-on-free (#69): fill a block, free it, allocate the same
    /// layout again, and check every byte handed out is zero. Returns whether
    /// it held.
    pub fn verify_zero_on_free(&self) -> bool {
        let layout = Layout::from_size_align(256, 16).expect("heap: self-test layout");
        // SAFETY: a non-zero-size layout; each pointer is checked for null,
        // written or read only within its `layout.size()` bytes, and freed
        // once with the same layout.
        unsafe {
            let first = GlobalAlloc::alloc(self, layout);
            if first.is_null() {
                return false;
            }
            ptr::write_bytes(first, 0xA5, layout.size());
            GlobalAlloc::dealloc(self, first, layout);
            let again = GlobalAlloc::alloc(self, layout);
            if again.is_null() {
                return false;
            }
            let clean = (0..layout.size()).all(|i| again.add(i).read() == 0);
            GlobalAlloc::dealloc(self, again, layout);
            clean
        }
    }

    /// Current `(bytes live, bytes mapped)`.
    pub fn stats(&self) -> (usize, usize) {
        let h = self.inner.lock();
        (h.allocated, h.total)
    }
}

impl Default for KernelHeap {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: `alloc`/`dealloc` serialize through the spinlock and uphold the
// `GlobalAlloc` contract - `alloc` returns either null or a `layout`-aligned
// block of at least `layout.size()` bytes, and `dealloc` is only called with
// pointers `alloc` returned.
unsafe impl GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut h = self.inner.lock();
        // SAFETY: standard `GlobalAlloc` call.
        let p = unsafe { h.alloc(layout) };
        if !p.is_null() {
            return p;
        }
        // Out of room: grow by at least the worst-case need, then retry once.
        // A need that overflows cannot be met at all (#121).
        let Some(need) = layout
            .size()
            .checked_add(layout.align())
            .and_then(|n| n.checked_add(HEADER_SIZE + NODE_SIZE))
        else {
            return ptr::null_mut();
        };
        if h.grow_now(need) {
            // SAFETY: as above.
            unsafe { h.alloc(layout) }
        } else {
            ptr::null_mut()
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        // SAFETY: `ptr` was returned by `alloc`; the stored header describes it.
        unsafe { self.inner.lock().dealloc(ptr) }
    }
}
