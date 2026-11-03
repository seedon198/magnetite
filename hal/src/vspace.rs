//! Address spaces built from objects a task retyped
//! ([ADR 0023](../../docs/adr/0023-user-threads-and-address-spaces.md)).
//!
//! A user-created address space is an `AddressSpace` object (its PML4), the
//! `PageTable` objects installed below it, and the Frames mapped through
//! them. Unlike [`paging::AddressSpace`](crate::paging::AddressSpace), nothing
//! here allocates or frees: every page belongs to the Untyped it was retyped
//! from. These functions only read and write entries, each a `u64` in a page
//! the caller names by a kind-checked [`ObjectRef`].
//!
//! The user half of such a space only ever holds entries these functions
//! wrote, so a walk from its root only meets live `PageTable` objects: the
//! kernel clears an entry before the table it names is destroyed.

use capability::object::{ObjectKind, ObjectRef};
use page_table::{table_indices, PageTableEntry, PageTableFlags, ENTRY_COUNT};

use crate::paging::{invlpg, read_cr3, AddressSpace, USER_TOP};

/// Why a mapping could not be made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VspaceError {
    /// A table level above the address is not installed.
    MissingTable,
    /// Every level is installed already (a table), or the address is mapped
    /// already (a frame).
    AlreadyMapped,
    /// The address is not page-aligned, or not in the user half.
    BadAddress,
}

/// The flags of an entry that links a table: user-reachable and writable, so
/// the leaf decides what a mapping allows.
const LINK: PageTableFlags = PageTableFlags::PRESENT
    .union(PageTableFlags::WRITABLE)
    .union(PageTableFlags::USER);

/// The address of entry `i` of the table page at physical `table`, after
/// checking the page is allocated.
fn entry_ptr(table: u64, i: usize, hhdm: u64) -> *mut u64 {
    assert!(i < ENTRY_COUNT);
    assert!(
        crate::frame::owns(table, 4096),
        "vspace: {table:#x} is not allocated table memory"
    );
    ((hhdm + table) as *mut u64).wrapping_add(i)
}

fn read(table: u64, i: usize, hhdm: u64) -> PageTableEntry {
    let p = entry_ptr(table, i, hhdm);
    // SAFETY: `p` is entry `i < 512` of an allocated, page-aligned table page
    // in the HHDM (checked by `entry_ptr`), so the read is in bounds and
    // aligned.
    PageTableEntry::from_bits(unsafe { p.read_volatile() })
}

fn write(table: u64, i: usize, e: PageTableEntry, hhdm: u64) {
    let p = entry_ptr(table, i, hhdm);
    // SAFETY: as in `read`. The page is a table object of this space (the
    // root, or a PageTable installed below it), so changing one entry changes
    // only this space's mappings.
    unsafe { p.write_volatile(e.bits()) }
}

fn of_kind(obj: ObjectRef, kind: ObjectKind) -> u64 {
    assert_eq!(
        obj.kind, kind,
        "vspace: {:#x} used as the wrong kind",
        obj.phys
    );
    obj.phys
}

fn user_page(vaddr: u64) -> Result<(), VspaceError> {
    if !vaddr.is_multiple_of(4096) || vaddr >= USER_TOP {
        return Err(VspaceError::BadAddress);
    }
    Ok(())
}

/// Make the freshly retyped (zeroed) AddressSpace `root` a space: copy in the
/// kernel half from `kernel`, supervisor-only, as every user space has it.
pub fn init_root(root: ObjectRef, kernel: &AddressSpace, hhdm: u64) {
    let root = of_kind(root, ObjectKind::AddressSpace);
    for i in ENTRY_COUNT / 2..ENTRY_COUNT {
        let e = PageTableEntry::from_bits(kernel.kernel_root_entry(i));
        write(root, i, e, hhdm);
    }
}

/// The tables on the path to `vaddr`, from the root: `tables[l]` is the table
/// at level `l` (PML4, PDPT, PD, PT), and `found` how many are installed.
fn walk(root: u64, vaddr: u64, hhdm: u64) -> ([u64; 4], usize) {
    let idx = table_indices(vaddr);
    let mut tables = [root, 0, 0, 0];
    for l in 0..3 {
        let e = read(tables[l], idx[l], hhdm);
        if !e.is_present() {
            return (tables, l + 1);
        }
        tables[l + 1] = e.addr();
    }
    (tables, 4)
}

