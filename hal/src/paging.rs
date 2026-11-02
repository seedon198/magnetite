//! Virtual memory: address spaces and the page-table walker.
//!
//! The pure [`page_table`] crate encodes entries, flags, and the virtual-
//! address split (no `unsafe`, host-tested). This module supplies the parts
//! that need hardware: the walk that reads and writes page tables through the
//! HHDM, allocation of intermediate tables from the frame allocator (#38), and
//! the `CR3` / `invlpg` instructions. See
//! [ADR 0011](../../docs/adr/0011-page-tables.md).
//!
//! An [`AddressSpace`] owns a root PML4 frame. The kernel address space is
//! built by [`AddressSpace::new_kernel`], which inherits the bootloader's
//! higher-half mappings (kernel image + HHDM) and leaves the lower half empty
//! for [`map`](AddressSpace::map) / [`unmap`](AddressSpace::unmap) to manage at
//! 4 KiB granularity. Large pages are created only by the bootloader; `map`
//! makes 4 KiB leaves, while [`translate`](AddressSpace::translate) understands
//! huge entries so it reads inherited mappings correctly.

use core::arch::asm;

use frame_alloc::PhysFrame;
use page_table::{table_indices, PageTableEntry, PageTableFlags, ENTRY_COUNT, PAGE_SIZE};

pub use page_table::{PageTableFlags as Flags, HUGE_1G, HUGE_2M, PAGE_SIZE as PAGE};

use crate::frame;

/// Index of the first higher-half PML4 entry (the kernel + HHDM live at or
/// above this; the lower half, below it, is per-address-space user space).
const HIGHER_HALF_START: usize = ENTRY_COUNT / 2;

/// Why a [`AddressSpace::map`] call failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapError {
    /// The frame allocator had no free frame for an intermediate table.
    OutOfFrames,
    /// The target virtual address is already mapped.
    AlreadyMapped,
    /// The virtual or physical address was not 4 KiB-aligned.
    Misaligned,
    /// A user mapping at or above [`USER_TOP`], where user code may never be.
    NotUserAddress,
    /// The path to the address runs through a large page, which has no child
    /// table to map into (see [`AddressSpace::split_huge`]).
    HugePage,
    /// The leaf flags include [`PageTableFlags::HUGE`]: on a 4 KiB entry that
    /// bit means PAT, never a large page.
    BadFlags,
}

/// Why a [`AddressSpace::protect`] call failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtectError {
    /// No 4 KiB leaf mapping exists for the address.
    NotMapped,
    /// The address is mapped by a large page, which this 4 KiB-granular call
    /// cannot repermission.
    HugePage,
    /// The flags include [`PageTableFlags::HUGE`] (PAT on a 4 KiB entry).
    BadFlags,
}

/// The end of the mappable user range. The last page of the lower half
/// (`0x7fff_ffff_f000`) is never mapped for user code: a `syscall` in it would
/// return to the non-canonical `0x8000_0000_0000`, and `sysretq` then faults
/// in ring 0 on Intel with the user's stack pointer already loaded.
pub const USER_TOP: u64 = 0x7fff_ffff_f000;

// A user instruction ends at or below `USER_TOP`, so the return address
// `syscall` saves is canonical; the entry stub checks it anyway (#103).
const _: () = assert!(USER_TOP < 0x8000_0000_0000);

/// A virtual address space: a root PML4 and the HHDM offset used to reach its
/// tables. A user space must end in [`destroy_user`](Self::destroy_user):
/// it has no `Drop`, and dropping one leaks its tables (#123).
#[must_use = "a user AddressSpace must be passed to destroy_user"]
pub struct AddressSpace {
    root: PhysFrame,
    hhdm: u64,
}

