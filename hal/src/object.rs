//! Retyping Untyped memory into object storage.
//!
//! The pure accounting lives in [`capability::object::Untyped`]; this is the
//! HAL half that actually touches the carved physical memory: it zeroes the
//! storage through the HHDM so a freshly retyped object never exposes whatever
//! previously occupied that memory (important when an Untyped region is revoked
//! and reused). See [ADR 0016](../../docs/adr/0016-capability-implementation.md).

use capability::object::{EndpointState, ObjectRef, Untyped};
use capability::space::{CapSpace, Slot};

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

/// Form a capability space over a CNode's retyped storage: a `Slot` array at
/// physical `phys` with `slots` entries, reached through the HHDM.
///
/// Each slot is initialized to empty (overwriting the retyped zeros with a
/// valid `Slot::empty()`, so no possibly-invalid bytes are ever read), then the
/// slice is handed back as a [`CapSpace`].
///
/// The caller must pass a `phys` that is a CNode just retyped for `slots` slots
/// (so `phys .. phys + slots * size_of::<Slot<ObjectRef>>()` is our RAM, mapped
/// in the HHDM at `hhdm` and owned exclusively by this CNode) and must not form
/// a second `CapSpace` aliasing the same storage while this one lives. (A safe
/// wrapper over internal `unsafe`, like the other HHDM-backed HAL constructors.)
#[must_use]
pub fn cnode_cap_space(phys: u64, slots: usize, hhdm: u64) -> CapSpace<'static, ObjectRef> {
    let ptr = (hhdm + phys) as *mut Slot<ObjectRef>;
    // SAFETY: per the caller contract above, `ptr .. ptr + slots` is our
    // exclusively-owned, HHDM-mapped CNode storage. We `write` (not read)
    // `Slot::empty()` into each entry first, so the slice then references only
    // initialized, valid `Slot`s and aliases nothing.
    unsafe {
        for i in 0..slots {
            ptr.add(i).write(Slot::empty());
        }
        CapSpace::new(core::slice::from_raw_parts_mut(ptr, slots))
    }
}

/// Load the [`Untyped`] control block stored at physical `phys` (the storage an
/// Untyped capability points at), reached via the HHDM.
///
/// The caller must pass a `phys` that holds a valid `Untyped` (written by
/// [`store_untyped`]); safe wrapper over internal `unsafe`.
#[must_use]
pub fn load_untyped(phys: u64, hhdm: u64) -> Untyped {
    let ptr = (hhdm + phys) as *const Untyped;
    // SAFETY: per the contract, `phys` holds a valid, aligned `Untyped` in our
    // HHDM-mapped RAM; `Untyped` is `repr(C)` and `Copy`, so this read is sound.
    unsafe { ptr.read() }
}

/// Store an [`Untyped`] control block at physical `phys`, via the HHDM.
///
/// The caller must own the storage at `phys` (a control block sized for an
/// `Untyped`); safe wrapper over internal `unsafe`.
pub fn store_untyped(phys: u64, hhdm: u64, untyped: Untyped) {
    let ptr = (hhdm + phys) as *mut Untyped;
    // SAFETY: as in `load_untyped`; the caller owns aligned storage for one
    // `repr(C)` `Untyped` at `phys`.
    unsafe { ptr.write(untyped) }
}

/// Load the [`EndpointState`] of the Endpoint whose storage is at physical
/// `phys`, via the HHDM.
///
/// The caller must pass a `phys` that is a retyped Endpoint's storage (zeroed
/// at retype, then written only by [`store_endpoint`]); safe wrapper over
/// internal `unsafe`.
#[must_use]
pub fn load_endpoint(phys: u64, hhdm: u64) -> EndpointState {
    let ptr = (hhdm + phys) as *const EndpointState;
    // SAFETY: per the contract, `phys` is an Endpoint's retyped storage in our
    // HHDM-mapped RAM, sized and aligned for `EndpointState` (its fixed
    // layout); the type is `repr(C)`, `Copy`, and valid for any bit pattern
    // of its four `u64`s, so this read is sound.
    unsafe { ptr.read() }
}

/// Store an Endpoint's [`EndpointState`] at physical `phys`, via the HHDM.
///
/// The caller must pass a retyped Endpoint's storage, as for
/// [`load_endpoint`]; safe wrapper over internal `unsafe`.
pub fn store_endpoint(phys: u64, hhdm: u64, state: EndpointState) {
    let ptr = (hhdm + phys) as *mut EndpointState;
    // SAFETY: as in `load_endpoint`; the caller owns that aligned,
    // `EndpointState`-sized storage at `phys`.
    unsafe { ptr.write(state) }
}

/// OR `bits` into the signal word of the Notification whose state lives at
/// physical `phys` (the first `u64` of its `NotificationState`), via the HHDM.
pub fn notification_signal(phys: u64, bits: u64, hhdm: u64) {
    let ptr = (hhdm + phys) as *mut u64;
    // SAFETY: `phys` is a Notification's retyped, zeroed state (its first field
    // is the `u64` signal word) in our HHDM-mapped RAM; read-modify-write of
    // that word stays within the object.
    unsafe {
        let cur = ptr.read_volatile();
        ptr.write_volatile(cur | bits);
    }
}

/// Read the pending signal word of the Notification whose state lives at
/// physical `phys`, via the HHDM.
#[must_use]
pub fn notification_pending(phys: u64, hhdm: u64) -> u64 {
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
    let dst = (hhdm + phys) as *mut u8;
    // SAFETY: per the contract, `[phys, phys+len)` is our HHDM-mapped RAM; the
    // source and destination do not overlap (source is a kernel buffer).
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
    }
}
