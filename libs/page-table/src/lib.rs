#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

//! Pure x86-64 4-level page-table math.
//!
//! This crate has **no** `unsafe` and touches no hardware. It encodes and
//! decodes page-table entries and their flags, and splits a virtual address
//! into its four table indices and page offset. The `unsafe` table walk, and
//! the `CR3`/`invlpg` instructions, live in `hal::paging`, which builds on
//! this; keeping the bit math here, free of `unsafe`, makes it unit-testable on
//! the host. See [ADR 0011](../../docs/adr/0011-page-tables.md).

/// Bytes per 4 KiB page.
pub const PAGE_SIZE: u64 = 4096;

/// Entries per page table at every level (512 × 8 bytes = one 4 KiB frame).
pub const ENTRY_COUNT: usize = 512;

/// Mask selecting the physical frame address bits (12..52) of an entry or
/// `CR3`. Clears flag bits and reserved high bits.
pub const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

/// Flags held in the low 12 bits (plus the NX bit 63) of a page-table entry.
///
/// Combine with `|`; test with [`contains`](PageTableFlags::contains).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageTableFlags(u64);

impl PageTableFlags {
    /// The entry is valid; the referenced table or page is present.
    pub const PRESENT: Self = Self(1 << 0);
    /// Writes are allowed through this entry.
    pub const WRITABLE: Self = Self(1 << 1);
    /// User-mode (ring 3) may access through this entry.
    pub const USER: Self = Self(1 << 2);
    /// Write-through caching for the referenced memory.
    pub const WRITE_THROUGH: Self = Self(1 << 3);
    /// Caching is disabled for the referenced memory (e.g. MMIO).
    pub const NO_CACHE: Self = Self(1 << 4);
    /// Set by the CPU when the entry has been read.
    pub const ACCESSED: Self = Self(1 << 5);
    /// Set by the CPU when the page has been written.
    pub const DIRTY: Self = Self(1 << 6);
    /// This entry maps a large page (2 MiB at PD, 1 GiB at PDPT) rather than
    /// pointing at a lower table.
    pub const HUGE: Self = Self(1 << 7);
    /// The mapping is global - not flushed from the TLB on a `CR3` reload.
    pub const GLOBAL: Self = Self(1 << 8);
    /// Execution is forbidden from the referenced memory (requires EFER.NXE).
    pub const NO_EXECUTE: Self = Self(1 << 63);

