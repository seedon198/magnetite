#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

//! The capability algebra: **rights** and **attenuation**, generic over the
//! kernel object a capability names.
//!
//! A capability is an unforgeable reference to a kernel object plus the
//! [`Rights`] held over it. The security model ([ADR 0006](../../docs/adr/0006-security-model.md))
//! is *no ambient authority* and *attenuation only*: a capability may be
//! derived into an equal-or-weaker one, never a stronger one. This crate
//! captures that algebra with **no** `unsafe` and no hardware, so it is
//! unit-tested on the host; the kernel instantiates [`Capability`] over its
//! real object handles and stores them in the per-task capability space
//! (M3, [ADR 0016](../../docs/adr/0016-capability-implementation.md)).

pub mod abi;
pub mod invoke;
pub mod object;
pub mod space;

pub use abi::{MessageInfo, SysError, MSG_REGISTERS, NO_SLOT};
pub use invoke::{InvokeError, InvokeOk, Method};
pub use object::{
    cnode_bytes, slot_align, EndpointState, NotificationState, ObjectKind, ObjectRef, Untyped,
};
pub use space::{CapError, CapId, CapSpace, Slot};

/// A set of rights a capability carries, as bit flags.
///
/// Rights are generic across object kinds; a given object only interprets the
/// ones that apply to it (e.g. a `Frame` reads [`Rights::MAP`]/[`Rights::WRITE`],
/// an `Endpoint` reads [`Rights::SEND`]/[`Rights::RECV`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rights(u32);

impl Rights {
    /// No rights.
    pub const NONE: Self = Self(0);
    /// Read the object's contents.
    pub const READ: Self = Self(1 << 0);
    /// Write the object's contents.
    pub const WRITE: Self = Self(1 << 1);
    /// Allow mappings to be executable (paired with map).
    pub const EXECUTE: Self = Self(1 << 2);
    /// Map the object into an address space (e.g. a `Frame`).
    pub const MAP: Self = Self(1 << 3);
    /// Send a message / capability through an endpoint.
    pub const SEND: Self = Self(1 << 4);
    /// Receive a message / capability from an endpoint.
    pub const RECV: Self = Self(1 << 5);
    /// Transfer (grant) capabilities along with a message.
    pub const GRANT: Self = Self(1 << 6);
    /// Retype untyped memory into objects.
    pub const RETYPE: Self = Self(1 << 7);

    /// Every defined right (the maximum a freshly created object cap holds).
    pub const ALL: Self = Self(
        Self::READ.0
            | Self::WRITE.0
            | Self::EXECUTE.0
            | Self::MAP.0
            | Self::SEND.0
            | Self::RECV.0
            | Self::GRANT.0
            | Self::RETYPE.0,
    );

    /// Construct from raw bits, keeping only defined rights.
    #[must_use]
    pub const fn from_bits_truncate(bits: u32) -> Self {
        Self(bits & Self::ALL.0)
    }

    /// The raw bits.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Whether every right in `other` is present in `self`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether no rights are set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The union of two rights sets.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// The intersection of two rights sets - the core of attenuation: the
    /// result can never contain a right absent from either operand.
    #[must_use]
    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }
}

impl core::ops::BitOr for Rights {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

/// An unforgeable reference to a kernel object (`Obj`) together with the rights
/// held over it and an optional badge.
///
/// `Obj` is the kernel's handle to the object (an index, a physical address, a
/// typed id - the kernel chooses); this crate treats it opaquely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capability<Obj> {
    /// The object this capability names.
    pub object: Obj,
    /// The rights held over the object.
    pub rights: Rights,
    /// An identifier the receiver sees (endpoints); `0` means unbadged.
    pub badge: u64,
}

impl<Obj: Copy> Capability<Obj> {
    /// A fresh, unbadged capability with the given rights.
    #[must_use]
    pub fn new(object: Obj, rights: Rights) -> Self {
        Self {
            object,
            rights,
            badge: 0,
        }
    }

    /// Whether this capability permits everything in `needed`.
    #[must_use]
    pub fn allows(self, needed: Rights) -> bool {
        self.rights.contains(needed)
    }

    /// Derive a weaker capability holding exactly `want` rights.
    ///
    /// Returns `None` if `want` is not a subset of the current rights - a
    /// capability can only be **attenuated**, never widened. The badge is
    /// preserved.
    #[must_use]
    pub fn derive(self, want: Rights) -> Option<Self> {
        if self.rights.contains(want) {
            Some(Self {
                rights: want,
                ..self
            })
        } else {
            None
        }
    }

    /// Mint a derived capability with rights `want` and a `badge`.
    ///
    /// Minting may only **attenuate** rights, and may set a badge only on a
    /// currently-unbadged capability (badge `0`); re-badging an already-badged
    /// capability, or widening rights, returns `None`.
    #[must_use]
    pub fn mint(self, want: Rights, badge: u64) -> Option<Self> {
        if !self.rights.contains(want) {
            return None;
        }
        if badge != 0 && self.badge != 0 && badge != self.badge {
            return None;
        }
        Some(Self {
            object: self.object,
            rights: want,
            badge: if badge != 0 { badge } else { self.badge },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_and_intersection_define_attenuation() {
        let rw = Rights::READ | Rights::WRITE;
        assert!(rw.contains(Rights::READ));
        assert!(!Rights::READ.contains(rw));
        // Intersection can only drop rights.
        assert_eq!(rw.intersection(Rights::READ), Rights::READ);
        assert_eq!(rw.intersection(Rights::ALL), rw);
    }

    #[test]
    fn from_bits_truncate_drops_undefined() {
        let r = Rights::from_bits_truncate(0xFFFF_FFFF);
        assert_eq!(r, Rights::ALL);
    }

    #[test]
    fn derive_only_weakens() {
        let cap = Capability::new(42u64, Rights::READ | Rights::WRITE | Rights::MAP);
        // Attenuation to a subset is allowed.
        let ro = cap.derive(Rights::READ).unwrap();
        assert_eq!(ro.rights, Rights::READ);
        assert_eq!(ro.object, 42);
        // Asking for a right we don't hold is rejected.
        assert!(cap.derive(Rights::SEND).is_none());
        // A derived cap cannot be re-widened beyond itself.
        assert!(ro.derive(Rights::READ | Rights::WRITE).is_none());
    }

    #[test]
    fn derive_preserves_object_and_badge() {
        let mut cap = Capability::new(7u32, Rights::ALL);
        cap.badge = 99;
        let d = cap.derive(Rights::READ).unwrap();
        assert_eq!(d.object, 7);
        assert_eq!(d.badge, 99);
    }

    #[test]
    fn mint_sets_badge_once_and_attenuates() {
        let cap = Capability::new(1u64, Rights::SEND | Rights::GRANT);
        let badged = cap.mint(Rights::SEND, 0xABCD).unwrap();
        assert_eq!(badged.badge, 0xABCD);
        assert_eq!(badged.rights, Rights::SEND);
        // Re-badging a badged cap to a different badge is rejected.
        assert!(badged.mint(Rights::SEND, 0x1234).is_none());
        // Re-minting with the same badge is fine.
        assert!(badged.mint(Rights::SEND, 0xABCD).is_some());
        // Minting cannot widen rights.
        assert!(cap.mint(Rights::RECV, 1).is_none());
    }

    #[test]
    fn allows_checks_required_rights() {
        let cap = Capability::new((), Rights::READ | Rights::MAP);
        assert!(cap.allows(Rights::READ));
        assert!(cap.allows(Rights::READ | Rights::MAP));
        assert!(!cap.allows(Rights::WRITE));
    }
}
