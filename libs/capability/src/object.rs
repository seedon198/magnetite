//! Kernel object references and **Untyped-memory** accounting.
//!
//! A capability names a kernel object by an [`ObjectRef`] - its [`ObjectKind`]
//! and the physical address of its storage. Object storage comes from
//! retyping [`Untyped`] memory (ADR 0016): `Untyped` is a physical region with
//! a bump **watermark**; carving advances it and is never individually
//! reclaimed (you revoke the whole region). This module is the pure accounting
//! (no `unsafe`, host-tested); the HAL zeroes the carved physical memory
//! through the HHDM when it performs a retype.

use core::mem::{align_of, size_of};

use crate::space::Slot;

/// The kind of a kernel object a capability can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    /// Unused physical memory that can be retyped into other objects.
    Untyped,
    /// A node of a capability space (holds capability slots).
    CNode,
    /// A physical page frame (mappable).
    Frame,
    /// A synchronous IPC rendezvous point.
    Endpoint,
    /// An asynchronous signal.
    Notification,
    /// A page-table root (address space).
    AddressSpace,
    /// A schedulable execution context.
    Thread,
}

/// A reference to a kernel object: its kind and the physical address of its
/// storage (in retyped Untyped memory, reached by the kernel via the HHDM).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectRef {
    /// What kind of object this is - tells the kernel how to interpret it.
    pub kind: ObjectKind,
    /// Physical address of the object's storage.
    pub phys: u64,
}

impl ObjectRef {
    /// A reference to a `kind` object stored at physical `phys`.
    #[must_use]
    pub const fn new(kind: ObjectKind, phys: u64) -> Self {
        Self { kind, phys }
    }
}

impl ObjectKind {
    /// The `(size, align)` of the storage a single object of this kind needs,
    /// for the **fixed-size** kinds. `None` for kinds whose size is not fixed:
    /// a `CNode` is sized by its slot count ([`cnode_bytes`]), `Untyped` is a
    /// region rather than a fixed object, and `Thread` lands with M4.
    #[must_use]
    pub const fn fixed_layout(self) -> Option<(u64, u64)> {
        match self {
            // A Frame is one page; an AddressSpace is a PML4 (also one page).
            ObjectKind::Frame | ObjectKind::AddressSpace => Some((4096, 4096)),
            ObjectKind::Endpoint => Some((
                size_of::<EndpointState>() as u64,
                align_of::<EndpointState>() as u64,
            )),
            ObjectKind::Notification => Some((
                size_of::<NotificationState>() as u64,
                align_of::<NotificationState>() as u64,
            )),
            ObjectKind::Untyped | ObjectKind::CNode | ObjectKind::Thread => None,
        }
    }
}

/// The storage state of an [`Endpoint`](ObjectKind::Endpoint), living in its
/// retyped memory. Zeroed on creation, which is an empty queue. The `ipc` crate
/// owns the encoding of these words (ADR 0018); the layout stays four words.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct EndpointState {
    /// First thread blocked on this endpoint, encoded `id + 1` (0 = none).
    pub queue_head: u64,
    /// Last thread blocked on this endpoint, encoded `id + 1` (0 = none).
    pub queue_tail: u64,
    /// Whether the blocked threads are senders or receivers (0 = empty).
    pub queue_kind: u64,
    _reserved: u64,
}

/// The storage state of a [`Notification`](ObjectKind::Notification).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct NotificationState {
    /// Accumulated signal word (set bits are pending signals).
    pub signal: u64,
    /// Head of the waiter queue (0 = none). Unused until Notification wait
    /// lands (M5, #162).
    pub waiters: u64,
}

/// Bytes of storage one capability slot occupies in a CNode.
#[must_use]
pub const fn slot_bytes() -> usize {
    size_of::<Slot<ObjectRef>>()
}

/// Alignment of a CNode's slot storage.
#[must_use]
pub const fn slot_align() -> usize {
    align_of::<Slot<ObjectRef>>()
}

/// Bytes of storage a CNode of `slots` capability slots needs.
#[must_use]
pub const fn cnode_bytes(slots: usize) -> usize {
    slots * slot_bytes()
}

/// Round `addr` up to a power-of-two `align`, or `None` on overflow.
const fn align_up(addr: u64, align: u64) -> Option<u64> {
    match addr.checked_add(align - 1) {
        Some(x) => Some(x & !(align - 1)),
        None => None,
    }
}

/// Bump-watermark accounting for a region of Untyped physical memory.
///
/// Objects are carved by advancing [`watermark`](Self::watermark) from
/// [`base`](Self::base); nothing is freed individually (reclaim the whole
/// region by revoking it). This type does the arithmetic only - the kernel
/// zeroes the carved memory when it retypes.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Untyped {
    base: u64,
    size: u64,
    watermark: u64,
}

impl Untyped {
    /// An untyped region `[base, base + size)` with nothing yet carved.
    #[must_use]
    pub const fn new(base: u64, size: u64) -> Self {
        Self {
            base,
            size,
            watermark: base,
        }
    }

    /// Physical base of the region.
    #[must_use]
    pub const fn base(&self) -> u64 {
        self.base
    }