    /// No flags set.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// The raw flag bits.
    #[must_use]
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Construct from raw bits, keeping only recognized flag positions (the low
    /// 12 bits and the NX bit); address bits are discarded.
    #[must_use]
    pub const fn from_bits_truncate(bits: u64) -> Self {
        Self(bits & !ADDR_MASK & !(0x7ff << 52))
    }

    /// Whether every flag in `other` is set in `self`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// The union of two flag sets.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl core::ops::BitOr for PageTableFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl core::ops::BitOrAssign for PageTableFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// One 64-bit page-table entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageTableEntry(u64);

impl PageTableEntry {
    /// An empty (not-present) entry.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// An entry pointing at physical address `phys` with `flags`. Only the
    /// frame-aligned address bits of `phys` are kept.
    #[must_use]
    pub const fn new(phys: u64, flags: PageTableFlags) -> Self {
        Self((phys & ADDR_MASK) | flags.bits())
    }

    /// Reinterpret raw entry bits.
    #[must_use]
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    /// The raw entry bits.
    #[must_use]
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Whether the entry is present.
    #[must_use]
    pub const fn is_present(self) -> bool {
        self.0 & PageTableFlags::PRESENT.bits() != 0
    }

    /// Whether the entry maps a large page (its [`PageTableFlags::HUGE`] bit).
    /// Meaningful only at the PDPT (1 GiB) and PD (2 MiB) levels.
    #[must_use]
    pub const fn is_huge(self) -> bool {
        self.0 & PageTableFlags::HUGE.bits() != 0
    }

    /// The physical address this entry points at (a table, or a mapped page).
    #[must_use]
    pub const fn addr(self) -> u64 {
        self.0 & ADDR_MASK
    }

    /// The entry's flags.
    #[must_use]
    pub const fn flags(self) -> PageTableFlags {
        PageTableFlags::from_bits_truncate(self.0)
    }
}

/// Size of a page mapped by a huge PD entry (2 MiB).
pub const HUGE_2M: u64 = 1 << 21;
/// Size of a page mapped by a huge PDPT entry (1 GiB).
pub const HUGE_1G: u64 = 1 << 30;

/// Child entry `i` of the table that replaces `huge`, a large-page entry of
/// `huge_size` ([`HUGE_1G`] or [`HUGE_2M`]): together the 512 children map
/// exactly what `huge` did, with the same flags. A 1 GiB page splits into
/// 2 MiB pages; a 2 MiB page into 4 KiB pages, which moves the PAT bit from
/// bit 12 (huge entries) to bit 7 (4 KiB entries, where bit 7 is PAT, not PS).
/// `None` if `huge` is not a large page, `huge_size` is neither size, or
/// `i >= 512`.
#[must_use]
pub const fn split_huge(huge: PageTableEntry, huge_size: u64, i: usize) -> Option<PageTableEntry> {
    /// Bits 0 to 12 (flags, PS and PAT) and 52 to 63 (available, keys, NX).
    const LOW: u64 = 0x1fff;
    const HIGH: u64 = 0xfff0_0000_0000_0000;
    const PAT_HUGE: u64 = 1 << 12;
    const PAT_4K: u64 = 1 << 7;
    if !huge.is_huge() || i >= ENTRY_COUNT {
        return None;
    }
    let bits = huge.bits();
    let base = bits & ADDR_MASK & !(huge_size - 1);
    let child_size = huge_size / ENTRY_COUNT as u64;
    let addr = base + i as u64 * child_size;
    let keep = bits & (LOW | HIGH);
    match huge_size {
        HUGE_1G => Some(PageTableEntry(keep | addr)),
        HUGE_2M => {
            let pat = if bits & PAT_HUGE != 0 { PAT_4K } else { 0 };
            let flags = keep & !(PAT_HUGE | PageTableFlags::HUGE.bits());
            Some(PageTableEntry(flags | pat | addr))
        }
        _ => None,
    }
}

/// A decoded x86-64 page-fault error code (the value the CPU pushes for #PF).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageFaultError(u64);

impl PageFaultError {
    /// Wrap a raw #PF error code.
    #[must_use]
    pub const fn new(bits: u64) -> Self {
        Self(bits)
    }

    /// Bit 0 - set: a protection violation on a present page; clear: the page
    /// was not present.
    #[must_use]
    pub const fn protection_violation(self) -> bool {
        self.0 & (1 << 0) != 0
    }

    /// Bit 1 - the access was a write (else a read).
    #[must_use]
    pub const fn write(self) -> bool {
        self.0 & (1 << 1) != 0
    }

    /// Bit 2 - the access was from user mode (CPL 3) rather than supervisor.
    #[must_use]
    pub const fn user(self) -> bool {
        self.0 & (1 << 2) != 0
    }

    /// Bit 3 - a reserved bit was set in a paging-structure entry.
    #[must_use]
    pub const fn reserved_write(self) -> bool {
        self.0 & (1 << 3) != 0
    }

    /// Bit 4 - the access was an instruction fetch (NX / SMEP).
    #[must_use]
    pub const fn instruction_fetch(self) -> bool {
        self.0 & (1 << 4) != 0
    }

    /// Bit 5 - a protection-key violation.
    #[must_use]
    pub const fn protection_key(self) -> bool {
        self.0 & (1 << 5) != 0
    }

    /// Bit 6 - a shadow-stack access.
    #[must_use]
    pub const fn shadow_stack(self) -> bool {
        self.0 & (1 << 6) != 0
    }

    /// The raw error-code bits.
    #[must_use]
    pub const fn bits(self) -> u64 {
        self.0
    }
}

/// The four table indices (PML4, PDPT, PD, PT - in that order) a virtual
/// address walks through.
#[must_use]
pub const fn table_indices(virt: u64) -> [usize; 4] {
    [
        ((virt >> 39) & 0x1ff) as usize,
        ((virt >> 30) & 0x1ff) as usize,
        ((virt >> 21) & 0x1ff) as usize,
        ((virt >> 12) & 0x1ff) as usize,
    ]
}

/// The byte offset of `virt` within its 4 KiB page.
#[must_use]
pub const fn page_offset(virt: u64) -> u64 {
    virt & (PAGE_SIZE - 1)
}

/// Whether `addr` is 4 KiB-aligned.
#[must_use]
pub const fn is_page_aligned(addr: u64) -> bool {
    addr & (PAGE_SIZE - 1) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_2m_gives_4k_pages_with_the_same_permissions() {
        let flags = PageTableFlags::PRESENT
            | PageTableFlags::WRITABLE
            | PageTableFlags::GLOBAL
            | PageTableFlags::NO_EXECUTE
            | PageTableFlags::HUGE;
        let huge = PageTableEntry::new(0x4020_0000, flags);
        for i in [0, 1, 255, 511] {
            let e = split_huge(huge, HUGE_2M, i).unwrap();
            assert_eq!(e.addr(), 0x4020_0000 + i as u64 * PAGE_SIZE);
            assert!(!e.is_huge(), "a 4 KiB entry must not carry PS");
            let want = flags.bits() & !PageTableFlags::HUGE.bits();
            assert_eq!(e.flags().bits(), want);
        }
    }

    #[test]
    fn split_2m_moves_pat_from_bit_12_to_bit_7() {
        let huge = PageTableEntry::from_bits(0x0020_0000 | 1 << 12 | 0x83);
        let e = split_huge(huge, HUGE_2M, 3).unwrap();
        assert_eq!(e.bits() & (1 << 7), 1 << 7, "PAT lands in bit 7");
        assert_eq!(e.addr(), 0x0020_3000, "bit 12 is not left in the address");
    }

    #[test]
    fn split_1g_gives_2m_huge_pages() {
        let huge = PageTableEntry::from_bits(0x4000_0000 | 1 << 12 | 1 << 63 | 0x183);
        let e = split_huge(huge, HUGE_1G, 2).unwrap();
        assert!(e.is_huge(), "children of a 1 GiB page are 2 MiB pages");
        assert_eq!(e.bits() & 0x000f_ffff_ffe0_0000, 0x4000_0000 + 2 * HUGE_2M);
        assert_eq!(e.bits() & (1 << 12), 1 << 12, "PAT stays in bit 12");
        assert_eq!(e.bits() & (0x183 | 1 << 63), 0x183 | 1 << 63);
        // Two levels of split reach the same 4 KiB frame directly.
        let leaf = split_huge(e, HUGE_2M, 5).unwrap();
        assert_eq!(leaf.addr(), 0x4000_0000 + 2 * HUGE_2M + 5 * PAGE_SIZE);
    }

    #[test]
    fn split_rejects_non_huge_entries_bad_sizes_and_indices() {
        let huge = PageTableEntry::from_bits(0x0020_0000 | 0x83);
        assert!(split_huge(huge, HUGE_2M, ENTRY_COUNT).is_none());
        assert!(split_huge(huge, PAGE_SIZE, 0).is_none());
        let table = PageTableEntry::new(0x5000, PageTableFlags::PRESENT);
        assert!(split_huge(table, HUGE_2M, 0).is_none());
    }

    #[test]
    fn entry_roundtrips_addr_and_flags() {
        let phys = 0x1234_5000;
        let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
        let e = PageTableEntry::new(phys, flags);
        assert!(e.is_present());
        assert_eq!(e.addr(), phys);
        assert!(e.flags().contains(PageTableFlags::WRITABLE));
        assert!(e.flags().contains(PageTableFlags::NO_EXECUTE));
        assert!(!e.flags().contains(PageTableFlags::USER));
    }

    #[test]
    fn new_discards_low_and_reserved_bits_of_phys() {
        // A misaligned physical address keeps only its frame-aligned bits.
        let e = PageTableEntry::new(0x1234_5fff, PageTableFlags::PRESENT);
        assert_eq!(e.addr(), 0x1234_5000);
    }

    #[test]
    fn flags_decode_ignores_address_bits() {
        let e = PageTableEntry::from_bits(0x1234_5000 | PageTableFlags::PRESENT.bits());
        // Decoding flags must not leak the address bits back in.
        assert_eq!(e.flags(), PageTableFlags::PRESENT);
    }

    #[test]
    fn empty_entry_is_not_present() {
        assert!(!PageTableEntry::empty().is_present());
        assert_eq!(PageTableEntry::empty().bits(), 0);
    }

    #[test]
    fn huge_flag_detected() {
        let e = PageTableEntry::new(0x4000_0000, PageTableFlags::PRESENT | PageTableFlags::HUGE);
        assert!(e.is_huge());
        assert!(!PageTableEntry::new(0x1000, PageTableFlags::PRESENT).is_huge());
    }

    #[test]
    fn indices_invert_composition() {
        let (p4, p3, p2, p1, off) = (511usize, 510, 7, 42, 0x123u64);
        let virt = ((p4 as u64) << 39)
            | ((p3 as u64) << 30)
            | ((p2 as u64) << 21)
            | ((p1 as u64) << 12)
            | off;
        assert_eq!(table_indices(virt), [p4, p3, p2, p1]);
        assert_eq!(page_offset(virt), off);
    }

    #[test]
    fn higher_half_kernel_base_indices() {
        // The kernel's higher-half link address.
        assert_eq!(table_indices(0xffff_ffff_8000_0000), [511, 510, 0, 0]);
        // The HHDM base used by Limine in this project.
        assert_eq!(table_indices(0xffff_8000_0000_0000)[0], 256);
    }

    #[test]
    fn page_fault_error_decodes_bits() {
        // present + write + instruction-fetch (protection violation on a write).
        let e = PageFaultError::new(0b1_0011);
        assert!(e.protection_violation());
        assert!(e.write());
        assert!(!e.user());
        assert!(e.instruction_fetch());
        assert!(!e.reserved_write());

        // not-present, read, user mode.
        let e = PageFaultError::new(0b100);
        assert!(!e.protection_violation());
        assert!(!e.write());
        assert!(e.user());
        assert!(!e.instruction_fetch());
    }

    #[test]
    fn page_alignment() {
        assert!(is_page_aligned(0x1000));
        assert!(!is_page_aligned(0x1001));
        assert!(is_page_aligned(0));
    }

    #[test]
    fn flags_from_bits_truncate_drops_addr() {
        let raw = 0x9876_3000 | PageTableFlags::PRESENT.bits() | PageTableFlags::NO_EXECUTE.bits();
        let f = PageTableFlags::from_bits_truncate(raw);
        assert!(f.contains(PageTableFlags::PRESENT));
        assert!(f.contains(PageTableFlags::NO_EXECUTE));
        // No address bits survived.
        assert_eq!(f.bits() & ADDR_MASK, 0);
    }
}
