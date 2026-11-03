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
    /// One page-table page, at any paging level below the root
    /// ([ADR 0023](../../docs/adr/0023-user-threads-and-address-spaces.md)).
    PageTable,
}

/// Where a Frame or PageTable capability is mapped
/// ([ADR 0023](../../docs/adr/0023-user-threads-and-address-spaces.md)),
/// packed into the capability's badge word, which those kinds do not
/// otherwise use. Badge 0 is "not mapped".
///
/// The record names the address space by ASID and generation, so a record of
/// a destroyed space is stale, never a reference to whatever reuses its ASID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mapping {
    /// The address space's ASID.
    pub asid: u8,
    /// The ASID's generation when the mapping was made.
    pub generation: u32,
    /// The virtual page number mapped at (or, for a table, covered).
    pub vpn: u64,
    /// The paging level: 0 for a frame, 1 to 3 for a table installed as a
    /// PDPT, page directory or page table.
    pub level: u8,
}

impl Mapping {
    /// ASIDs: 6 bits.
    pub const ASIDS: usize = 64;
    /// Generations: 19 bits, then they wrap.
    pub const GENERATIONS: u32 = 1 << 19;
    const MAPPED: u64 = 1 << 63;
    const LEVEL_SHIFT: u32 = 61;
    const ASID_SHIFT: u32 = 55;
    const GEN_SHIFT: u32 = 36;
    const VPN_MASK: u64 = (1 << 36) - 1;

    /// Pack into a badge word. `None` if a field is out of range.
    #[must_use]
    pub const fn pack(self) -> Option<u64> {
        if self.level > 3
            || self.asid as usize >= Self::ASIDS
            || self.generation >= Self::GENERATIONS
            || self.vpn > Self::VPN_MASK
        {
            return None;
        }
        Some(
            Self::MAPPED
                | ((self.level as u64) << Self::LEVEL_SHIFT)
                | ((self.asid as u64) << Self::ASID_SHIFT)
                | ((self.generation as u64) << Self::GEN_SHIFT)
                | self.vpn,
        )
    }

    /// Unpack a badge word; `None` for an unmapped capability.
    #[must_use]
    pub const fn unpack(badge: u64) -> Option<Self> {
        if badge & Self::MAPPED == 0 {
            return None;
        }
        Some(Self {
            level: ((badge >> Self::LEVEL_SHIFT) & 0b11) as u8,
            asid: ((badge >> Self::ASID_SHIFT) & 0b11_1111) as u8,
            generation: ((badge >> Self::GEN_SHIFT) as u32) & (Self::GENERATIONS - 1),
            vpn: badge & Self::VPN_MASK,
        })
    }

    /// The virtual address mapped at.
    #[must_use]
    pub const fn vaddr(self) -> u64 {
        self.vpn << 12
    }
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
            // A Frame is one page; an AddressSpace is a PML4, and a PageTable
            // one table below it (also one page each).
            ObjectKind::Frame | ObjectKind::AddressSpace | ObjectKind::PageTable => {
                Some((4096, 4096))
            }
            ObjectKind::Endpoint => Some((
                size_of::<EndpointState>() as u64,
                align_of::<EndpointState>() as u64,
            )),
            ObjectKind::Notification => Some((
                size_of::<NotificationState>() as u64,
                align_of::<NotificationState>() as u64,
            )),
            // A Thread is its kernel stack: four pages (ADR 0023).
            ObjectKind::Thread => Some((16384, 4096)),
            ObjectKind::Untyped | ObjectKind::CNode => None,
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

/// The storage state of a [`Notification`](ObjectKind::Notification), living
/// in its retyped memory. Zeroed on creation: no pending bits and no waiters.
/// The `ipc` crate owns the encoding of the queue words
/// ([ADR 0022](../../docs/adr/0022-notification-wait.md)).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct NotificationState {
    /// Accumulated signal word (set bits are pending signals).
    pub signal: u64,
    /// First thread waiting, encoded `id + 1` (0 = none).
    pub waiters: u64,
    /// Last thread waiting, encoded `id + 1` (0 = none).
    pub waiters_tail: u64,
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

    /// Bytes at the start of a child region split from an Untyped
    /// ([`child`](Self::child)) that hold its own control block.
    pub const CHILD_HEADER: u64 = 64;

    /// The smallest child region, and the unit its size comes in: a page.
    pub const CHILD_UNIT: u64 = 4096;