    /// Total size of the region in bytes.
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// Current watermark (first unused physical address).
    #[must_use]
    pub const fn watermark(&self) -> u64 {
        self.watermark
    }

    /// Bytes still available for carving (after the watermark).
    #[must_use]
    pub const fn free(&self) -> u64 {
        (self.base + self.size) - self.watermark
    }

    /// Carve `count` objects of `size` bytes each, aligned to `align` (a power
    /// of two), advancing the watermark past all of them. Returns the physical
    /// base of the first object, or `None` if the request is malformed or does
    /// not fit (the watermark is unchanged on failure).
    pub fn allocate(&mut self, size: u64, align: u64, count: u64) -> Option<u64> {
        if size == 0 || count == 0 || align == 0 || !align.is_power_of_two() {
            return None;
        }
        let start = align_up(self.watermark, align)?;
        let total = size.checked_mul(count)?;
        let end = start.checked_add(total)?;
        if end > self.base.checked_add(self.size)? {
            return None;
        }
        self.watermark = end;
        Some(start)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocate_advances_watermark_and_aligns() {
        let mut u = Untyped::new(0x1_0000, 0x4000); // 16 KiB at 64 KiB
        assert_eq!(u.watermark(), 0x1_0000);
        // 2 objects of 100 bytes, 64-aligned.
        let a = u.allocate(100, 64, 2).unwrap();
        assert_eq!(a, 0x1_0000); // already aligned
        assert_eq!(u.watermark(), 0x1_0000 + 200);
        // Next allocation is realigned up to 64.
        let b = u.allocate(8, 64, 1).unwrap();
        assert_eq!(b % 64, 0);
        assert!(b >= 0x1_0000 + 200);
    }

    #[test]
    fn allocate_rejects_overflow_of_region() {
        let mut u = Untyped::new(0, 0x1000); // 4 KiB
                                             // One 4 KiB object fits exactly.
        assert_eq!(u.allocate(0x1000, 0x1000, 1), Some(0));
        assert_eq!(u.free(), 0);
        // Nothing more fits.
        assert_eq!(u.allocate(1, 1, 1), None);
    }

    #[test]
    fn allocate_count_must_all_fit() {
        let mut u = Untyped::new(0, 0x1000);
        // 5 x 1 KiB = 5 KiB does not fit in 4 KiB; reject entirely.
        assert_eq!(u.allocate(1024, 1024, 5), None);
        // Watermark untouched on failure.
        assert_eq!(u.watermark(), 0);
        // 4 do fit.
        assert_eq!(u.allocate(1024, 1024, 4), Some(0));
    }

    #[test]
    fn allocate_validates_arguments() {
        let mut u = Untyped::new(0x2000, 0x2000);
        assert_eq!(u.allocate(0, 8, 1), None); // zero size
        assert_eq!(u.allocate(8, 0, 1), None); // zero align
        assert_eq!(u.allocate(8, 24, 1), None); // non-power-of-two align
        assert_eq!(u.allocate(8, 8, 0), None); // zero count
        assert_eq!(u.watermark(), 0x2000); // unchanged
    }

    #[test]
    fn allocate_detects_arithmetic_overflow() {
        let mut u = Untyped::new(u64::MAX - 0xfff, 0x1000);
        // A count that overflows u64 is rejected, not wrapped.
        assert_eq!(u.allocate(u64::MAX, 1, 2), None);
    }

    #[test]
    fn object_ref_carries_kind_and_addr() {
        let r = ObjectRef::new(ObjectKind::Frame, 0x3000);
        assert_eq!(r.kind, ObjectKind::Frame);
        assert_eq!(r.phys, 0x3000);
    }

    #[test]
    fn fixed_layouts_are_sane() {
        assert_eq!(ObjectKind::Frame.fixed_layout(), Some((4096, 4096)));
        assert_eq!(ObjectKind::AddressSpace.fixed_layout(), Some((4096, 4096)));
        let (esz, eal) = ObjectKind::Endpoint.fixed_layout().unwrap();
        assert!(esz > 0 && eal.is_power_of_two());
        assert!(ObjectKind::Notification.fixed_layout().is_some());
        // Variable- or not-yet-sized kinds have no fixed layout.
        assert_eq!(ObjectKind::Untyped.fixed_layout(), None);
        assert_eq!(ObjectKind::CNode.fixed_layout(), None);
        assert_eq!(ObjectKind::Thread.fixed_layout(), None);
    }

    #[test]
    fn endpoint_state_is_four_words() {
        let s = EndpointState {
            queue_head: 1,
            queue_tail: 2,
            queue_kind: 3,
            ..EndpointState::default()
        };
        assert_eq!(size_of::<EndpointState>(), 32);
        assert_eq!((s.queue_head, s.queue_tail, s.queue_kind), (1, 2, 3));
    }

    #[test]
    fn cnode_bytes_scales_with_slots() {
        assert_eq!(cnode_bytes(0), 0);
        assert_eq!(cnode_bytes(16), 16 * slot_bytes());
        assert!(slot_align().is_power_of_two());
    }
}