impl AddressSpace {
    /// Build the kernel address space: allocate a fresh PML4 and copy the
    /// bootloader's higher-half top-level entries (kernel image + HHDM) into
    /// it, leaving the lower half empty. Returns `None` if no frame is free.
    ///
    /// Call [`switch`](Self::switch) to install it in `CR3`.
    pub fn new_kernel(hhdm: u64) -> Option<Self> {
        let root = frame::alloc_frame()?; // zeroed by the allocator
        let current = read_cr3() & page_table::ADDR_MASK;
        // SAFETY: `current` is the physical base of the active PML4 (from CR3);
        // `root` is a freshly allocated, zeroed 4 KiB frame we exclusively own.
        // Both are mapped in the HHDM at `hhdm`, so these are valid, aligned
        // pointers to 512-entry tables. We copy only the higher-half entries.
        unsafe {
            let src = (hhdm + current) as *const u64;
            let dst = (hhdm + root.addr()) as *mut u64;
            for i in HIGHER_HALF_START..ENTRY_COUNT {
                dst.add(i).write_volatile(src.add(i).read_volatile());
            }
        }
        Some(Self { root, hhdm })
    }

    /// Build a user address space: a fresh PML4 whose kernel half (entries
    /// 256 to 511) is copied from `kernel`'s root, and whose user half is
    /// empty. The copied entries have `USER` cleared. Returns `None` if no
    /// frame is free.
    ///
    /// The copied entries point at the kernel's own lower-level tables, so the
    /// kernel half stays shared as long as the kernel never adds a top-level
    /// entry later; [`populate_higher_half`](Self::populate_higher_half) on the
    /// kernel space guarantees that.
    pub fn new_user(kernel: &AddressSpace) -> Option<Self> {
        let root = frame::alloc_frame()?; // zeroed by the allocator
        let hhdm = kernel.hhdm;
        for i in HIGHER_HALF_START..ENTRY_COUNT {
            let e = kernel.entry(kernel.root.addr(), i);
            assert!(e.is_present(), "paging: kernel half not populated");
            // Limine sets `USER` on some kernel-half entries; ring 3 must
            // never reach the kernel half, so the copy is supervisor-only.
            let bits = e.bits() & !PageTableFlags::USER.bits();
            let dst = ((hhdm + root.addr()) as *mut u64).wrapping_add(i);
            // SAFETY: `root` is a freshly allocated, zeroed 4 KiB frame we
            // exclusively own, mapped in the HHDM; `i < 512` keeps the write
            // in bounds and aligned.
            unsafe { dst.write_volatile(bits) }
        }
        Some(Self { root, hhdm })
    }

    /// Whether every kernel-half top-level entry (256 to 511) has `USER`
    /// clear, so nothing in the kernel half is reachable from ring 3 whatever
    /// the lower levels say.
    #[must_use]
    pub fn kernel_half_supervisor_only(&self) -> bool {
        (HIGHER_HALF_START..ENTRY_COUNT).all(|i| {
            !self
                .entry(self.root.addr(), i)
                .flags()
                .contains(PageTableFlags::USER)
        })
    }

    /// Give every empty kernel-half top-level entry (256 to 511) a fresh,
    /// zeroed table, present and writable but not user-accessible. Call once
    /// on the kernel space before creating any user space, so that every
    /// later kernel mapping lands in tables all address spaces share. Returns
    /// `false` if frames ran out.
    pub fn populate_higher_half(&mut self) -> bool {
        for i in HIGHER_HALF_START..ENTRY_COUNT {
            if self.entry(self.root.addr(), i).is_present() {
                continue;
            }
            let Some(table) = frame::alloc_frame() else {
                return false;
            };
            let e = PageTableEntry::new(
                table.addr(),
                PageTableFlags::PRESENT | PageTableFlags::WRITABLE,
            );
            // SAFETY: in-bounds entry of our own PML4, reached via the HHDM;
            // it was empty, and `table` is a fresh zeroed frame we own.
            unsafe { self.entry_ptr(self.root.addr(), i).write_volatile(e.bits()) }
        }
        true
    }

