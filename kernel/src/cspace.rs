//! Capability spaces and the **CNode registry**
//! ([ADR 0020](../../docs/adr/0020-capability-derivation-and-revoke.md)).
//!
//! Every live CNode is registered here, with the capability space formed over
//! its storage, and a thread's capability space is the CNode it is attached
//! to. A slot anywhere is a [`SlotRef`]: a registry index and a slot index.
//! The registry is the [`SlotStore`] the pure mapping database
//! (`capability::mdb`) runs over, so every copy, mint, move, delete and IPC
//! transfer keeps the derivation tree consistent across all CNodes.
//!
//! One lock, `CSPACES`, guards it all. It nests after `IPC` and before
//! `SCHED` (the lock order in `hal::sync`). A delete or revoke may destroy
//! an object and wake threads blocked on it, so it runs under both
//! (`ipc::with_caps`); reads take `CSPACES` alone.

use ::ipc::Ipc;
use capability::mdb::{self, Deleted, Entry, REVOKE_BUDGET};
use capability::{
    CapError, CapId, CapSpace, Capability, ObjectKind, ObjectRef, Rights, SlotRef, SlotStore,
};
use hal::object::CNodeSpace;
use hal::sync::SpinLock;
use sched::ThreadId;

/// The most CNodes alive at once.
pub const MAX_CNODES: usize = 32;

/// A registered CNode: the object, and the capability space over its slots.
struct Node {
    object: ObjectRef,
    space: CNodeSpace,
}

/// Every live CNode, and the CNode each thread's capability space is.
pub struct Registry {
    cnodes: [Option<Node>; MAX_CNODES],
    of: [Option<u16>; ::ipc::MAX_THREADS],
}

/// The registry. A CNode's space can be formed only once while it lives
/// (`hal::object::CNodeSpace`), so it is registered at most once.
pub static CSPACES: SpinLock<Registry> = SpinLock::new(Registry {
    cnodes: [const { None }; MAX_CNODES],
    of: [None; ::ipc::MAX_THREADS],
});

impl Registry {
    /// Register CNode `object`, with the capability space formed over it, and
    /// return its registry index; `None` if the registry is full.
    pub fn register(&mut self, object: ObjectRef, space: CNodeSpace) -> Option<u16> {
        assert!(
            space.capacity() <= usize::from(u16::MAX) + 1,
            "cspace: a CNode too large to name its slots"
        );
        let i = self.cnodes.iter().position(Option::is_none)?;
        self.cnodes[i] = Some(Node { object, space });
        Some(i as u16)
    }

    /// Make CNode `c` thread `t`'s capability space.
    pub fn attach(&mut self, t: ThreadId, c: u16) {
        assert!(
            self.cnodes[usize::from(c)].is_some(),
            "cspace: no such CNode"
        );
        self.of[t.0] = Some(c);
    }

    /// Thread `t`'s CNode, if it has a capability space.
    #[must_use]
    pub fn of(&self, t: ThreadId) -> Option<u16> {
        self.of[t.0]
    }

    fn node(&self, c: u16) -> &Node {
        self.cnodes[usize::from(c)]
            .as_ref()
            .expect("cspace: a reference to an unregistered CNode")
    }

    fn node_mut(&mut self, c: u16) -> &mut Node {
        self.cnodes[usize::from(c)]
            .as_mut()
            .expect("cspace: a reference to an unregistered CNode")
    }

    /// The CNode object registered at `c`.
    #[must_use]
    pub fn object(&self, c: u16) -> ObjectRef {
        self.node(c).object
    }

