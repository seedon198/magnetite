//! The system-call ABI: the message-register calling convention.
//!
//! Invoking a capability carries a packed [`MessageInfo`] word - the method
//! **label**, the number of message-register words used (**length**), and the
//! number of capabilities to transfer (**caps**) - plus the message registers
//! themselves and the invoked capability id. This mirrors seL4's IPC shape and
//! is a natural fit for the message payloads blocking IPC adds in M4
//! ([ADR 0016](../../docs/adr/0016-capability-implementation.md),
//! [ADR 0017](../../docs/adr/0017-threads-and-scheduling.md)).
//!
//! It also holds the system-call ABI of
//! [ADR 0019](../../docs/adr/0019-first-user-task.md): syscall numbers, error
//! codes, method labels and the decoding of a method from its registers. All
//! pure, so it is host-tested, and shared by the kernel and `libs/sys`.

/// Number of message-register words carried in registers.
pub const MSG_REGISTERS: usize = 4;

/// A packed message-info word: method `label`, message `length`, and capability
/// transfer `caps` count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageInfo(u64);

impl MessageInfo {
    const LABEL_BITS: u64 = 32;
    const LENGTH_SHIFT: u64 = 32;
    const CAPS_SHIFT: u64 = 40;

    /// Build from a `label`, a message `length` (words), and a `caps` count.
    #[must_use]
    pub const fn new(label: u32, length: u8, caps: u8) -> Self {
        Self(
            (label as u64)
                | ((length as u64) << Self::LENGTH_SHIFT)
                | ((caps as u64) << Self::CAPS_SHIFT),
        )
    }

    /// Reinterpret a raw word (as delivered in a register).
    #[must_use]
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    /// The raw word.
    #[must_use]
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// The method label.
    #[must_use]
    pub const fn label(self) -> u32 {
        (self.0 & ((1 << Self::LABEL_BITS) - 1)) as u32
    }

    /// The number of message-register words used (clamped to [`MSG_REGISTERS`]).
    #[must_use]
    pub const fn length(self) -> u8 {
        let n = (self.0 >> Self::LENGTH_SHIFT) as u8;
        if (n as usize) > MSG_REGISTERS {
            MSG_REGISTERS as u8
        } else {
            n
        }
    }

    /// The number of capabilities to transfer with the message.
    #[must_use]
    pub const fn caps(self) -> u8 {
        (self.0 >> Self::CAPS_SHIFT) as u8
    }
}

// ------------------------------------------------------------- the syscall ABI
//
// ADR 0019 decision 6. Registers: `rax` = number in, error out (0 = success);
// `rdi` = capability slot in, badge out; `rsi` = `MessageInfo`; `rdx`, `r8`,
// `r9`, `r10` = message registers 0 to 3; `r12` = receive slot (`NO_SLOT` for
// none); `r13` = the slot of the capability to send.

use crate::invoke::{InvokeError, Method};
use crate::object::ObjectKind;
use crate::space::{CapError, CapId};
use crate::Rights;

/// System-call numbers (in `rax`).
pub mod sys {
    /// Send a message through an endpoint; blocks until received.
    pub const SEND: u64 = 1;
    /// Receive a message from an endpoint; blocks until one arrives.
    pub const RECV: u64 = 2;
    /// Send, then wait for the reply; on a non-endpoint capability, invoke a
    /// kernel object method.
    pub const CALL: u64 = 3;
    /// Answer the caller in this thread's reply slot.
    pub const REPLY: u64 = 4;
    /// Reply, then receive, in one step.
    pub const REPLY_RECV: u64 = 5;
    /// Give up the processor.
    pub const YIELD: u64 = 6;
    /// End the calling thread.
    pub const EXIT: u64 = 7;
}

/// "No slot" in `r12` (receive slot): refuse an incoming capability.
pub const NO_SLOT: u64 = u64::MAX;

/// A system-call error, returned in `rax` as its [`code`](SysError::code).
/// One per `IpcError` and [`InvokeError`] variant, plus the syscall layer's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum SysError {
    /// The endpoint capability lacks `SEND` or `RECV`.
    MissingRight = 1,
    /// A capability was sent through an endpoint capability without `GRANT`.
    NoGrant = 2,
    /// The message claims more than one capability.
    TooManyCaps = 3,
    /// The slot naming the capability to send is empty.
    SourceEmpty = 4,
    /// A capability was sent but the receiver named no receive slot.
    NoReceiveSlot = 5,
    /// The receive slot is already occupied.
    ReceiveSlotFull = 6,
    /// `Reply` with no caller waiting in the reply slot.
    NoReplySlot = 7,
    /// `Recv` while still holding a caller in the reply slot.
    ReplySlotFull = 8,
    /// The thread this caller was waiting on for a reply exited.
    PeerExited = 9,
    /// The invoked capability is not of the method's target kind.
    WrongObject = 10,
    /// The invoked capability lacks a right the method requires.
    InsufficientRights = 11,
    /// A slot a method names is outside the capability space.
    CapOutOfRange = 12,
    /// A slot a method names is empty.
    CapEmpty = 13,
    /// A destination slot a method names is occupied.
    CapOccupied = 14,
    /// The requested rights would widen the source's.
    CapNotPermitted = 15,
    /// The Untyped region cannot satisfy a retype.
    OutOfMemory = 16,
    /// The method is not supported with these arguments.
    Unsupported = 17,
    /// The slot the syscall names is null, empty, out of range or the wrong
    /// kind of capability for the operation.
    InvalidCap = 18,
    /// An unknown syscall number or method label, or malformed arguments.
    InvalidSyscall = 19,
}