    /// Tear down a user address space: free every frame mapped in its user
    /// half (entries 0 to 255), every table below them, and the root. The
    /// kernel half's tables are shared with every space and are never touched.
    /// Returns the number of frames freed.
    ///
    /// Every user-half frame is the task's own (nothing is shared between
    /// tasks yet), so freeing them is sound once no thread runs in this space.
    ///
    /// # Panics
    /// If this space is the one in `CR3`.
    pub fn destroy_user(self) -> usize {
        assert!(
            read_cr3() & !0xfff != self.root.addr(),
            "paging: destroying the active address space"
        );
        let mut freed = 0;
        for i in 0..HIGHER_HALF_START {
            if let Some(pdpt) = self.next_table(self.root.addr(), i) {
                freed += self.free_subtree(pdpt, 2);
            }
        }
        frame::free_frame(self.root);
        freed + 1
    }

    /// Free the table at `table_phys` and everything below it. `level` 2 is a
    /// PDPT, 1 a page directory, 0 a page table, whose present entries are
    /// leaf frames. A user half holds no huge mappings (`map` makes only
    /// 4 KiB pages), so a huge entry above the leaves is a kernel bug. At the
    /// leaf level bit 7 is PAT, not a large page, so it is not checked there.
    fn free_subtree(&self, table_phys: u64, level: u8) -> usize {
        let mut freed = 0;
        for i in 0..ENTRY_COUNT {
            let e = self.entry(table_phys, i);
            if !e.is_present() {
                continue;
            }
            assert!(
                level == 0 || !e.is_huge(),
                "paging: huge mapping in a user half"
            );
            freed += if level == 0 {
                frame::free_frame(PhysFrame::containing(e.addr()));
                1
            } else {
                self.free_subtree(e.addr(), level - 1)
            };
        }
        frame::free_frame(PhysFrame::containing(table_phys));
        freed + 1
    }

    /// The physical address of this space's root table, to tell spaces apart.
    #[must_use]
    pub fn root_phys(&self) -> u64 {
        self.root.addr()
    }

    /// Install this address space in `CR3`, making its mappings active.
    ///
    /// Every `AddressSpace` maps the kernel higher half by construction
    /// (`new_kernel` inherits it and `new_user` copies it), so the running
    /// kernel, its stack and the HHDM stay mapped across the switch. A safe
    /// function cannot rest on that alone (#67): it checks that this space
    /// maps the code doing the switch, the current stack and its own root
    /// through the HHDM, and panics before loading `CR3` otherwise, rather
    /// than triple-fault after.
    ///
    /// # Panics
    /// If this space does not map the current code, stack or HHDM.
    pub fn switch(&self) {
        let code = Self::switch as *const () as u64;
        let stack = crate::cpu::stack_pointer();
        let tables = self.hhdm + self.root.addr();
        assert!(
            [code, stack, tables]
                .iter()
                .all(|&va| self.translate(va).is_some()),
            "paging: switching to an address space that does not map the kernel"
        );
        // SAFETY: `root` is a valid PML4 that maps the code executing now, the
        // current stack and the HHDM (checked above), so execution continues
        // correctly after the load.
        unsafe { write_cr3(self.root.addr()) }
    }