    /// The capability space of CNode `c`, to read.
    #[must_use]
    pub fn space(&self, c: u16) -> &CapSpace<'static, ObjectRef> {
        &self.node(c).space
    }

    /// Slot `id` of CNode `c`.
    ///
    /// # Errors
    /// [`CapError::OutOfRange`] if the CNode has no such slot.
    pub fn slot_in(&self, c: u16, id: CapId) -> Result<SlotRef, CapError> {
        if self.space(c).contains(id) {
            Ok(SlotRef::new(c, id.0 as u16))
        } else {
            Err(CapError::OutOfRange)
        }
    }

    /// Slot `id` of thread `t`'s capability space.
    ///
    /// # Errors
    /// [`CapError::OutOfRange`] if `t` has no capability space or it has no
    /// such slot.
    pub fn slot(&self, t: ThreadId, id: CapId) -> Result<SlotRef, CapError> {
        self.slot_in(self.of(t).ok_or(CapError::OutOfRange)?, id)
    }

    /// The whole entry at `at`: the capability and its place in the
    /// derivation tree. `None` if the slot is empty.
    #[must_use]
    pub fn entry(&self, at: SlotRef) -> Option<Entry<ObjectRef>> {
        self.get(at)
    }

    /// The capability at `at`.
    ///
    /// # Errors
    /// [`CapError::Empty`] if the slot is empty.
    pub fn cap(&self, at: SlotRef) -> Result<Capability<ObjectRef>, CapError> {
        self.get(at).map(|e| e.cap).ok_or(CapError::Empty)
    }

    /// The capability in slot `id` of thread `t`'s capability space.
    ///
    /// # Errors
    /// As for [`slot`](Self::slot) and [`cap`](Self::cap).
    pub fn lookup(&self, t: ThreadId, id: CapId) -> Result<Capability<ObjectRef>, CapError> {
        self.cap(self.slot(t, id)?)
    }

    /// The registry index of CNode `object`, if it is registered.
    #[must_use]
    pub fn index_of(&self, object: ObjectRef) -> Option<u16> {
        self.cnodes
            .iter()
            .position(|n| n.as_ref().is_some_and(|n| n.object == object))
            .map(|i| i as u16)
    }

    /// Whether every registry entry is taken.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.cnodes.iter().all(Option::is_some)
    }

    /// Unregister the emptied CNode `c` and detach every thread from it.
    fn unregister(&mut self, c: u16) {
        debug_assert!(self.space(c).iter().next().is_none());
        self.cnodes[usize::from(c)] = None;
        for of in &mut self.of {
            if *of == Some(c) {
                *of = None;
            }
        }
    }

    /// The first capability in any registered CNode naming an object stored
    /// in physical `[start, end)`. A registered CNode stored there counts
    /// too, as its slot 0. Memory may be freed only while this is `None`
    /// (#105).
    #[must_use]
    pub fn find_reference(&self, start: u64, end: u64) -> Option<SlotRef> {
        let inside = |o: ObjectRef| (start..end).contains(&o.phys);
        self.cnodes.iter().enumerate().find_map(|(c, node)| {
            let node = node.as_ref()?;
            let c = c as u16;
            if inside(node.object) {
                return Some(SlotRef::new(c, 0));
            }
            node.space
                .iter()
                .find(|(_, cap)| inside(cap.object))
                .map(|(id, _)| SlotRef::new(c, id.0 as u16))
        })
    }
}

impl SlotStore<ObjectRef> for Registry {
    /// A slot of an unregistered CNode reads as empty: a destroyed CNode
    /// leaves nothing behind, and no link ever names it.
    fn get(&self, at: SlotRef) -> Option<Entry<ObjectRef>> {
        self.cnodes[usize::from(at.cnode)]
            .as_ref()?
            .space
            .entry(CapId(u32::from(at.index)))
    }

    fn set(&mut self, at: SlotRef, entry: Option<Entry<ObjectRef>>) {
        self.node_mut(at.cnode)
            .space
            .set_entry(CapId(u32::from(at.index)), entry);
    }
}

/// Run `f` on the registry, with interrupts off and `CSPACES` held.
pub fn with<R>(f: impl FnOnce(&mut Registry) -> R) -> R {
    hal::cpu::without_interrupts(|| f(&mut CSPACES.lock()))
}

/// Register CNode `cnode`, with the capability space formed over it, as
/// thread `t`'s capability space. Returns its registry index.
///
/// # Panics
/// If the registry is full.
pub fn install(t: ThreadId, cnode: ObjectRef, space: CNodeSpace) -> u16 {
    with(|r| {
        let c = r
            .register(cnode, space)
            .expect("cspace: the CNode registry is full");
        r.attach(t, c);
        c
    })
}

