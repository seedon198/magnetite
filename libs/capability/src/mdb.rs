//! The **mapping database**: the capability derivation tree (ADR 0020).
//!
//! Every occupied slot carries a [`Link`]: the previous and next entries of a
//! doubly linked list, and a `depth`. Each list is a pre-order walk of a
//! derivation tree, so an entry's descendants are the entries right after it
//! with a greater depth, and its children are the ones at exactly `depth + 1`.
//! A slot is named anywhere in the system by a [`SlotRef`], never by a pointer,
//! and this module reads and writes slots only through a [`SlotStore`], so it
//! stays `#![forbid(unsafe_code)]` and host-tested; the kernel binds the store
//! to its CNode registry.
//!
//! The operations keep the invariant:
//! - [`insert_root`] starts a new tree;
//! - [`insert_child`] derives an entry, placed right after its parent;
//! - [`delete`] removes one entry and lifts its descendants to its parent;
//! - [`revoke_step`] deletes one descendant of an entry, deepest and last
//!   first, so a revoke cut off between steps leaves a valid tree;
//! - [`move_entry`] moves an entry to another slot, keeping its place.
//!
//! **The last capability.** Capabilities to one object form one contiguous run
//! of the list, as long as every capability to it descends from one original
//! and its capabilities only ever derive copies of the same object. That holds
//! for every kind except Untyped, whose children are the objects retyped from
//! it. So [`Deleted::last`], a check of the two list neighbours, is exact for
//! every object except an Untyped region, whose memory is reclaimed by revoke
//! instead.

use crate::space::CapError;
use crate::{Capability, Rights};

/// The most deletions one revoke call makes before it returns
/// [`SysError::Again`](crate::SysError), bounding the work done with
/// interrupts off (ADR 0020).
pub const REVOKE_BUDGET: usize = 64;

/// A slot anywhere in the system: a CNode's registry index and the slot's
/// index within it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SlotRef {
    /// The CNode's index in the kernel's CNode registry.
    pub cnode: u16,
    /// The slot's index within that CNode.
    pub index: u16,
}

impl SlotRef {
    /// The slot `index` of registry CNode `cnode`.
    #[must_use]
    pub const fn new(cnode: u16, index: u16) -> Self {
        Self { cnode, index }
    }
}

/// An entry's place in its list: its neighbours and its depth in the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Link {
    /// The previous entry, or `None` at the head of a list.
    pub prev: Option<SlotRef>,
    /// The next entry, or `None` at the tail of a list.
    pub next: Option<SlotRef>,
    /// The number of ancestors the entry has; a tree's root is at 0.
    pub depth: u16,
}

/// An occupied slot: the capability and its place in the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry<Obj> {
    /// The capability the slot holds.
    pub cap: Capability<Obj>,
    /// Its place in the database.
    pub link: Link,
}

/// Where the database reads and writes slots.
pub trait SlotStore<Obj> {
    /// The entry at `at`, or `None` if the slot is empty.
    fn get(&self, at: SlotRef) -> Option<Entry<Obj>>;
    /// Fill the slot at `at` with `entry`, or empty it with `None`.
    fn set(&mut self, at: SlotRef, entry: Option<Entry<Obj>>);
}

/// Why a database operation failed. Nothing changes when one does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MdbError {
    /// The slot named as the source is empty.
    Empty,
    /// The destination slot is already occupied.
    Occupied,
    /// The parent is already as deep as a tree can go.
    TooDeep,
}

/// An entry that [`delete`] or [`revoke_step`] removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deleted<Obj> {
    /// The capability the slot held.
    pub cap: Capability<Obj>,
    /// Neither list neighbour names the same object, so this was the last
    /// capability to it and the object can be destroyed. Exact for every
    /// object but an Untyped region (see the module docs).
    pub last: bool,
}

/// The entry at a slot a link points to. An empty slot there means the
/// database is corrupt, which no operation can recover from.
fn linked<Obj: Copy, S: SlotStore<Obj> + ?Sized>(store: &S, at: SlotRef) -> Entry<Obj> {
    store
        .get(at)
        .expect("capability database: a link names an empty slot")
}