    /// Map `virt` → `phys` (both 4 KiB-aligned) with `flags`, creating any
    /// missing intermediate tables. `PRESENT` is added automatically.
    ///
    /// # Errors
    /// [`MapError`] if an address is misaligned, the target is already mapped,
    /// or no frame is free for an intermediate table.
    pub fn map(&mut self, virt: u64, phys: u64, flags: PageTableFlags) -> Result<(), MapError> {
        if !page_table::is_page_aligned(virt) || !page_table::is_page_aligned(phys) {
            return Err(MapError::Misaligned);
        }
        if flags.contains(PageTableFlags::USER) && virt >= USER_TOP {
            return Err(MapError::NotUserAddress);
        }
        if flags.contains(PageTableFlags::HUGE) {
            return Err(MapError::BadFlags);
        }
        let idx = table_indices(virt);
        // Intermediate tables must be reachable with at least the leaf's
        // privilege; propagate USER up the path when the leaf is a user page.
        let parent_flags = if flags.contains(PageTableFlags::USER) {
            PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER
        } else {
            PageTableFlags::PRESENT | PageTableFlags::WRITABLE
        };

        let mut table = self.root.addr();
        for &i in &idx[..3] {
            table = self.next_table_create(table, i, parent_flags)?;
        }

        let leaf = self.entry_ptr(table, idx[3]);
        // SAFETY: `leaf` points at a present intermediate table's entry (just
        // walked/created) in the HHDM; reading and writing one u64 is in bounds.
        let existing = PageTableEntry::from_bits(unsafe { leaf.read_volatile() });
        if existing.is_present() {
            return Err(MapError::AlreadyMapped);
        }
        let entry = PageTableEntry::new(phys, flags | PageTableFlags::PRESENT);
        // SAFETY: as above; writing the leaf entry establishes the mapping.
        unsafe { leaf.write_volatile(entry.bits()) }
        invlpg(virt);
        Ok(())
    }

    /// Remove the mapping for `virt`, if present. Returns whether an entry was
    /// cleared. Each intermediate table the removal leaves empty is freed and
    /// unlinked, from the page table up (#64), except the root and the
    /// kernel half's PDPTs, whose top-level entries every user space shares.
    pub fn unmap(&mut self, virt: u64) -> bool {
        let idx = table_indices(virt);
        // tables[l] is the table at level l: PML4, PDPT, PD, PT.
        let mut tables = [self.root.addr(); 4];
        for l in 0..3 {
            match self.next_table(tables[l], idx[l]) {
                Some(next) => tables[l + 1] = next,
                None => return false,
            }
        }
        let leaf = self.entry_ptr(tables[3], idx[3]);
        // SAFETY: `leaf` is an in-bounds entry of a present table reached via
        // the HHDM.
        let entry = PageTableEntry::from_bits(unsafe { leaf.read_volatile() });
        if !entry.is_present() {
            return false;
        }
        // SAFETY: as above; clearing the entry removes the mapping.
        unsafe { leaf.write_volatile(0) }
        for l in (1..4).rev() {
            let shared = l == 1 && idx[0] >= HIGHER_HALF_START;
            if shared || (0..ENTRY_COUNT).any(|i| self.entry(tables[l], i).is_present()) {
                break;
            }
            // SAFETY: in-bounds entry of the present parent table; clearing it
            // unlinks the now-empty table at level `l` before it is freed.
            unsafe { self.entry_ptr(tables[l - 1], idx[l - 1]).write_volatile(0) }
            frame::free_frame(PhysFrame::containing(tables[l]));
        }
        // Also drops any paging-structure cache entries for the freed tables.
        invlpg(virt);
        true
    }

    /// Translate `virt` to the physical address it maps to, or `None` if it is
    /// unmapped. Resolves 4 KiB pages and inherited 2 MiB / 1 GiB huge pages.
    #[must_use]
    pub fn translate(&self, virt: u64) -> Option<u64> {
        let idx = table_indices(virt);
        let mut table = self.root.addr();
        // PML4 (idx 0) and PDPT (idx 1): huge only legal at PDPT (1 GiB).
        let pml4e = self.entry(table, idx[0]);
        if !pml4e.is_present() {
            return None;
        }
        table = pml4e.addr();

        let pdpte = self.entry(table, idx[1]);
        if !pdpte.is_present() {
            return None;
        }
        if pdpte.is_huge() {
            return Some(pdpte.huge_addr(HUGE_1G) | (virt & (HUGE_1G - 1)));
        }
        table = pdpte.addr();

        let pde = self.entry(table, idx[2]);
        if !pde.is_present() {
            return None;
        }
        if pde.is_huge() {
            return Some(pde.huge_addr(HUGE_2M) | (virt & (HUGE_2M - 1)));
        }
        table = pde.addr();

        let pte = self.entry(table, idx[3]);
        if !pte.is_present() {
            return None;
        }
        Some(pte.addr() | page_table::page_offset(virt))
    }