/// Install the PageTable `table` in address space `root` at the first level
/// above `vaddr` with no table yet. Returns that level: 1 for a PDPT, 2 for a
/// page directory, 3 for a page table.
///
/// # Errors
/// [`VspaceError::BadAddress`] outside the user half, and
/// [`VspaceError::AlreadyMapped`] if all three levels are installed.
pub fn install_table(
    root: ObjectRef,
    vaddr: u64,
    table: ObjectRef,
    hhdm: u64,
) -> Result<u8, VspaceError> {
    let root = of_kind(root, ObjectKind::AddressSpace);
    let table = of_kind(table, ObjectKind::PageTable);
    user_page(vaddr & !0xfff)?;
    let (tables, found) = walk(root, vaddr, hhdm);
    if found == 4 {
        return Err(VspaceError::AlreadyMapped);
    }
    let parent = found - 1;
    let i = table_indices(vaddr)[parent];
    write(tables[parent], i, PageTableEntry::new(table, LINK), hhdm);
    Ok(found as u8)
}

/// Undo [`install_table`]: clear the entry at `level` above `vaddr` in
/// `root`, if it still links `table`, and flush the TLB if `root` is live.
/// Returns whether it did.
pub fn uninstall_table(
    root: ObjectRef,
    vaddr: u64,
    level: u8,
    table: ObjectRef,
    hhdm: u64,
) -> bool {
    let root = of_kind(root, ObjectKind::AddressSpace);
    let table = of_kind(table, ObjectKind::PageTable);
    let level = usize::from(level);
    assert!((1..=3).contains(&level));
    let (tables, found) = walk(root, vaddr, hhdm);
    if found <= level || tables[level] != table {
        return false;
    }
    let i = table_indices(vaddr)[level - 1];
    write(tables[level - 1], i, PageTableEntry::empty(), hhdm);
    if read_cr3() & page_table::ADDR_MASK == root {
        // A whole subtree went: flush every non-global entry.
        // SAFETY: reloading the CR3 already in use changes no mapping.
        unsafe { crate::paging::write_cr3(root) }
    }
    true
}

/// Map Frame `frame` at `vaddr` in address space `root` with `flags` (the
/// leaf's; `USER` and `PRESENT` are added).
///
/// # Errors
/// [`VspaceError::BadAddress`], [`VspaceError::MissingTable`] if a level
/// above is not installed, and [`VspaceError::AlreadyMapped`].
pub fn map_frame(
    root: ObjectRef,
    vaddr: u64,
    frame: ObjectRef,
    flags: PageTableFlags,
    hhdm: u64,
) -> Result<(), VspaceError> {
    let root = of_kind(root, ObjectKind::AddressSpace);
    let frame = of_kind(frame, ObjectKind::Frame);
    user_page(vaddr)?;
    let (tables, found) = walk(root, vaddr, hhdm);
    if found < 4 {
        return Err(VspaceError::MissingTable);
    }
    let i = table_indices(vaddr)[3];
    if read(tables[3], i, hhdm).is_present() {
        return Err(VspaceError::AlreadyMapped);
    }
    let leaf = flags
        .union(PageTableFlags::PRESENT)
        .union(PageTableFlags::USER);
    write(tables[3], i, PageTableEntry::new(frame, leaf), hhdm);
    Ok(())
}

/// Undo [`map_frame`]: clear the leaf at `vaddr` in `root` if it still maps
/// `frame`, and flush it from the TLB. Returns whether it did.
pub fn unmap_frame(root: ObjectRef, vaddr: u64, frame: ObjectRef, hhdm: u64) -> bool {
    let root = of_kind(root, ObjectKind::AddressSpace);
    let frame = of_kind(frame, ObjectKind::Frame);
    let (tables, found) = walk(root, vaddr, hhdm);
    if found < 4 {
        return false;
    }
    let i = table_indices(vaddr)[3];
    let e = read(tables[3], i, hhdm);
    if !e.is_present() || e.addr() != frame {
        return false;
    }
    write(tables[3], i, PageTableEntry::empty(), hhdm);
    invlpg(vaddr);
    true
}

/// The frame mapped at `vaddr` in address space `root`, and the leaf's flags.
#[must_use]
pub fn translate(root: ObjectRef, vaddr: u64, hhdm: u64) -> Option<(u64, PageTableFlags)> {
    let root = of_kind(root, ObjectKind::AddressSpace);
    let (tables, found) = walk(root, vaddr, hhdm);
    if found < 4 {
        return None;
    }
    let e = read(tables[3], table_indices(vaddr)[3], hhdm);
    e.is_present().then(|| (e.addr(), e.flags()))
}

/// Load address space `root` into `CR3`.
pub fn activate(root: ObjectRef) {
    let root = of_kind(root, ObjectKind::AddressSpace);
    assert!(
        crate::frame::owns(root, 4096),
        "vspace: {root:#x} is not allocated table memory"
    );
    // SAFETY: `root` is a live AddressSpace object, an allocated page whose
    // kernel half was copied from the kernel space at retype (`init_root`),
    // or the root the kernel's loader built; either way it maps the kernel
    // image, every kernel stack and the HHDM, so the code running now stays
    // mapped across the switch.
    unsafe { crate::paging::write_cr3(root) }
}