fn set_next<Obj: Copy, S: SlotStore<Obj> + ?Sized>(
    store: &mut S,
    at: SlotRef,
    next: Option<SlotRef>,
) {
    let mut e = linked(store, at);
    e.link.next = next;
    store.set(at, Some(e));
}

fn set_prev<Obj: Copy, S: SlotStore<Obj> + ?Sized>(
    store: &mut S,
    at: SlotRef,
    prev: Option<SlotRef>,
) {
    let mut e = linked(store, at);
    e.link.prev = prev;
    store.set(at, Some(e));
}

/// Place `cap` in the empty slot `at` as the root of a tree of its own.
pub fn insert_root<Obj: Copy, S: SlotStore<Obj> + ?Sized>(
    store: &mut S,
    at: SlotRef,
    cap: Capability<Obj>,
) -> Result<(), MdbError> {
    if store.get(at).is_some() {
        return Err(MdbError::Occupied);
    }
    store.set(
        at,
        Some(Entry {
            cap,
            link: Link::default(),
        }),
    );
    Ok(())
}

/// Place `cap` in the empty slot `at` as a child of the entry at `parent`,
/// right after it in the list. The caller has already derived `cap` from the
/// parent's capability (attenuated, or retyped from an Untyped).
pub fn insert_child<Obj: Copy, S: SlotStore<Obj> + ?Sized>(
    store: &mut S,
    parent: SlotRef,
    at: SlotRef,
    cap: Capability<Obj>,
) -> Result<(), MdbError> {
    let p = store.get(parent).ok_or(MdbError::Empty)?;
    if store.get(at).is_some() {
        return Err(MdbError::Occupied);
    }
    let depth = p.link.depth.checked_add(1).ok_or(MdbError::TooDeep)?;
    if let Some(n) = p.link.next {
        set_prev(store, n, Some(at));
    }
    set_next(store, parent, Some(at));
    store.set(
        at,
        Some(Entry {
            cap,
            link: Link {
                prev: Some(parent),
                next: p.link.next,
                depth,
            },
        }),
    );
    Ok(())
}

/// Whether the neighbour at `at`, if any, names `object`.
fn names<Obj: Copy + PartialEq, S: SlotStore<Obj> + ?Sized>(
    store: &S,
    at: Option<SlotRef>,
    object: &Obj,
) -> bool {
    at.is_some_and(|r| linked(store, r).cap.object == *object)
}

/// Remove the entry at `at`. Its descendants stay valid and move up one level,
/// to its parent: deleting a capability does not take back what was derived
/// from it, only [`revoke_step`] does. Costs time in proportion to the
/// entry's subtree, for the depth fix-up.
pub fn delete<Obj: Copy + PartialEq, S: SlotStore<Obj> + ?Sized>(
    store: &mut S,
    at: SlotRef,
) -> Result<Deleted<Obj>, MdbError> {
    let e = store.get(at).ok_or(MdbError::Empty)?;
    let mut cur = e.link.next;
    while let Some(r) = cur {
        let mut d = linked(store, r);
        if d.link.depth <= e.link.depth {
            break;
        }
        d.link.depth -= 1;
        store.set(r, Some(d));
        cur = d.link.next;
    }
    if let Some(p) = e.link.prev {
        set_next(store, p, e.link.next);
    }
    if let Some(n) = e.link.next {
        set_prev(store, n, e.link.prev);
    }
    store.set(at, None);
    let last =
        !names(store, e.link.prev, &e.cap.object) && !names(store, e.link.next, &e.cap.object);
    Ok(Deleted { cap: e.cap, last })
}

/// The last descendant of the entry at `at`, which is a leaf, or `None` if
/// it has no descendants.
fn last_descendant<Obj: Copy, S: SlotStore<Obj> + ?Sized>(
    store: &S,
    at: SlotRef,
    depth: u16,
    first: Option<SlotRef>,
) -> Option<SlotRef> {
    let mut last = None;
    let mut cur = first;
    while let Some(r) = cur {
        let e = linked(store, r);
        if e.link.depth <= depth {
            break;
        }
        last = Some(r);
        cur = e.link.next;
    }
    debug_assert!(last != Some(at));
    last
}