    /// Self-test the mapping path end to end: map a fresh frame at an unused
    /// lower-half address, write and read back a sentinel through it, confirm
    /// `translate`, then unmap and free. Returns whether everything matched.
    /// Intended for early bring-up.
    #[must_use]
    pub fn selftest(&mut self) -> bool {
        const TEST_VA: u64 = 0x5000_0000;
        const SENTINEL: u64 = 0x0BAD_C0DE_CAFE_F00D;
        let Some(f) = frame::alloc_frame() else {
            return false;
        };
        if self
            .map(TEST_VA, f.addr(), PageTableFlags::WRITABLE)
            .is_err()
        {
            frame::free_frame(f);
            return false;
        }
        let ptr = TEST_VA as *mut u64;
        // SAFETY: TEST_VA was just mapped to our freshly allocated frame `f`
        // with write permission, so this read/write stays within that page.
        let (wrote_ok, via_hhdm) = unsafe {
            ptr.write_volatile(SENTINEL);
            let read_back = ptr.read_volatile();
            // Cross-check by reading the same physical frame via the HHDM.
            let hhdm_view = ((self.hhdm + f.addr()) as *const u64).read_volatile();
            (read_back == SENTINEL, hhdm_view)
        };
        let translated = self.translate(TEST_VA) == Some(f.addr());
        self.unmap(TEST_VA);
        frame::free_frame(f);
        wrote_ok && via_hhdm == SENTINEL && translated
    }

    /// Change the flags of an existing 4 KiB mapping for `virt`, keeping its
    /// physical target. `PRESENT` is added automatically.
    ///
    /// # Errors
    /// [`ProtectError::NotMapped`] if `virt` has no leaf mapping, or
    /// [`ProtectError::HugePage`] if it is mapped by a large page.
    pub fn protect(&mut self, virt: u64, flags: PageTableFlags) -> Result<(), ProtectError> {
        if flags.contains(PageTableFlags::HUGE) {
            return Err(ProtectError::BadFlags);
        }
        let idx = table_indices(virt);
        let mut table = self.root.addr();
        for &i in &idx[..3] {
            let e = self.entry(table, i);
            if !e.is_present() {
                return Err(ProtectError::NotMapped);
            }
            if e.is_huge() {
                return Err(ProtectError::HugePage);
            }
            table = e.addr();
        }
        let leaf = self.entry_ptr(table, idx[3]);
        // SAFETY: in-bounds HHDM entry of a present table.
        let e = PageTableEntry::from_bits(unsafe { leaf.read_volatile() });
        if !e.is_present() {
            return Err(ProtectError::NotMapped);
        }
        let entry = PageTableEntry::new(e.addr(), flags | PageTableFlags::PRESENT);
        // SAFETY: as above; rewriting the leaf's flags, keeping its address.
        unsafe { leaf.write_volatile(entry.bits()) }
        invlpg(virt);
        Ok(())
    }

    /// Make sure `virt` is mapped by a 4 KiB leaf: replace each huge entry on
    /// its path (an inherited 1 GiB or 2 MiB page, e.g. in the HHDM) with a
    /// table of smaller pages that maps exactly the same range with the same
    /// flags, so 4 KiB-granular calls such as [`make_read_only`] can then act
    /// on it. Nothing visible changes, so no TLB flush is needed. A no-op if
    /// the path has no huge entry.
    ///
    /// [`make_read_only`]: Self::make_read_only
    ///
    /// # Errors
    /// [`MapError::OutOfFrames`] if no frame is free for a new table; the
    /// mapping is then unchanged (a 1 GiB split already done stays, harmlessly).
    pub fn split_huge(&mut self, virt: u64) -> Result<(), MapError> {
        let idx = table_indices(virt);
        let pml4e = self.entry(self.root.addr(), idx[0]);
        if !pml4e.is_present() {
            return Ok(());
        }
        let mut table = pml4e.addr();
        for (&i, size) in idx[1..3].iter().zip([HUGE_1G, HUGE_2M]) {
            let e = self.entry(table, i);
            if !e.is_present() {
                return Ok(());
            }
            if e.is_huge() {
                self.replace_huge(table, i, e, size)?;
            }
            table = self.entry(table, i).addr();
        }
        Ok(())
    }

