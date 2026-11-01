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

pub use page_table::{PageTableFlags as Flags, PAGE_SIZE as PAGE};

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
}

/// Why a [`AddressSpace::protect`] call failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtectError {
    /// No 4 KiB leaf mapping exists for the address.
    NotMapped,
    /// The address is mapped by a large page, which this 4 KiB-granular call
    /// cannot repermission.
    HugePage,
}

/// The end of the mappable user range. The last page of the lower half
/// (`0x7fff_ffff_f000`) is never mapped for user code: a `syscall` in it would
/// return to the non-canonical `0x8000_0000_0000`, and `sysretq` then faults
/// in ring 0 on Intel with the user's stack pointer already loaded.
pub const USER_TOP: u64 = 0x7fff_ffff_f000;

/// A virtual address space: a root PML4 and the HHDM offset used to reach its
/// tables.
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

    /// The physical address of this space's root table, to tell spaces apart.
    #[must_use]
    pub fn root_phys(&self) -> u64 {
        self.root.addr()
    }

    /// Install this address space in `CR3`, making its mappings active.
    ///
    /// Sound because every `AddressSpace` maps the kernel higher half by
    /// construction, so the currently-executing kernel, its stack, and the
    /// HHDM remain mapped across the switch.
    pub fn switch(&self) {
        // SAFETY: `root` is a valid PML4 that maps the kernel higher half (see
        // the type invariant), so execution continues correctly after the load.
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
    /// cleared. Does not reclaim now-empty intermediate tables (see ADR 0011).
    pub fn unmap(&mut self, virt: u64) -> bool {
        let idx = table_indices(virt);
        let mut table = self.root.addr();
        for &i in &idx[..3] {
            match self.next_table(table, i) {
                Some(next) => table = next,
                None => return false,
            }
        }
        let leaf = self.entry_ptr(table, idx[3]);
        // SAFETY: `leaf` is an in-bounds entry of a present table reached via
        // the HHDM.
        let entry = PageTableEntry::from_bits(unsafe { leaf.read_volatile() });
        if !entry.is_present() {
            return false;
        }
        // SAFETY: as above; clearing the entry removes the mapping.
        unsafe { leaf.write_volatile(0) }
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
            return Some(pdpte.addr() | (virt & ((1 << 30) - 1)));
        }
        table = pdpte.addr();

        let pde = self.entry(table, idx[2]);
        if !pde.is_present() {
            return None;
        }
        if pde.is_huge() {
            return Some(pde.addr() | (virt & ((1 << 21) - 1)));
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
            return Some((pdpte.addr() | (virt & ((1 << 30) - 1)), pdpte.flags()));
        }
        table = pdpte.addr();
        let pde = self.entry(table, idx[2]);
        if !pde.is_present() {
            return None;
        }
        if pde.is_huge() {
            return Some((pde.addr() | (virt & ((1 << 21) - 1)), pde.flags()));
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