    /// The control block of a child Untyped split from its parent over the
    /// carved storage `[base, base + size)`: the block itself sits in the
    /// first [`CHILD_HEADER`](Self::CHILD_HEADER) bytes, and the child's
    /// region is the rest, so one carve holds both. `None` unless `size` is a
    /// whole number of pages ([`CHILD_UNIT`](Self::CHILD_UNIT)) and `base` is
    /// page-aligned.
    #[must_use]
    pub const fn child(base: u64, size: u64) -> Option<Self> {
        if size == 0
            || !size.is_multiple_of(Self::CHILD_UNIT)
            || !base.is_multiple_of(Self::CHILD_UNIT)
        {
            return None;
        }
        Some(Self::new(
            base + Self::CHILD_HEADER,
            size - Self::CHILD_HEADER,
        ))
    }

    /// Make the whole region free again: the watermark back to the base.
    /// Only once nothing retyped from it is alive: its capability was
    /// revoked and is the region's only one (ADR 0020).
    pub fn reset(&mut self) {
        self.watermark = self.base;
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
    fn a_child_keeps_its_control_block_out_of_its_region() {
        let c = Untyped::child(0x4_0000, 0x2000).unwrap();
        assert_eq!(c.base(), 0x4_0000 + Untyped::CHILD_HEADER);
        assert_eq!(c.size(), 0x2000 - Untyped::CHILD_HEADER);
        assert!(core::mem::size_of::<Untyped>() as u64 <= Untyped::CHILD_HEADER);
        // A page-aligned object skips the header's page; the region holds one.
        let mut c2 = c;
        assert_eq!(c2.allocate(0x1000, 0x1000, 1), Some(0x4_1000));
        assert_eq!(c2.allocate(0x1000, 0x1000, 1), None);
        // Reset never hands out the header.
        c2.reset();
        assert_eq!(c2.watermark(), 0x4_0000 + Untyped::CHILD_HEADER);
        for (base, size) in [(0x4_0000, 0), (0x4_0000, 0x1800), (0x4_0800, 0x1000)] {
            assert_eq!(Untyped::child(base, size), None);
        }
    }

    #[test]
    fn reset_frees_the_whole_region() {
        let mut u = Untyped::new(0x1_0000, 0x2000);
        assert!(u.allocate(0x2000, 0x1000, 1).is_some());
        assert_eq!(u.free(), 0);
        u.reset();
        assert_eq!((u.watermark(), u.free()), (0x1_0000, 0x2000));
        assert_eq!(u.allocate(0x1000, 0x1000, 1), Some(0x1_0000));
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
    fn mappings_pack_and_unpack() {
        let m = Mapping {
            asid: 63,
            generation: Mapping::GENERATIONS - 1,
            vpn: (1 << 36) - 1,
            level: 3,
        };
        let w = m.pack().unwrap();
        assert_ne!(w, 0, "a mapping is never the unmapped badge");
        assert_eq!(Mapping::unpack(w), Some(m));
        let low = Mapping {
            asid: 0,
            generation: 0,
            vpn: 0x4_0000,
            level: 0,
        };
        assert_eq!(Mapping::unpack(low.pack().unwrap()), Some(low));
        assert_eq!(low.vaddr(), 0x4000_0000);
        assert_eq!(Mapping::unpack(0), None);
        for bad in [
            Mapping { asid: 64, ..low },
            Mapping {
                generation: Mapping::GENERATIONS,
                ..low
            },
            Mapping {
                vpn: 1 << 36,
                ..low
            },
            Mapping { level: 4, ..low },
        ] {
            assert_eq!(bad.pack(), None);
        }
    }

    #[test]
    fn fixed_layouts_are_sane() {
        assert_eq!(ObjectKind::Frame.fixed_layout(), Some((4096, 4096)));
        assert_eq!(ObjectKind::AddressSpace.fixed_layout(), Some((4096, 4096)));
        assert_eq!(ObjectKind::PageTable.fixed_layout(), Some((4096, 4096)));
        let (esz, eal) = ObjectKind::Endpoint.fixed_layout().unwrap();
        assert!(esz > 0 && eal.is_power_of_two());
        assert!(ObjectKind::Notification.fixed_layout().is_some());
        // Variable- or not-yet-sized kinds have no fixed layout.
        assert_eq!(ObjectKind::Untyped.fixed_layout(), None);
        assert_eq!(ObjectKind::CNode.fixed_layout(), None);
        assert_eq!(ObjectKind::Thread.fixed_layout(), Some((16384, 4096)));
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

    #[test]
    fn a_root_cnode_still_fits_one_page() {
        // A slot carries its derivation link too (ADR 0020 decision 7).
        assert_eq!(slot_bytes(), 48);
        assert!(cnode_bytes(64) <= 4096);
    }
}