    /// Make the existing 4 KiB mapping of `virt` read-only and non-executable,
    /// keeping its target and every other bit (cache type, global). Flushes
    /// the page, including a global or huge TLB entry still covering it.
    ///
    /// # Errors
    /// [`ProtectError::NotMapped`] if `virt` has no leaf mapping, or
    /// [`ProtectError::HugePage`] if it is mapped by a large page (see
    /// [`split_huge`](Self::split_huge)).
    pub fn make_read_only(&mut self, virt: u64) -> Result<(), ProtectError> {
        let idx = table_indices(virt);
        let mut table = self.root.addr();
        for &i in &idx[..3] {
            let e = self.entry(table, i);
            if !e.is_present() {
                return Err(ProtectError::NotMapped);
            }
            if e.is_huge() {
                return Err(ProtectError::HugePage);
            }
            table = e.addr();
        }
        let leaf = self.entry_ptr(table, idx[3]);
        // SAFETY: in-bounds HHDM entry of a present table.
        let bits = unsafe { leaf.read_volatile() };
        if !PageTableEntry::from_bits(bits).is_present() {
            return Err(ProtectError::NotMapped);
        }
        let ro = (bits & !PageTableFlags::WRITABLE.bits()) | PageTableFlags::NO_EXECUTE.bits();
        // SAFETY: as above; clearing WRITABLE and setting NX on the leaf,
        // keeping its address and other bits.
        unsafe { leaf.write_volatile(ro) }
        invlpg(virt);
        Ok(())
    }

    /// The physical target and effective leaf flags for `virt` (resolving huge
    /// pages), or `None` if unmapped. Used to verify permissions.
    #[must_use]
    pub fn query(&self, virt: u64) -> Option<(u64, PageTableFlags)> {
        let idx = table_indices(virt);
        let mut table = self.root.addr();
        let pml4e = self.entry(table, idx[0]);
        if !pml4e.is_present() {
            return None;
        }
        table = pml4e.addr();
        let pdpte = self.entry(table, idx[1]);
        if !pdpte.is_present() {
            return None;
        }
        if pdpte.is_huge() {
            let phys = pdpte.huge_addr(HUGE_1G) | (virt & (HUGE_1G - 1));
            return Some((phys, pdpte.flags()));
        }
        table = pdpte.addr();
        let pde = self.entry(table, idx[2]);
        if !pde.is_present() {
            return None;
        }
        if pde.is_huge() {
            let phys = pde.huge_addr(HUGE_2M) | (virt & (HUGE_2M - 1));
            return Some((phys, pde.flags()));
        }
        table = pde.addr();
        let pte = self.entry(table, idx[3]);
        if !pte.is_present() {
            return None;
        }
        Some((pte.addr() | page_table::page_offset(virt), pte.flags()))
    }

    /// The flags of the top-level (PML4) entry covering `virt`, or `None` if it
    /// is not present. NX is cumulative across levels, so NX set here makes
    /// everything beneath non-executable; this reads it back for verification.
    #[must_use]
    pub fn top_level_flags(&self, virt: u64) -> Option<PageTableFlags> {
        let e = self.entry(self.root.addr(), table_indices(virt)[0]);
        if e.is_present() {
            Some(e.flags())
        } else {
            None
        }
    }