impl SysError {
    /// The value returned in `rax`.
    #[must_use]
    pub const fn code(self) -> u64 {
        self as u64
    }

    /// Decode a value from `rax` (`None` for 0, which is success, and for
    /// unknown codes).
    #[must_use]
    pub fn from_code(code: u64) -> Option<Self> {
        use SysError::*;
        Some(match code {
            1 => MissingRight,
            2 => NoGrant,
            3 => TooManyCaps,
            4 => SourceEmpty,
            5 => NoReceiveSlot,
            6 => ReceiveSlotFull,
            7 => NoReplySlot,
            8 => ReplySlotFull,
            9 => PeerExited,
            10 => WrongObject,
            11 => InsufficientRights,
            12 => CapOutOfRange,
            13 => CapEmpty,
            14 => CapOccupied,
            15 => CapNotPermitted,
            16 => OutOfMemory,
            17 => Unsupported,
            18 => InvalidCap,
            19 => InvalidSyscall,
            _ => return None,
        })
    }
}

impl From<CapError> for SysError {
    fn from(e: CapError) -> Self {
        match e {
            CapError::OutOfRange => SysError::CapOutOfRange,
            CapError::Empty => SysError::CapEmpty,
            CapError::Occupied => SysError::CapOccupied,
            CapError::NotPermitted => SysError::CapNotPermitted,
        }
    }
}

impl From<InvokeError> for SysError {
    fn from(e: InvokeError) -> Self {
        match e {
            InvokeError::WrongObject => SysError::WrongObject,
            InvokeError::InsufficientRights => SysError::InsufficientRights,
            InvokeError::Cap(c) => c.into(),
            InvokeError::OutOfMemory => SysError::OutOfMemory,
            InvokeError::Unsupported => SysError::Unsupported,
        }
    }
}

/// Method labels for a `Call` on a kernel object (not an endpoint).
pub mod label {
    /// CNode copy: `mr0` = src, `mr1` = dst, `mr2` = rights.
    pub const CNODE_COPY: u32 = 1;
    /// CNode mint: as copy, plus `mr3` = badge.
    pub const CNODE_MINT: u32 = 2;
    /// CNode move: `mr0` = src, `mr1` = dst.
    pub const CNODE_MOVE: u32 = 3;
    /// CNode delete: `mr0` = slot.
    pub const CNODE_DELETE: u32 = 4;
    /// Frame identify: no arguments; returns `mr0` = kind code, `mr1` = size.
    pub const FRAME_IDENTIFY: u32 = 5;
    /// Untyped retype: `mr0` = kind code, `mr1` = count (1), `mr2` = dst.
    pub const UNTYPED_RETYPE: u32 = 6;
    /// Notification signal: `mr0` = bits.
    pub const NOTIFICATION_SIGNAL: u32 = 7;
}

/// The ABI code of an object kind.
#[must_use]
pub const fn kind_code(kind: ObjectKind) -> u64 {
    match kind {
        ObjectKind::Untyped => 0,
        ObjectKind::CNode => 1,
        ObjectKind::Frame => 2,
        ObjectKind::Endpoint => 3,
        ObjectKind::Notification => 4,
        ObjectKind::AddressSpace => 5,
        ObjectKind::Thread => 6,
    }
}

/// The object kind for an ABI code.
#[must_use]
pub const fn kind_from_code(code: u64) -> Option<ObjectKind> {
    Some(match code {
        0 => ObjectKind::Untyped,
        1 => ObjectKind::CNode,
        2 => ObjectKind::Frame,
        3 => ObjectKind::Endpoint,
        4 => ObjectKind::Notification,
        5 => ObjectKind::AddressSpace,
        6 => ObjectKind::Thread,
        _ => return None,
    })
}

/// A slot named by a task. Slot 0 is the null slot and is never valid, nor is
/// any id wider than a [`CapId`] ([`NO_SLOT`] included).
pub fn slot(raw: u64) -> Result<CapId, SysError> {
    match u32::try_from(raw) {
        Ok(0) | Err(_) => Err(SysError::InvalidCap),
        Ok(id) => Ok(CapId(id)),
    }
}

