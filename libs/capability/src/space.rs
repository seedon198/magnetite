//! Per-task **capability space**: a flat table of capability slots.
//!
//! A [`CapId`] is an index into the table; lookup is O(1). Operations enforce
//! the capability rules from [`crate`]: a slot can be filled only when empty,
//! and `copy`/`mint` may only **attenuate** the source capability's rights
//! (never widen). The table operates over caller-provided storage (as the frame
//! allocator does with its bitmap), so this stays `#![forbid(unsafe_code)]` and
//! host-testable; the kernel composes a growable two-level space from these
//! tables, allocating the backing slots from Untyped memory (ADR 0016).

use crate::{Capability, Rights};

/// An index naming a capability slot within a [`CapSpace`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CapId(pub u32);

/// One capability slot: empty, or holding a [`Capability`].
#[derive(Debug, Clone, Copy)]
pub struct Slot<Obj> {
    cap: Option<Capability<Obj>>,
}

impl<Obj> Slot<Obj> {
    /// An empty slot.
    #[must_use]
    pub const fn empty() -> Self {
        Self { cap: None }
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

    fn index(&self, id: CapId) -> Result<usize, CapError> {
        let i = id.0 as usize;
        if i < self.slots.len() {
            Ok(i)
        } else {
            Err(CapError::OutOfRange)
        }
    }

    /// Every occupied slot and the capability it holds, in slot order.
    pub fn iter(&self) -> impl Iterator<Item = (CapId, Capability<Obj>)> + '_
    where
        Obj: Copy,
    {
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
        self.index(id)
            .map(|i| self.slots[i].cap.is_some())
            .unwrap_or(false)
    }

    /// Place `cap` into the first empty slot; returns its [`CapId`], or `None`
    /// if the table is full.
    pub fn insert(&mut self, cap: Capability<Obj>) -> Option<CapId> {
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if slot.cap.is_none() {
                slot.cap = Some(cap);
                return Some(CapId(i as u32));
            }
        }
        None
    }

    /// Place `cap` into the slot at `id`, which must be empty.
    pub fn insert_at(&mut self, id: CapId, cap: Capability<Obj>) -> Result<(), CapError> {
        let i = self.index(id)?;
        if self.slots[i].cap.is_some() {
            return Err(CapError::Occupied);
        }
        self.slots[i].cap = Some(cap);
        Ok(())
    }

    /// Remove and return the capability at `id`.
    pub fn delete(&mut self, id: CapId) -> Result<Capability<Obj>, CapError> {
        let i = self.index(id)?;
        self.slots[i].cap.take().ok_or(CapError::Empty)
    }

    /// Move the capability from `src` to `dst` (which must be empty); `src`
    /// becomes empty.
    pub fn move_cap(&mut self, src: CapId, dst: CapId) -> Result<(), CapError> {
        let cap = self.get(src)?;
        let d = self.index(dst)?;
        if self.slots[d].cap.is_some() {
            return Err(CapError::Occupied);
        }
        self.slots[d].cap = Some(cap);
        let s = self.index(src)?;
        self.slots[s].cap = None;
        Ok(())
    }

    /// Copy `src` into the empty slot `dst`, **attenuated** to `mask` rights.
    /// Fails with [`CapError::NotPermitted`] if `mask` is not a subset of
    /// `src`'s rights (a copy can only weaken).
    pub fn copy(&mut self, src: CapId, dst: CapId, mask: Rights) -> Result<(), CapError> {
        let derived = self.get(src)?.derive(mask).ok_or(CapError::NotPermitted)?;
        self.insert_at(dst, derived)
    }