    /// Mark the virtual range `[start, end)` non-executable at the PML4 level,
    /// so every page under those top-level entries is NX. Intended for the
    /// HHDM. Call [`reload`](Self::reload) afterwards to flush the TLB.
    pub fn make_range_nx_toplevel(&mut self, start: u64, end: u64) {
        if end <= start {
            return;
        }
        let first = table_indices(start)[0];
        let last = table_indices(end - 1)[0];
        for i in first..=last {
            let ptr = self.entry_ptr(self.root.addr(), i);
            // SAFETY: in-bounds PML4 entry reached via the HHDM.
            let e = PageTableEntry::from_bits(unsafe { ptr.read_volatile() });
            if e.is_present() {
                let ne = PageTableEntry::new(e.addr(), e.flags() | PageTableFlags::NO_EXECUTE);
                // SAFETY: as above; OR-ing NX into the top-level entry.
                unsafe { ptr.write_volatile(ne.bits()) }
            }
        }
    }

    /// Reload `CR3` with our own root, flushing all non-global TLB entries.
    /// Needed after changing higher-level entries (e.g. PML4 flags).
    pub fn reload(&self) {
        self.switch();
    }

    // --- internal walk helpers (all HHDM-based) ---

    /// Replace huge entry `i` (`e`, mapping `size` bytes) of the table at
    /// `table_phys` with a pointer to a new table of 512 children that map the
    /// same range with the same flags.
    fn replace_huge(
        &self,
        table_phys: u64,
        i: usize,
        e: PageTableEntry,
        size: u64,
    ) -> Result<(), MapError> {
        let new = frame::alloc_frame().ok_or(MapError::OutOfFrames)?;
        for c in 0..ENTRY_COUNT {
            let child = page_table::split_huge(e, size, c).expect("paging: split a huge entry");
            // SAFETY: `new` is a fresh, zeroed frame reached via the HHDM and
            // `c < 512`; nothing maps it as a table yet.
            unsafe { self.entry_ptr(new.addr(), c).write_volatile(child.bits()) }
        }
        // The new link must allow everything a child allows (write and user
        // are AND-ed across levels); the children keep the real permissions.
        let mut link = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
        if e.flags().contains(PageTableFlags::USER) {
            link |= PageTableFlags::USER;
        }
        // SAFETY: in-bounds HHDM entry of a present table. One aligned 8-byte
        // store swaps the huge page for an equivalent table, so the walker
        // sees either the old or the new mapping, never a mix; the children
        // were written first (x86 keeps stores in order).
        unsafe {
            self.entry_ptr(table_phys, i)
                .write_volatile(PageTableEntry::new(new.addr(), link).bits());
        }
        Ok(())
    }

    /// Pointer to entry `i` of the table at physical `table_phys`, via the HHDM.
    fn entry_ptr(&self, table_phys: u64, i: usize) -> *mut u64 {
        // Each entry is 8 bytes; `i < 512`.
        ((self.hhdm + table_phys) as *mut u64).wrapping_add(i)
    }

    /// Read entry `i` of the table at physical `table_phys`.
    fn entry(&self, table_phys: u64, i: usize) -> PageTableEntry {
        // SAFETY: `table_phys` is a page-table frame mapped in the HHDM and
        // `i < 512`, so the pointer is in bounds and aligned for a u64 read.
        PageTableEntry::from_bits(unsafe { self.entry_ptr(table_phys, i).read_volatile() })
    }

    /// Follow entry `i` to the next-level table, or `None` if not present (or a
    /// huge mapping, which has no child table).
    fn next_table(&self, table_phys: u64, i: usize) -> Option<u64> {
        let e = self.entry(table_phys, i);
        if e.is_present() && !e.is_huge() {
            Some(e.addr())
        } else {
            None
        }
    }

