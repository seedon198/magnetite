//! ASIDs for the address spaces tasks create
//! ([ADR 0023](../../docs/adr/0023-user-threads-and-address-spaces.md)).
//!
//! Each live `AddressSpace` object holds one of 64 ASIDs. A mapping record in
//! a Frame or PageTable capability names its space by ASID and generation;
//! freeing an ASID bumps its generation, so every record naming the old space
//! goes stale at once, and a stale record is simply "not mapped".
//!
//! `ASIDS` is a leaf lock: taken under `CSPACES`, never held while taking
//! another.

use capability::{Capability, Mapping, ObjectKind, ObjectRef};
use hal::sync::SpinLock;

struct Asids {
    roots: [Option<u64>; Mapping::ASIDS],
    generations: [u32; Mapping::ASIDS],
}

static ASIDS: SpinLock<Asids> = SpinLock::new(Asids {
    roots: [None; Mapping::ASIDS],
    generations: [0; Mapping::ASIDS],
});

/// Whether an ASID is free for a new address space.
pub fn has_free() -> bool {
    ASIDS.lock().roots.iter().any(Option::is_none)
}

/// Give address space `root` an ASID; `None` if all are taken.
pub fn alloc(root: ObjectRef) -> Option<(u8, u32)> {
    let mut a = ASIDS.lock();
    let i = a.roots.iter().position(Option::is_none)?;
    a.roots[i] = Some(root.phys);
    Some((i as u8, a.generations[i]))
}

/// The ASID and generation of live address space `root`.
pub fn of(root: ObjectRef) -> Option<(u8, u32)> {
    let a = ASIDS.lock();
    let i = a.roots.iter().position(|r| *r == Some(root.phys))?;
    Some((i as u8, a.generations[i]))
}

/// Free address space `root`'s ASID, making every mapping record that names
/// it stale.
pub fn free(root: ObjectRef) {
    let mut a = ASIDS.lock();
    if let Some(i) = a.roots.iter().position(|r| *r == Some(root.phys)) {
        a.roots[i] = None;
        a.generations[i] = (a.generations[i] + 1) % Mapping::GENERATIONS;
    }
}

/// How many ASIDs are in use.
pub fn live() -> usize {
    ASIDS.lock().roots.iter().flatten().count()
}

/// The live mapping a Frame or PageTable capability records, and the space
/// it is in; `None` for any other kind, an unmapped capability, or a record
/// of a destroyed space.
pub fn mapping(cap: &Capability<ObjectRef>) -> Option<(Mapping, ObjectRef)> {
    if !matches!(cap.object.kind, ObjectKind::Frame | ObjectKind::PageTable) {
        return None;
    }
    let m = Mapping::unpack(cap.badge)?;
    let a = ASIDS.lock();
    let root = a.roots[usize::from(m.asid)]?;
    (a.generations[usize::from(m.asid)] == m.generation)
        .then_some((m, ObjectRef::new(ObjectKind::AddressSpace, root)))
}

/// Undo the live mapping `cap` records, if any: unmap a Frame, or uninstall
/// a PageTable. The capability is being deleted, or unmapped.
pub fn unmap(cap: &Capability<ObjectRef>, hhdm: u64) {
    let Some((m, root)) = mapping(cap) else {
        return;
    };
    match cap.object.kind {
        ObjectKind::Frame => {
            hal::vspace::unmap_frame(root, m.vaddr(), cap.object, hhdm);
        }
        _ => {
            hal::vspace::uninstall_table(root, m.vaddr(), m.level, cap.object, hhdm);
        }
    }
}
