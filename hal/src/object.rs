//! Retyping Untyped memory into object storage.
//!
//! The pure accounting lives in [`capability::object::Untyped`]; this is the
//! HAL half that actually touches the carved physical memory: it zeroes the
//! storage through the HHDM so a freshly retyped object never exposes whatever
//! previously occupied that memory (important when an Untyped region is revoked
//! and reused). See [ADR 0016](../../docs/adr/0016-capability-implementation.md).

use core::ops::{Deref, DerefMut};

use capability::object::{EndpointState, ObjectKind, ObjectRef, Untyped};
use capability::space::{CapSpace, Slot};

use crate::sync::SpinLock;

/// Panic unless `[phys, phys + len)` lies in allocated pool frames. Every
/// access below writes or reads through `hhdm + phys`, so this keeps safe
/// kernel code from aiming one at memory nobody allocated: the kernel image,
/// the frame bitmap, MMIO (#106). The accessors also take an [`ObjectRef`]
/// and check its kind, so one kind of object is never used as another.
fn check(phys: u64, len: usize) {
    assert!(
        crate::frame::owns(phys, len as u64),
        "object: {phys:#x}+{len:#x} is not allocated object memory"
    );
}

/// Carve `count` objects of `size` bytes each (aligned to `align`) from
/// `untyped`, **zero** their storage, and return the physical base of the
/// first. Returns `None` if the request is malformed or does not fit; on
/// failure nothing is carved or zeroed.
///
/// `hhdm` is the Higher-Half Direct Map offset, through which the carved
/// physical range is reached.
pub fn retype(untyped: &mut Untyped, size: u64, align: u64, count: u64, hhdm: u64) -> Option<u64> {
    let base = untyped.allocate(size, align, count)?;
    let len = (size * count) as usize;
    check(base, len);
    let ptr = (hhdm + base) as *mut u8;
    // SAFETY: `base .. base + size*count` was just carved from `untyped` by
    // `allocate` (which checked it lies within the region), so it is backing
    // RAM mapped in the HHDM at `hhdm` and now owned exclusively by the objects
    // being created. Zeroing that range stays within it.
    unsafe {
        core::ptr::write_bytes(ptr, 0, len);
    }
    Some(base)
}

/// The most capability spaces alive at once.
const LIVE_CNODES: usize = 64;

/// The storage, `[start, end)` physical, of every live [`CNodeSpace`]. A
/// leaf lock: taken under `CSPACES` when a space is dropped, never held
/// while taking another.
static LIVE: SpinLock<[Option<(u64, u64)>; LIVE_CNODES]> = SpinLock::new([None; LIVE_CNODES]);

/// Whether a live [`CNodeSpace`] covers any of physical `[start, end)`.
/// Freeing a frame under one would leave the space pointing at memory a new
/// owner gets, so [`free_frame`](crate::frame::free_frame) refuses to.
#[must_use]
pub fn cnode_live_in(start: u64, end: u64) -> bool {
    LIVE.lock()
        .iter()
        .flatten()
        .any(|&(s, e)| s < end && start < e)
}

/// A capability space over a CNode's storage, and the only one: forming a
/// second over the same storage panics, and so does freeing the storage
/// while this lives. Dereferences to the [`CapSpace`].
pub struct CNodeSpace {
    space: CapSpace<'static, ObjectRef>,
    start: u64,
}

impl Deref for CNodeSpace {
    type Target = CapSpace<'static, ObjectRef>;

    fn deref(&self) -> &Self::Target {
        &self.space
    }
}

impl DerefMut for CNodeSpace {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.space
    }
}

impl Drop for CNodeSpace {
    fn drop(&mut self) {
        let mut live = LIVE.lock();
        let entry = live
            .iter_mut()
            .find(|e| e.is_some_and(|(s, _)| s == self.start))
            .expect("object: a live CNode space is recorded");
        *entry = None;
    }
}

/// Form the capability space over CNode `cnode`'s retyped storage: a `Slot`
/// array of `slots` entries, reached through the HHDM.
///
/// Each slot is initialized to empty (overwriting the retyped zeros with a
/// valid `Slot::empty()`, so no possibly-invalid bytes are ever read), then the
/// slice is handed back inside a [`CNodeSpace`].
///
/// # Panics
/// If `cnode` is not a CNode, its storage is not allocated pool memory, or a
/// live [`CNodeSpace`] already covers any of it: two spaces over one CNode
/// would alias (#106).
#[must_use]
pub fn cnode_cap_space(cnode: ObjectRef, slots: usize, hhdm: u64) -> CNodeSpace {
    assert_eq!(cnode.kind, ObjectKind::CNode, "object: not a CNode");
    let phys = cnode.phys;
    let len = slots * core::mem::size_of::<Slot<ObjectRef>>();
    check(phys, len);
    let end = phys + len as u64;
    {
        let mut live = LIVE.lock();
        assert!(
            !live.iter().flatten().any(|&(s, e)| s < end && phys < e),
            "object: a second capability space over CNode {phys:#x}"
        );
        let free = live
            .iter_mut()
            .find(|e| e.is_none())
            .expect("object: too many live CNode spaces");
        *free = Some((phys, end));
    }
    let ptr = (hhdm + phys) as *mut Slot<ObjectRef>;
    // SAFETY: `phys .. end` is allocated pool RAM (checked above), mapped in
    // the HHDM at `hhdm`, and no other live `CNodeSpace` covers any of it
    // (recorded in `LIVE` until this one drops); and `free_frame` refuses to
    // free it while this lives, so the `'static` slice never outlives the
    // memory. We `write` (not read) `Slot::empty()` into each entry first, so
    // the slice then references only initialized, valid `Slot`s.
    let space = unsafe {
        for i in 0..slots {
            ptr.add(i).write(Slot::empty());
        }
        CapSpace::new(core::slice::from_raw_parts_mut(ptr, slots))
    };
    CNodeSpace { space, start: phys }
}