/// Run `f` on thread `t`'s capability space and its CNode object, to read,
/// with interrupts off and `CSPACES` held. `None` if `t` has none.
pub fn with_space<R>(
    t: ThreadId,
    f: impl FnOnce(&CapSpace<'static, ObjectRef>, ObjectRef) -> R,
) -> Option<R> {
    with(|r| {
        let c = r.of(t)?;
        Some(f(r.space(c), r.object(c)))
    })
}

/// Place a kernel-made capability in slot `id` of thread `t`'s capability
/// space, as the root of a derivation tree of its own.
///
/// # Errors
/// As for [`Registry::slot`], and [`CapError::Occupied`].
pub fn insert_root(
    t: ThreadId,
    id: CapId,
    cap: Capability<ObjectRef>,
) -> Result<SlotRef, CapError> {
    with(|r| {
        let at = r.slot(t, id)?;
        mdb::insert_root(r, at, cap)?;
        Ok(at)
    })
}

/// CNodes whose last capability is gone, waiting to be emptied.
struct Dying {
    cnodes: [u16; MAX_CNODES],
    len: usize,
}

impl Dying {
    const fn new() -> Self {
        Self {
            cnodes: [0; MAX_CNODES],
            len: 0,
        }
    }

    /// Settle a deleted capability: undo the mapping a Frame or PageTable
    /// capability records, then destroy what it was the last capability to.
    /// A CNode is queued to be emptied; an AddressSpace frees its ASID;
    /// anything else goes to `ipc::destroy`.
    fn settle(&mut self, ipc: &mut Ipc, r: &Registry, d: &Deleted<ObjectRef>) {
        crate::vspace::unmap(&d.cap, crate::ipc::hhdm());
        if d.last && d.cap.object.kind == ObjectKind::AddressSpace {
            crate::vspace::free(d.cap.object);
            return;
        }
        if d.last && d.cap.object.kind == ObjectKind::Thread {
            crate::thread::unbind_object(d.cap.object);
            return;
        }
        if !d.last || d.cap.object.kind != ObjectKind::CNode {
            crate::ipc::destroy(ipc, d);
            return;
        }
        if let Some(c) = r.index_of(d.cap.object) {
            // Each CNode's last capability goes once, so this never fills.
            if !self.cnodes[..self.len].contains(&c) {
                self.cnodes[self.len] = c;
                self.len += 1;
            }
        }
    }

    /// Empty and unregister every queued CNode, and any CNode that dies on
    /// the way. A loop, not recursion: CNodes held in CNodes cannot exhaust
    /// the kernel stack.
    fn drain(&mut self, ipc: &mut Ipc, r: &mut Registry) {
        while self.len > 0 {
            self.len -= 1;
            let c = self.cnodes[self.len];
            self.empty(ipc, r, c);
        }
    }

    /// Delete every capability in CNode `c`, settling each, then unregister
    /// it. A CNode already gone is skipped: one holding the last capability
    /// to itself is queued while it is being emptied.
    fn empty(&mut self, ipc: &mut Ipc, r: &mut Registry, c: u16) {
        if r.cnodes[usize::from(c)].is_none() {
            return;
        }
        for i in 0..r.space(c).capacity() {
            let at = SlotRef::new(c, i as u16);
            if r.get(at).is_some() {
                let d = mdb::delete(r, at).expect("cspace: an occupied slot");
                self.settle(ipc, r, &d);
            }
        }
        r.unregister(c);
    }
}

/// Destroy what `d` was the last capability to (ADR 0020): an Endpoint's or
/// Notification's waiters wake with `Revoked`, and a CNode is emptied, each
/// of its capabilities deleted and destroyed in turn, and leaves the
/// registry. Call with `IPC` and `CSPACES` held.
fn destroy(ipc: &mut Ipc, r: &mut Registry, d: &Deleted<ObjectRef>) {
    let mut dying = Dying::new();
    dying.settle(ipc, r, d);
    dying.drain(ipc, r);
}

/// Copy the capability at `src` into the empty `dst` as its child, with
/// `rights` (a subset). A PageTable capability cannot be copied, so each
/// table is installed in at most one place, and a copy of a Frame capability
/// starts unmapped (ADR 0023).
///
/// # Errors
/// As for `mdb::copy`, and [`CapError::NotPermitted`] for a PageTable.
pub fn copy(r: &mut Registry, src: SlotRef, dst: SlotRef, rights: Rights) -> Result<(), CapError> {
    let kind = r.cap(src)?.object.kind;
    if kind == ObjectKind::PageTable {
        return Err(CapError::NotPermitted);
    }
    mdb::copy(r, src, dst, rights)?;
    if kind == ObjectKind::Frame {
        let mut e = r.get(dst).expect("cspace: the copy just made");
        e.cap.badge = 0;
        r.set(dst, Some(e));
    }
    Ok(())
}

/// Mint the capability at `src` into the empty `dst`: a copy with `rights`
/// and `badge`. Frame and PageTable capabilities keep their mapping record
/// in the badge word, so they cannot be minted (ADR 0023).
///
/// # Errors
/// As for `mdb::mint`, and [`CapError::NotPermitted`] for those kinds.
pub fn mint(
    r: &mut Registry,
    src: SlotRef,
    dst: SlotRef,
    rights: Rights,
    badge: u64,
) -> Result<(), CapError> {
    let kind = r.cap(src)?.object.kind;
    if matches!(kind, ObjectKind::Frame | ObjectKind::PageTable) {
        return Err(CapError::NotPermitted);
    }
    mdb::mint(r, src, dst, rights, badge)
}

/// Delete the capability at `at`, destroying its object if it was the last
/// capability to it (ADR 0020). Call with `IPC` and `CSPACES` held.
///
/// # Errors
/// [`CapError::Empty`] if the slot is empty.
pub fn delete(ipc: &mut Ipc, r: &mut Registry, at: SlotRef) -> Result<(), CapError> {
    let d = mdb::delete(r, at)?;
    destroy(ipc, r, &d);
    Ok(())
}

/// Empty CNode `c` and unregister it: delete every capability in it,
/// destroying what each was the last capability to. Call with `IPC` and
/// `CSPACES` held.
pub fn clear(ipc: &mut Ipc, r: &mut Registry, c: u16) {
    let mut dying = Dying::new();
    dying.empty(ipc, r, c);
    dying.drain(ipc, r);
}

/// Revoke the capability at `at`: delete its descendants, deepest and last
/// first, destroying each object whose last capability goes, for at most
/// [`REVOKE_BUDGET`] deletions. Returns whether the revoke finished; if not,
/// call again to continue (ADR 0020). Call with `IPC` and `CSPACES` held.
///
/// A finished revoke of an Untyped region's original capability leaves
/// nothing retyped from the region alive, so its watermark goes back to the
/// base and the whole region can be retyped again.
///
/// The revoked capability's own CNode may be destroyed on the way, if the
/// last capability to it was among the descendants; the revoke is then
/// finished, since nothing is left of the slot.
///
/// # Errors
/// [`CapError::Empty`] if the slot is empty.
pub fn revoke(ipc: &mut Ipc, r: &mut Registry, at: SlotRef) -> Result<bool, CapError> {
    r.cap(at)?;
    for _ in 0..REVOKE_BUDGET {
        if r.get(at).is_none() {
            return Ok(true);
        }
        let Some((_, d)) = mdb::revoke_step(r, at)? else {
            break;
        };
        destroy(ipc, r, &d);
    }
    if r.get(at).is_none() {
        return Ok(true);
    }
    if mdb::has_descendants(r, at)? {
        return Ok(false);
    }
    let obj = r.cap(at)?.object;
    if obj.kind == ObjectKind::Untyped && mdb::is_original(r, at)? {
        let hhdm = crate::ipc::hhdm();
        let mut ut = hal::object::load_untyped(obj, hhdm);
        ut.reset();
        hal::object::store_untyped(obj, hhdm, ut);
    }
    Ok(true)
}

/// Unregister CNode `c`, deleting every capability in it and destroying
/// what they were the last capabilities to.
pub fn remove(c: u16) {
    crate::ipc::with_caps(|ipc, r| clear(ipc, r, c));
}

/// Unregister thread `t`'s CNode, deleting every capability in it, when the
/// thread is reaped.
pub fn release(t: ThreadId) {
    if let Some(c) = with(|r| r.of(t)) {
        remove(c);
    }
}