    /// Like [`copy`](Self::copy) but also set a `badge` on the new capability
    /// (mint). Fails if `mask` would widen rights or the badge rule is violated.
    pub fn mint(
        &mut self,
        src: CapId,
        dst: CapId,
        mask: Rights,
        badge: u64,
    ) -> Result<(), CapError> {
        let minted = self
            .get(src)?
            .mint(mask, badge)
            .ok_or(CapError::NotPermitted)?;
        self.insert_at(dst, minted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Rights;

    #[test]
    fn iter_yields_only_occupied_slots_in_order() {
        let mut slots = [Slot::empty(); 4];
        let mut space = CapSpace::new(&mut slots);
        space
            .insert_at(CapId(3), Capability::new(30u32, Rights::READ))
            .unwrap();
        space
            .insert_at(CapId(1), Capability::new(10u32, Rights::READ))
            .unwrap();
        let seen: alloc_free::Pairs = space.iter().map(|(id, c)| (id.0, c.object)).collect();
        assert_eq!(seen.as_slice(), &[(1, 10), (3, 30)]);
    }

    /// A tiny fixed-capacity collector, since the crate is `no_std`.
    mod alloc_free {
        #[derive(Default)]
        pub struct Pairs([(u32, u32); 8], usize);
        impl Pairs {
            pub fn as_slice(&self) -> &[(u32, u32)] {
                &self.0[..self.1]
            }
        }
        impl FromIterator<(u32, u32)> for Pairs {
            fn from_iter<I: IntoIterator<Item = (u32, u32)>>(it: I) -> Self {
                let mut p = Self::default();
                for x in it {
                    p.0[p.1] = x;
                    p.1 += 1;
                }
                p
            }
        }
    }

    fn space(slots: &mut [Slot<u64>]) -> CapSpace<'_, u64> {
        CapSpace::new(slots)
    }

    #[test]
    fn insert_get_delete() {
        let mut backing = [Slot::<u64>::empty(); 8];
        let mut cs = space(&mut backing);
        let cap = Capability::new(0x1000u64, Rights::READ | Rights::WRITE);
        let id = cs.insert(cap).unwrap();
        assert_eq!(id, CapId(0));
        assert!(cs.is_occupied(id));
        assert_eq!(cs.get(id).unwrap().object, 0x1000);
        let removed = cs.delete(id).unwrap();
        assert_eq!(removed.object, 0x1000);
        assert!(!cs.is_occupied(id));
        assert_eq!(cs.get(id), Err(CapError::Empty));
    }

    #[test]
    fn insert_fills_first_free_and_can_exhaust() {
        let mut backing = [Slot::<u64>::empty(); 2];
        let mut cs = space(&mut backing);
        assert_eq!(cs.insert(Capability::new(1, Rights::READ)), Some(CapId(0)));
        assert_eq!(cs.insert(Capability::new(2, Rights::READ)), Some(CapId(1)));
        assert_eq!(cs.insert(Capability::new(3, Rights::READ)), None);
        // Freeing a slot lets the next insert reuse it.
        cs.delete(CapId(0)).unwrap();
        assert_eq!(cs.insert(Capability::new(4, Rights::READ)), Some(CapId(0)));
    }

    #[test]
    fn copy_attenuates_and_rejects_widening() {
        let mut backing = [Slot::<u64>::empty(); 4];
        let mut cs = space(&mut backing);
        cs.insert_at(
            CapId(0),
            Capability::new(7, Rights::READ | Rights::WRITE | Rights::MAP),
        )
        .unwrap();
        // Copy a weaker cap into slot 1.
        cs.copy(CapId(0), CapId(1), Rights::READ).unwrap();
        assert_eq!(cs.get(CapId(1)).unwrap().rights, Rights::READ);
        assert_eq!(cs.get(CapId(1)).unwrap().object, 7);
        // The original is unchanged.
        assert!(cs.get(CapId(0)).unwrap().rights.contains(Rights::WRITE));
        // Copying a right the source lacks is rejected.
        assert_eq!(
            cs.copy(CapId(0), CapId(2), Rights::SEND),
            Err(CapError::NotPermitted)
        );
    }

    #[test]
    fn copy_into_occupied_slot_fails() {
        let mut backing = [Slot::<u64>::empty(); 4];
        let mut cs = space(&mut backing);
        cs.insert_at(CapId(0), Capability::new(1, Rights::ALL))
            .unwrap();
        cs.insert_at(CapId(1), Capability::new(2, Rights::READ))
            .unwrap();
        assert_eq!(
            cs.copy(CapId(0), CapId(1), Rights::READ),
            Err(CapError::Occupied)
        );
    }

    #[test]
    fn move_empties_source() {
        let mut backing = [Slot::<u64>::empty(); 4];
        let mut cs = space(&mut backing);
        cs.insert_at(CapId(0), Capability::new(9, Rights::ALL))
            .unwrap();
        cs.move_cap(CapId(0), CapId(3)).unwrap();
        assert!(!cs.is_occupied(CapId(0)));
        assert_eq!(cs.get(CapId(3)).unwrap().object, 9);
    }

    #[test]
    fn mint_sets_badge_on_copy() {
        let mut backing = [Slot::<u64>::empty(); 4];
        let mut cs = space(&mut backing);
        cs.insert_at(CapId(0), Capability::new(1, Rights::SEND | Rights::GRANT))
            .unwrap();
        cs.mint(CapId(0), CapId(1), Rights::SEND, 0x42).unwrap();
        let minted = cs.get(CapId(1)).unwrap();
        assert_eq!(minted.badge, 0x42);
        assert_eq!(minted.rights, Rights::SEND);
    }

    #[test]
    fn out_of_range_is_caught() {
        let mut backing = [Slot::<u64>::empty(); 2];
        let mut cs = space(&mut backing);
        assert_eq!(cs.get(CapId(5)), Err(CapError::OutOfRange));
        assert_eq!(
            cs.insert_at(CapId(5), Capability::new(1, Rights::READ)),
            Err(CapError::OutOfRange)
        );
    }
}
