//! Capability **invocation**: the typed method surface and its authorization.
//!
//! Everything a task can do is invoke a capability it holds with a [`Method`].
//! This module defines the methods, the result/error types, and the pure
//! authorization check (the invoked capability must be of the method's target
//! [`ObjectKind`] and hold the method's [required rights](Method::required_rights)).
//! The kernel performs the effects; M4's ring-3 `syscall` entry decodes raw
//! register arguments into a [`Method`] and calls the kernel's `invoke`
//! (ADR 0016).

use crate::object::{ObjectKind, ObjectRef};
use crate::space::{CapError, CapId};
use crate::{Capability, Rights};

/// A method invoked on a capability. Each variant targets one object kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// Copy a capability `src` → `dst` within the CNode, attenuating to
    /// `rights` (a subset of the source's).
    CNodeCopy {
        /// Source slot.
        src: CapId,
        /// Destination slot (must be empty).
        dst: CapId,
        /// Rights for the copy (subset of the source's).
        rights: Rights,
    },
    /// Like [`CNodeCopy`](Method::CNodeCopy) but also set a `badge`.
    CNodeMint {
        /// Source slot.
        src: CapId,
        /// Destination slot (must be empty).
        dst: CapId,
        /// Rights for the mint (subset of the source's).
        rights: Rights,
        /// Badge to stamp on the new capability.
        badge: u64,
    },
    /// Move a capability `src` → `dst` (source emptied).
    CNodeMove {
        /// Source slot.
        src: CapId,
        /// Destination slot (must be empty).
        dst: CapId,
    },
    /// Delete the capability in slot `id`.
    CNodeDelete {
        /// Slot to clear.
        id: CapId,
    },
    /// Revoke the capability in slot `id`: delete everything derived from
    /// it, but not the capability itself. Does at most
    /// [`REVOKE_BUDGET`](crate::mdb::REVOKE_BUDGET) deletions, then fails
    /// with [`InvokeError::Again`] if more remain; invoking it again
    /// continues (ADR 0020).
    CNodeRevoke {
        /// Slot whose descendants to delete.
        id: CapId,
    },
    /// Identify a Frame: returns its physical address.
    FrameIdentify,
    /// Retype this Untyped into `count` objects of a fixed-size `kind`,
    /// installing the first new capability at `dst` in the acting cap space.
    UntypedRetype {
        /// Kind of object to create (a fixed-size kind).
        kind: ObjectKind,
        /// How many to create.
        count: u64,
        /// Slot to install the (first) new capability into.
        dst: CapId,
    },
    /// Signal a Notification: OR `bits` into its signal word.
    NotificationSignal {
        /// Bits to set.
        bits: u64,
    },
}

/// The successful result of an invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvokeOk {
    /// The method completed with no return value.
    Done,
    /// A physical address (e.g. from `FrameIdentify`).
    Phys(u64),
    /// A reference to a newly created object (e.g. from `UntypedRetype`).
    Created(ObjectRef),
}

/// Why an invocation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvokeError {
    /// The invoked capability is not of the method's target kind.
    WrongObject,
    /// The invoked capability lacks a right the method requires.
    InsufficientRights,
    /// A capability-space operation failed (empty/occupied/out-of-range).
    Cap(CapError),
    /// The Untyped region could not satisfy a retype.
    OutOfMemory,
    /// The method is not supported for the given arguments (e.g. retyping a
    /// variable-size kind through this path).
    Unsupported,
    /// A revoke used its budget with descendants left: invoke it again.
    Again,
}

impl Method {
    /// The object kind this method must be invoked on.
    #[must_use]
    pub fn target_kind(&self) -> ObjectKind {
        match self {
            Method::CNodeCopy { .. }
            | Method::CNodeMint { .. }
            | Method::CNodeMove { .. }
            | Method::CNodeDelete { .. }
            | Method::CNodeRevoke { .. } => ObjectKind::CNode,
            Method::FrameIdentify => ObjectKind::Frame,
            Method::UntypedRetype { .. } => ObjectKind::Untyped,
            Method::NotificationSignal { .. } => ObjectKind::Notification,
        }
    }