    /// Like [`next_table`](Self::next_table), but allocate and link a zeroed
    /// table when entry `i` is absent.
    fn next_table_create(
        &self,
        table_phys: u64,
        i: usize,
        parent_flags: PageTableFlags,
    ) -> Result<u64, MapError> {
        let ptr = self.entry_ptr(table_phys, i);
        // SAFETY: in-bounds HHDM entry of a present table.
        let e = PageTableEntry::from_bits(unsafe { ptr.read_volatile() });
        if e.is_present() && e.is_huge() {
            // A large page has no child table; writing into it as one would
            // corrupt the memory it maps (#120).
            return Err(MapError::HugePage);
        }
        if e.is_present() {
            // Upgrade an existing intermediate so the whole path permits at
            // least `parent_flags`. U/S and R/W are cumulative across levels, so
            // reusing a kernel-created table (e.g. left behind by an earlier
            // mapping) for a user page requires OR-ing USER/WRITABLE into it.
            let want = parent_flags.bits();
            if e.flags().bits() & want != want {
                let upgraded = PageTableEntry::new(e.addr(), e.flags() | parent_flags);
                // SAFETY: as above; widening an intermediate entry's flags,
                // keeping its address.
                unsafe { ptr.write_volatile(upgraded.bits()) }
            }
            return Ok(e.addr());
        }
        let new = frame::alloc_frame().ok_or(MapError::OutOfFrames)?; // zeroed
        let entry = PageTableEntry::new(new.addr(), parent_flags);
        // SAFETY: as above; linking the new intermediate table.
        unsafe { ptr.write_volatile(entry.bits()) }
        Ok(new.addr())
    }
}

/// Read the current `CR3` (the active PML4 physical base plus flags).
fn read_cr3() -> u64 {
    let value: u64;
    // SAFETY: reading CR3 is a side-effect-free privileged register read.
    unsafe { asm!("mov {}, cr3", out(reg) value, options(nomem, nostack, preserves_flags)) }
    value
}

/// Load `pml4_phys` into `CR3`, switching the active address space and flushing
/// non-global TLB entries.
///
/// # Safety
/// `pml4_phys` must be the physical base of a valid PML4 that maps the
/// currently executing code, its stack, and the HHDM.
unsafe fn write_cr3(pml4_phys: u64) {
    // SAFETY: guaranteed by the caller's contract above.
    unsafe { asm!("mov cr3, {}", in(reg) pml4_phys, options(nostack, preserves_flags)) }
}

/// The `IA32_EFER` model-specific register.
const IA32_EFER: u32 = 0xC000_0080;
/// EFER's No-Execute-Enable bit; must be set for NX page flags to be honored.
const EFER_NXE: u64 = 1 << 11;

/// Enable the No-Execute (NX) feature in `EFER`, so the NX bit on page-table
/// entries is enforced. Idempotent; a no-op if already enabled.
pub fn enable_nx() {
    let efer = rdmsr(IA32_EFER);
    if efer & EFER_NXE == 0 {
        // SAFETY: setting EFER.NXE only makes the CPU honor the NX page-flag;
        // it changes no existing mapping other than to enforce NX where set.
        unsafe { wrmsr(IA32_EFER, efer | EFER_NXE) }
    }
}

/// Read a model-specific register.
fn rdmsr(msr: u32) -> u64 {
    let (hi, lo): (u32, u32);
    // SAFETY: a plain MSR read with no memory effects; IA32_EFER exists on all
    // x86-64 CPUs.
    unsafe {
        asm!("rdmsr", in("ecx") msr, out("edx") hi, out("eax") lo,
             options(nomem, nostack, preserves_flags))
    }
    ((hi as u64) << 32) | (lo as u64)
}

/// Write a model-specific register.
///
/// # Safety
/// `msr` must be a writable MSR and `value` a valid value for it.
unsafe fn wrmsr(msr: u32, value: u64) {
    let (hi, lo) = ((value >> 32) as u32, value as u32);
    // SAFETY: guaranteed by the caller's contract above.
    unsafe {
        asm!("wrmsr", in("ecx") msr, in("edx") hi, in("eax") lo,
             options(nostack, preserves_flags))
    }
}

/// Invalidate the TLB entry for one virtual address.
fn invlpg(virt: u64) {
    // SAFETY: `invlpg` only flushes a TLB entry; it cannot violate memory
    // safety regardless of the operand.
    unsafe { asm!("invlpg [{}]", in(reg) virt, options(nostack, preserves_flags)) }
}

// Keep the public page-size constant referenced so re-exports stay meaningful.
const _: () = assert!(PAGE_SIZE == 4096);