/// Rights named by a task: only defined bits.
fn rights(raw: u64) -> Result<Rights, SysError> {
    let bits = u32::try_from(raw).map_err(|_| SysError::InvalidSyscall)?;
    let r = Rights::from_bits_truncate(bits);
    if r.bits() == bits {
        Ok(r)
    } else {
        Err(SysError::InvalidSyscall)
    }
}

/// Decode a kernel-object method from a `Call`'s label and message registers.
/// Every slot it names passes [`slot`]; a retype must be of one Frame,
/// Endpoint or Notification.
pub fn decode_method(info: MessageInfo, mr: [u64; MSG_REGISTERS]) -> Result<Method, SysError> {
    let need = |n: u8| {
        if info.length() < n {
            Err(SysError::InvalidSyscall)
        } else {
            Ok(())
        }
    };
    Ok(match info.label() {
        label::CNODE_COPY => {
            need(3)?;
            Method::CNodeCopy {
                src: slot(mr[0])?,
                dst: slot(mr[1])?,
                rights: rights(mr[2])?,
            }
        }
        label::CNODE_MINT => {
            need(4)?;
            Method::CNodeMint {
                src: slot(mr[0])?,
                dst: slot(mr[1])?,
                rights: rights(mr[2])?,
                badge: mr[3],
            }
        }
        label::CNODE_MOVE => {
            need(2)?;
            Method::CNodeMove {
                src: slot(mr[0])?,
                dst: slot(mr[1])?,
            }
        }
        label::CNODE_DELETE => {
            need(1)?;
            Method::CNodeDelete { id: slot(mr[0])? }
        }
        label::FRAME_IDENTIFY => Method::FrameIdentify,
        label::UNTYPED_RETYPE => {
            need(3)?;
            let kind = kind_from_code(mr[0]).ok_or(SysError::InvalidSyscall)?;
            let fixed = matches!(
                kind,
                ObjectKind::Frame | ObjectKind::Endpoint | ObjectKind::Notification
            );
            if !fixed || mr[1] != 1 {
                return Err(SysError::Unsupported);
            }
            Method::UntypedRetype {
                kind,
                count: 1,
                dst: slot(mr[2])?,
            }
        }
        label::NOTIFICATION_SIGNAL => {
            need(1)?;
            Method::NotificationSignal { bits: mr[0] }
        }
        _ => return Err(SysError::InvalidSyscall),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_fields() {
        let mi = MessageInfo::new(0xABCD, 3, 1);
        assert_eq!(mi.label(), 0xABCD);
        assert_eq!(mi.length(), 3);
        assert_eq!(mi.caps(), 1);
        assert_eq!(MessageInfo::from_bits(mi.bits()), mi);
    }

    #[test]
    fn length_is_clamped_to_capacity() {
        // A bogus over-long length is clamped, so decoders never read past the
        // register bank.
        let mi = MessageInfo::new(1, 200, 0);
        assert_eq!(mi.length() as usize, MSG_REGISTERS);
    }

    #[test]
    fn fields_are_independent() {
        let mi = MessageInfo::new(u32::MAX, 0, 0);
        assert_eq!(mi.label(), u32::MAX);
        assert_eq!(mi.length(), 0);
        assert_eq!(mi.caps(), 0);
    }

    // ---------------------------------------------------------------- syscall ABI

    const ALL_ERRORS: [SysError; 19] = [
        SysError::MissingRight,
        SysError::NoGrant,
        SysError::TooManyCaps,
        SysError::SourceEmpty,
        SysError::NoReceiveSlot,
        SysError::ReceiveSlotFull,
        SysError::NoReplySlot,
        SysError::ReplySlotFull,
        SysError::PeerExited,
        SysError::WrongObject,
        SysError::InsufficientRights,
        SysError::CapOutOfRange,
        SysError::CapEmpty,
        SysError::CapOccupied,
        SysError::CapNotPermitted,
        SysError::OutOfMemory,
        SysError::Unsupported,
        SysError::InvalidCap,
        SysError::InvalidSyscall,
    ];

    fn msg(label: u32, length: u8, mr: [u64; MSG_REGISTERS]) -> Result<Method, SysError> {
        decode_method(MessageInfo::new(label, length, 0), mr)
    }

    #[test]
    fn error_codes_round_trip() {
        for (i, e) in ALL_ERRORS.iter().enumerate() {
            assert_eq!(e.code(), i as u64 + 1);
            assert_eq!(SysError::from_code(e.code()), Some(*e));
        }
        assert_eq!(SysError::from_code(0), None);
        assert_eq!(SysError::from_code(20), None);
    }

    #[test]
    fn slot_zero_and_wide_slots_are_invalid_cap() {
        assert_eq!(slot(0), Err(SysError::InvalidCap));
        assert_eq!(slot(1 << 32), Err(SysError::InvalidCap));
        assert_eq!(slot(NO_SLOT), Err(SysError::InvalidCap));
        assert_eq!(slot(5), Ok(CapId(5)));
    }

    #[test]
    fn decodes_each_method() {
        let rw = u64::from((Rights::READ | Rights::WRITE).bits());
        assert_eq!(
            msg(label::CNODE_COPY, 3, [4, 5, rw, 0]),
            Ok(Method::CNodeCopy {
                src: CapId(4),
                dst: CapId(5),
                rights: Rights::READ | Rights::WRITE
            })
        );
        assert_eq!(
            msg(label::CNODE_MINT, 4, [4, 5, rw, 9]),
            Ok(Method::CNodeMint {
                src: CapId(4),
                dst: CapId(5),
                rights: Rights::READ | Rights::WRITE,
                badge: 9
            })
        );
        assert_eq!(
            msg(label::CNODE_MOVE, 2, [4, 5, 0, 0]),
            Ok(Method::CNodeMove {
                src: CapId(4),
                dst: CapId(5)
            })
        );
        assert_eq!(
            msg(label::CNODE_DELETE, 1, [4, 0, 0, 0]),
            Ok(Method::CNodeDelete { id: CapId(4) })
        );
        assert_eq!(
            msg(label::FRAME_IDENTIFY, 0, [0; 4]),
            Ok(Method::FrameIdentify)
        );
        assert_eq!(
            msg(
                label::UNTYPED_RETYPE,
                3,
                [kind_code(ObjectKind::Notification), 1, 4, 0]
            ),
            Ok(Method::UntypedRetype {
                kind: ObjectKind::Notification,
                count: 1,
                dst: CapId(4)
            })
        );
        assert_eq!(
            msg(label::NOTIFICATION_SIGNAL, 1, [0b101, 0, 0, 0]),
            Ok(Method::NotificationSignal { bits: 0b101 })
        );
    }

    #[test]
    fn kind_codes_round_trip() {
        for code in 0..7 {
            let kind = kind_from_code(code).unwrap();
            assert_eq!(kind_code(kind), code);
        }
        assert_eq!(kind_from_code(7), None);
    }

    #[test]
    fn unknown_label_is_invalid_syscall() {
        assert_eq!(msg(0, 4, [0; 4]), Err(SysError::InvalidSyscall));
        assert_eq!(msg(8, 4, [0; 4]), Err(SysError::InvalidSyscall));
    }

    #[test]
    fn short_message_is_invalid_syscall() {
        assert_eq!(
            msg(label::CNODE_COPY, 2, [4, 5, 1, 0]),
            Err(SysError::InvalidSyscall)
        );
    }

    #[test]
    fn unknown_rights_bits_are_invalid_syscall() {
        assert_eq!(
            msg(label::CNODE_COPY, 3, [4, 5, 1 << 8, 0]),
            Err(SysError::InvalidSyscall)
        );
        assert_eq!(
            msg(label::CNODE_COPY, 3, [4, 5, 1 << 40, 0]),
            Err(SysError::InvalidSyscall)
        );
    }

    #[test]
    fn retype_refuses_variable_kinds_and_counts() {
        let cnode = kind_code(ObjectKind::CNode);
        let note = kind_code(ObjectKind::Notification);
        assert_eq!(
            msg(label::UNTYPED_RETYPE, 3, [cnode, 1, 4, 0]),
            Err(SysError::Unsupported)
        );
        assert_eq!(
            msg(label::UNTYPED_RETYPE, 3, [note, 2, 4, 0]),
            Err(SysError::Unsupported)
        );
        assert_eq!(
            msg(label::UNTYPED_RETYPE, 3, [99, 1, 4, 0]),
            Err(SysError::InvalidSyscall)
        );
    }

    #[test]
    fn method_slots_refuse_null() {
        assert_eq!(
            msg(label::CNODE_COPY, 3, [0, 5, 1, 0]),
            Err(SysError::InvalidCap)
        );
        assert_eq!(
            msg(
                label::UNTYPED_RETYPE,
                3,
                [kind_code(ObjectKind::Frame), 1, 0, 0]
            ),
            Err(SysError::InvalidCap)
        );
    }

    #[test]
    fn invoke_errors_map() {
        assert_eq!(
            SysError::from(InvokeError::Cap(CapError::Empty)),
            SysError::CapEmpty
        );
        assert_eq!(
            SysError::from(InvokeError::Cap(CapError::OutOfRange)),
            SysError::CapOutOfRange
        );
        assert_eq!(
            SysError::from(InvokeError::WrongObject),
            SysError::WrongObject
        );
        assert_eq!(
            SysError::from(InvokeError::OutOfMemory),
            SysError::OutOfMemory
        );
    }
}
