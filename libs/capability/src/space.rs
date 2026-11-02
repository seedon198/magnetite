//! Per-task **capability space**: a flat table of capability slots.
//!
//! A [`CapId`] is an index into the table; lookup is O(1). The table is plain
//! storage over caller-provided slots (as the frame allocator does with its
//! bitmap), so this stays `#![forbid(unsafe_code)]` and host-testable; the
//! kernel allocates the backing slots from Untyped memory (ADR 0016).
//!
//! Every slot also carries its [`Link`] in the capability derivation tree
//! (ADR 0020), and that tree spans every capability space. So a table only
//! reads and writes whole entries: copying, minting, moving and deleting a
//! capability go through [`crate::mdb`], which keeps the links consistent.

use crate::mdb::{Entry, Link};
use crate::Capability;

/// An index naming a capability slot within a [`CapSpace`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CapId(pub u32);

/// One capability slot: empty, or holding a [`Capability`] and its place in
/// the derivation tree.
#[derive(Debug, Clone, Copy)]
pub struct Slot<Obj> {
    cap: Option<Capability<Obj>>,
    link: Link,
}

impl<Obj> Slot<Obj> {
    /// An empty slot.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            cap: None,
            link: Link {
                prev: None,
                next: None,
                depth: 0,
            },
        }
    }
}

impl<Obj> Default for Slot<Obj> {
    fn default() -> Self {
        Self::empty()
    }
}

/// Why a capability-space operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapError {
    /// The [`CapId`] is outside the table.
    OutOfRange,
    /// The source slot is empty.
    Empty,
    /// The destination slot is already occupied.
    Occupied,
    /// The requested rights are not a subset of the source's (would widen), or
    /// a badge rule was violated.
    NotPermitted,
}

/// A flat table of capability slots backed by a caller-provided slice.
pub struct CapSpace<'a, Obj> {
    slots: &'a mut [Slot<Obj>],
}

impl<'a, Obj: Copy> CapSpace<'a, Obj> {
    /// Wrap `slots` as a capability space. All slots should start empty.
    #[must_use]
    pub fn new(slots: &'a mut [Slot<Obj>]) -> Self {
        Self { slots }
    }

    /// Number of slots.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// Whether `id` names a slot of this table.
    #[must_use]
    pub fn contains(&self, id: CapId) -> bool {
        (id.0 as usize) < self.slots.len()
    }

    fn index(&self, id: CapId) -> Result<usize, CapError> {
        if self.contains(id) {
            Ok(id.0 as usize)
        } else {
            Err(CapError::OutOfRange)
        }
    }

    /// Every occupied slot and the capability it holds, in slot order.
    pub fn iter(&self) -> impl Iterator<Item = (CapId, Capability<Obj>)> + '_ {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.cap.map(|c| (CapId(i as u32), c)))
    }

    /// The capability at `id`, or an error if the slot is out of range or empty.
    pub fn get(&self, id: CapId) -> Result<Capability<Obj>, CapError> {
        let i = self.index(id)?;
        self.slots[i].cap.ok_or(CapError::Empty)
    }

    /// Whether the slot at `id` holds a capability.
    #[must_use]
    pub fn is_occupied(&self, id: CapId) -> bool {
        self.get(id).is_ok()
    }

    /// The first empty slot, if any.
    #[must_use]
    pub fn first_empty(&self) -> Option<CapId> {
        self.slots
            .iter()
            .position(|s| s.cap.is_none())
            .map(|i| CapId(i as u32))
    }

    /// The whole entry at `id`, or `None` if the slot is empty.
    ///
    /// # Panics
    /// If `id` is out of range: callers check [`contains`](Self::contains)
    /// first, and a link never names a slot outside its table.
    #[must_use]
    pub fn entry(&self, id: CapId) -> Option<Entry<Obj>> {
        let s = &self.slots[id.0 as usize];
        s.cap.map(|cap| Entry { cap, link: s.link })
    }

    /// Fill the slot at `id` with `entry`, or empty it with `None`. Only
    /// [`crate::mdb`] should call this, through a [`crate::mdb::SlotStore`].
    ///
    /// # Panics
    /// If `id` is out of range, as for [`entry`](Self::entry).
    pub fn set_entry(&mut self, id: CapId, entry: Option<Entry<Obj>>) {
        self.slots[id.0 as usize] = match entry {
            Some(e) => Slot {
                cap: Some(e.cap),
                link: e.link,
            },
            None => Slot::empty(),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Rights;

    fn entry(object: u64) -> Entry<u64> {
        Entry {
            cap: Capability::new(object, Rights::READ),
            link: Link::default(),
        }
    }

    #[test]
    fn iter_yields_only_occupied_slots_in_order() {
        let mut slots = [Slot::empty(); 4];
        let mut space = CapSpace::new(&mut slots);
        space.set_entry(CapId(3), Some(entry(30)));
        space.set_entry(CapId(1), Some(entry(10)));
        let mut seen = [(0, 0); 2];
        for (n, (id, c)) in space.iter().enumerate() {
            seen[n] = (id.0, c.object);
        }
        assert_eq!(seen, [(1, 10), (3, 30)]);
    }

    #[test]
    fn entries_round_trip_and_clear() {
        let mut slots = [Slot::empty(); 4];
        let mut space = CapSpace::new(&mut slots);
        let mut e = entry(7);
        e.link = Link {
            prev: Some(crate::mdb::SlotRef::new(1, 2)),
            next: None,
            depth: 3,
        };
        space.set_entry(CapId(2), Some(e));
        assert_eq!(space.entry(CapId(2)), Some(e));
        assert!(space.is_occupied(CapId(2)));
        assert_eq!(space.get(CapId(2)).unwrap().object, 7);
        space.set_entry(CapId(2), None);
        assert_eq!(space.entry(CapId(2)), None);
        assert_eq!(space.get(CapId(2)), Err(CapError::Empty));
    }

    #[test]
    fn first_empty_finds_a_free_slot_or_none() {
        let mut slots = [Slot::empty(); 2];
        let mut space = CapSpace::new(&mut slots);
        assert_eq!(space.first_empty(), Some(CapId(0)));
        space.set_entry(CapId(0), Some(entry(1)));
        assert_eq!(space.first_empty(), Some(CapId(1)));
        space.set_entry(CapId(1), Some(entry(2)));
        assert_eq!(space.first_empty(), None);
    }

    #[test]
    fn out_of_range_is_caught() {
        let mut slots = [Slot::<u64>::empty(); 2];
        let space = CapSpace::new(&mut slots);
        assert!(!space.contains(CapId(2)));
        assert_eq!(space.get(CapId(5)), Err(CapError::OutOfRange));
        assert!(!space.is_occupied(CapId(5)));
    }
}