/// Whether the entry at `at` has any descendants.
pub fn has_descendants<Obj: Copy, S: SlotStore<Obj> + ?Sized>(
    store: &S,
    at: SlotRef,
) -> Result<bool, MdbError> {
    let e = store.get(at).ok_or(MdbError::Empty)?;
    Ok(e.link
        .next
        .is_some_and(|n| linked(store, n).link.depth > e.link.depth))
}

/// One step of revoking the entry at `at`: delete its last descendant, which
/// is a leaf, and return where it was and what it held, or `None` once the
/// entry has no descendants left. The entry itself stays.
///
/// Each step is independent, so the caller may stop after any of them (after
/// [`REVOKE_BUDGET`] steps, say) and continue later from the same `at`; and
/// destroying a deleted object may change the database in between. A step
/// walks the entry's remaining subtree to find its end.
pub fn revoke_step<Obj: Copy + PartialEq, S: SlotStore<Obj> + ?Sized>(
    store: &mut S,
    at: SlotRef,
) -> Result<Option<(SlotRef, Deleted<Obj>)>, MdbError> {
    let e = store.get(at).ok_or(MdbError::Empty)?;
    match last_descendant(store, at, e.link.depth, e.link.next) {
        Some(leaf) => Ok(Some((leaf, delete(store, leaf)?))),
        None => Ok(None),
    }
}

/// Move the entry at `from` into the empty slot `to`. Its place in the tree
/// is unchanged.
pub fn move_entry<Obj: Copy, S: SlotStore<Obj> + ?Sized>(
    store: &mut S,
    from: SlotRef,
    to: SlotRef,
) -> Result<(), MdbError> {
    let e = store.get(from).ok_or(MdbError::Empty)?;
    if store.get(to).is_some() {
        return Err(MdbError::Occupied);
    }
    store.set(from, None);
    store.set(to, Some(e));
    if let Some(p) = e.link.prev {
        set_next(store, p, Some(to));
    }
    if let Some(n) = e.link.next {
        set_prev(store, n, Some(to));
    }
    Ok(())
}

/// Copy the capability at `src` into the empty slot `dst` as its child,
/// **attenuated** to `mask` rights.
///
/// # Errors
/// [`CapError::NotPermitted`] if `mask` is not a subset of the source's
/// rights (a copy can only weaken), or the source's tree is too deep;
/// [`CapError::Empty`] or [`CapError::Occupied`] for the slots.
pub fn copy<Obj: Copy, S: SlotStore<Obj> + ?Sized>(
    store: &mut S,
    src: SlotRef,
    dst: SlotRef,
    mask: Rights,
) -> Result<(), CapError> {
    let from = store.get(src).ok_or(CapError::Empty)?;
    let derived = from.cap.derive(mask).ok_or(CapError::NotPermitted)?;
    Ok(insert_child(store, src, dst, derived)?)
}

/// Like [`copy`], but also set a `badge` on the new capability (mint).
///
/// # Errors
/// As for [`copy`], and [`CapError::NotPermitted`] if the badge rule is
/// violated (re-badging a badged capability).
pub fn mint<Obj: Copy, S: SlotStore<Obj> + ?Sized>(
    store: &mut S,
    src: SlotRef,
    dst: SlotRef,
    mask: Rights,
    badge: u64,
) -> Result<(), CapError> {
    let from = store.get(src).ok_or(CapError::Empty)?;
    let minted = from.cap.mint(mask, badge).ok_or(CapError::NotPermitted)?;
    Ok(insert_child(store, src, dst, minted)?)
}

impl From<MdbError> for CapError {
    fn from(e: MdbError) -> Self {
        match e {
            MdbError::Empty => CapError::Empty,
            MdbError::Occupied => CapError::Occupied,
            MdbError::TooDeep => CapError::NotPermitted,
        }
    }
}

#[cfg(test)]
mod tests;