    /// The rights the invoked capability must hold for this method.
    #[must_use]
    pub fn required_rights(&self) -> Rights {
        match self {
            // Modifying a CNode's slots requires write authority over it.
            Method::CNodeCopy { .. }
            | Method::CNodeMint { .. }
            | Method::CNodeMove { .. }
            | Method::CNodeDelete { .. }
            | Method::CNodeRevoke { .. } => Rights::WRITE,
            Method::FrameIdentify => Rights::READ,
            Method::UntypedRetype { .. } => Rights::RETYPE,
            Method::NotificationSignal { .. } => Rights::SEND,
        }
    }

    /// Check that `invoked` may be invoked with this method: it must be of the
    /// target kind and hold the required rights.
    ///
    /// # Errors
    /// [`InvokeError::WrongObject`] or [`InvokeError::InsufficientRights`].
    pub fn authorize(&self, invoked: &Capability<ObjectRef>) -> Result<(), InvokeError> {
        if invoked.object.kind != self.target_kind() {
            return Err(InvokeError::WrongObject);
        }
        if !invoked.allows(self.required_rights()) {
            return Err(InvokeError::InsufficientRights);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(kind: ObjectKind, rights: Rights) -> Capability<ObjectRef> {
        Capability::new(ObjectRef::new(kind, 0x1000), rights)
    }

    #[test]
    fn target_kinds_match_methods() {
        assert_eq!(Method::FrameIdentify.target_kind(), ObjectKind::Frame);
        assert_eq!(
            Method::NotificationSignal { bits: 1 }.target_kind(),
            ObjectKind::Notification
        );
        assert_eq!(
            Method::CNodeDelete { id: CapId(0) }.target_kind(),
            ObjectKind::CNode
        );
        let revoke = Method::CNodeRevoke { id: CapId(0) };
        assert_eq!(revoke.target_kind(), ObjectKind::CNode);
        assert_eq!(revoke.required_rights(), Rights::WRITE);
        assert_eq!(
            Method::UntypedRetype {
                kind: ObjectKind::Frame,
                count: 1,
                dst: CapId(0)
            }
            .target_kind(),
            ObjectKind::Untyped
        );
    }

    #[test]
    fn authorize_checks_kind_then_rights() {
        let m = Method::FrameIdentify;
        // Wrong kind.
        assert_eq!(
            m.authorize(&cap(ObjectKind::Endpoint, Rights::ALL)),
            Err(InvokeError::WrongObject)
        );
        // Right kind, missing READ.
        assert_eq!(
            m.authorize(&cap(ObjectKind::Frame, Rights::WRITE)),
            Err(InvokeError::InsufficientRights)
        );
        // Right kind and right.
        assert_eq!(m.authorize(&cap(ObjectKind::Frame, Rights::READ)), Ok(()));
    }

    #[test]
    fn retype_requires_retype_right() {
        let m = Method::UntypedRetype {
            kind: ObjectKind::Frame,
            count: 1,
            dst: CapId(1),
        };
        assert_eq!(m.required_rights(), Rights::RETYPE);
        assert_eq!(
            m.authorize(&cap(ObjectKind::Untyped, Rights::WRITE)),
            Err(InvokeError::InsufficientRights)
        );
        assert_eq!(
            m.authorize(&cap(ObjectKind::Untyped, Rights::RETYPE)),
            Ok(())
        );
    }

    #[test]
    fn signal_requires_send() {
        let m = Method::NotificationSignal { bits: 0b101 };
        assert_eq!(m.required_rights(), Rights::SEND);
        assert_eq!(
            m.authorize(&cap(ObjectKind::Notification, Rights::RECV)),
            Err(InvokeError::InsufficientRights)
        );
        assert_eq!(
            m.authorize(&cap(ObjectKind::Notification, Rights::SEND)),
            Ok(())
        );
    }
}