/// Panic unless `obj` is of `kind`, and return its storage's address.
fn of_kind(obj: ObjectRef, kind: ObjectKind) -> u64 {
    assert_eq!(
        obj.kind, kind,
        "object: {:#x} used as the wrong kind",
        obj.phys
    );
    obj.phys
}

/// Load the [`Untyped`] control block of Untyped object `ut` (the storage an
/// Untyped capability points at), reached via the HHDM.
///
/// The control block must have been written by [`store_untyped`] first.
#[must_use]
pub fn load_untyped(ut: ObjectRef, hhdm: u64) -> Untyped {
    let phys = of_kind(ut, ObjectKind::Untyped);
    check(phys, core::mem::size_of::<Untyped>());
    let ptr = (hhdm + phys) as *const Untyped;
    // SAFETY: `phys` is an Untyped control block in allocated, HHDM-mapped
    // RAM, aligned for `Untyped`; `Untyped` is `repr(C)` and `Copy`, and any
    // bit pattern of its three `u64`s is a value, so this read is sound.
    unsafe { ptr.read() }
}

/// Store the [`Untyped`] control block of Untyped object `ut`, via the HHDM.
pub fn store_untyped(ut: ObjectRef, hhdm: u64, untyped: Untyped) {
    let phys = of_kind(ut, ObjectKind::Untyped);
    check(phys, core::mem::size_of::<Untyped>());
    let ptr = (hhdm + phys) as *mut Untyped;
    // SAFETY: as in `load_untyped`: aligned, allocated storage for one
    // `repr(C)` `Untyped` at `phys`.
    unsafe { ptr.write(untyped) }
}

/// Load the [`EndpointState`] of Endpoint `ep`, via the HHDM.
#[must_use]
pub fn load_endpoint(ep: ObjectRef, hhdm: u64) -> EndpointState {
    let phys = of_kind(ep, ObjectKind::Endpoint);
    check(phys, core::mem::size_of::<EndpointState>());
    let ptr = (hhdm + phys) as *const EndpointState;
    // SAFETY: `phys` is an Endpoint's retyped storage in allocated,
    // HHDM-mapped RAM, sized and aligned for `EndpointState` (its fixed
    // layout); the type is `repr(C)`, `Copy`, and valid for any bit pattern
    // of its four `u64`s, so this read is sound.
    unsafe { ptr.read() }
}

/// Store the [`EndpointState`] of Endpoint `ep`, via the HHDM.
pub fn store_endpoint(ep: ObjectRef, hhdm: u64, state: EndpointState) {
    let phys = of_kind(ep, ObjectKind::Endpoint);
    check(phys, core::mem::size_of::<EndpointState>());
    let ptr = (hhdm + phys) as *mut EndpointState;
    // SAFETY: as in `load_endpoint`: aligned, allocated,
    // `EndpointState`-sized storage at `phys`.
    unsafe { ptr.write(state) }
}

/// OR `bits` into the signal word of Notification `n` (the first `u64` of
/// its `NotificationState`), via the HHDM.
pub fn notification_signal(n: ObjectRef, bits: u64, hhdm: u64) {
    let phys = of_kind(n, ObjectKind::Notification);
    check(phys, 8);
    let ptr = (hhdm + phys) as *mut u64;
    // SAFETY: `phys` is a Notification's retyped, zeroed state (its first field
    // is the `u64` signal word) in our HHDM-mapped RAM; read-modify-write of
    // that word stays within the object.
    unsafe {
        let cur = ptr.read_volatile();
        ptr.write_volatile(cur | bits);
    }
}

/// Read the pending signal word of Notification `n`, via the HHDM.
#[must_use]
pub fn notification_pending(n: ObjectRef, hhdm: u64) -> u64 {
    let phys = of_kind(n, ObjectKind::Notification);
    check(phys, 8);
    let ptr = (hhdm + phys) as *const u64;
    // SAFETY: `phys` is a Notification's retyped state in HHDM-mapped RAM; its
    // first field is the `u64` signal word, valid to read.
    unsafe { ptr.read_volatile() }
}

/// Copy `bytes` into physical memory at `phys`, reached via the HHDM. Used to
/// load code or data into a freshly allocated frame.
///
/// The caller must own `[phys, phys + bytes.len())` (e.g. a frame just
/// allocated) and keep `bytes.len()` within it.
pub fn write_phys(phys: u64, hhdm: u64, bytes: &[u8]) {
    check(phys, bytes.len());
    let dst = (hhdm + phys) as *mut u8;
    // SAFETY: per the contract, `[phys, phys+len)` is our HHDM-mapped RAM; the
    // source and destination do not overlap (source is a kernel buffer).
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
    }
}
