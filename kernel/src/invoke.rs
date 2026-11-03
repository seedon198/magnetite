//! Kernel object invocation (#57, #82): the methods a capability names, and
//! the path a ring-3 `Call` on a non-endpoint capability takes
//! ([ADR 0016](../../docs/adr/0016-capability-implementation.md),
//! [ADR 0019](../../docs/adr/0019-first-user-task.md) decision 6).

use ::ipc::Ipc;
use capability::abi::kind_code;
use capability::{
    mdb, CapError, CapId, InvokeError, InvokeOk, Method, ObjectKind, SlotStore, SysError,
    MSG_REGISTERS,
};
use sched::ThreadId;

use crate::cspace::{self, Registry};

/// The kernel-side capability invocation entry (#57): resolve the invoked
/// capability in CNode `cnode` of the registry, authorize it (kind + rights),
/// then perform the method's effect. Every change to a slot goes through the
/// mapping database (ADR 0020): a copy, a mint and a retyped object become
/// children of the capability they came from, and a delete or revoke
/// destroys each object whose last capability it removes. Call with `IPC`
/// and `CSPACES` held (`ipc::with_caps`).
///
/// CNode methods act on the acting CNode `cnode`: [`invoke_current`] only
/// lets a task name its own CNode.
pub fn invoke(
    ipc: &mut Ipc,
    reg: &mut Registry,
    cnode: u16,
    invoked: CapId,
    method: Method,
    hhdm: u64,
) -> Result<InvokeOk, InvokeError> {
    use capability::{Capability, ObjectRef, Rights, Untyped};

    let slot = |reg: &Registry, id| reg.slot_in(cnode, id).map_err(InvokeError::Cap);
    let at = slot(reg, invoked)?;
    let cap = reg.cap(at).map_err(InvokeError::Cap)?;
    method.authorize(&cap)?;

    match method {
        Method::CNodeCopy { src, dst, rights } => {
            let (src, dst) = (slot(reg, src)?, slot(reg, dst)?);
            mdb::copy(reg, src, dst, rights).map_err(InvokeError::Cap)?;
            Ok(InvokeOk::Done)
        }
        Method::CNodeMint {
            src,
            dst,
            rights,
            badge,
        } => {
            let (src, dst) = (slot(reg, src)?, slot(reg, dst)?);
            mdb::mint(reg, src, dst, rights, badge).map_err(InvokeError::Cap)?;
            Ok(InvokeOk::Done)
        }
        Method::CNodeMove { src, dst } => {
            let (src, dst) = (slot(reg, src)?, slot(reg, dst)?);
            mdb::move_entry(reg, src, dst).map_err(|e| InvokeError::Cap(e.into()))?;
            Ok(InvokeOk::Done)
        }
        Method::CNodeDelete { id } => {
            let id = slot(reg, id)?;
            cspace::delete(ipc, reg, id).map_err(InvokeError::Cap)?;
            Ok(InvokeOk::Done)
        }
        Method::CNodeRevoke { id } => {
            let id = slot(reg, id)?;
            if cspace::revoke(ipc, reg, id).map_err(InvokeError::Cap)? {
                Ok(InvokeOk::Done)
            } else {
                Err(InvokeError::Again)
            }
        }
        Method::FrameIdentify => Ok(InvokeOk::Phys(cap.object.phys)),
        Method::NotificationSignal { bits } => {
            crate::ipc::signal(ipc, cap.object, bits);
            Ok(InvokeOk::Done)
        }
        Method::UntypedRetype { kind, count, dst } => {
            let (size, align) = kind.fixed_layout().ok_or(InvokeError::Unsupported)?;
            // Check the destination before taking any memory: a refused retype
            // must leave the Untyped as it was.
            let dst = slot(reg, dst)?;
            if reg.get(dst).is_some() {
                return Err(InvokeError::Cap(CapError::Occupied));
            }
            // The Untyped capability points at its control block (an `Untyped`).
            let mut ut = hal::object::load_untyped(cap.object, hhdm);
            let base = hal::object::retype(&mut ut, size, align, count, hhdm)
                .ok_or(InvokeError::OutOfMemory)?;
            hal::object::store_untyped(cap.object, hhdm, ut);
            let obj = ObjectRef::new(kind, base);
            mdb::insert_child(reg, at, dst, Capability::new(obj, Rights::ALL))
                .map_err(|e| InvokeError::Cap(e.into()))?;
            Ok(InvokeOk::Created(obj))
        }
        Method::UntypedSplit { size, dst } => {
            let dst = slot(reg, dst)?;
            if reg.get(dst).is_some() {
                return Err(InvokeError::Cap(CapError::Occupied));
            }
            let unit = Untyped::CHILD_UNIT;
            let mut ut = hal::object::load_untyped(cap.object, hhdm);
            let base = hal::object::retype(&mut ut, size, unit, 1, hhdm)
                .ok_or(InvokeError::OutOfMemory)?;
            let child = Untyped::child(base, size).ok_or(InvokeError::Unsupported)?;
            hal::object::store_untyped(cap.object, hhdm, ut);
            // The child's control block heads its own storage.
            let obj = ObjectRef::new(ObjectKind::Untyped, base);
            hal::object::store_untyped(obj, hhdm, child);
            mdb::insert_child(reg, at, dst, Capability::new(obj, Rights::ALL))
                .map_err(|e| InvokeError::Cap(e.into()))?;
            Ok(InvokeOk::Created(obj))
        }
    }
}

/// Invoke `method` on the capability in slot `slot` of thread `t`'s capability
/// space, for a ring-3 `Call`. Returns the result words for the message
/// registers. No physical address is ever returned: a retype answers only
/// success, and identifying a frame gives its kind and size.
///
/// # Errors
/// [`SysError::InvalidCap`] if the slot itself is empty or out of range (as
/// for IPC); `Unsupported` for a CNode other than the task's own; otherwise
/// the method's own error.
pub fn invoke_current(
    t: ThreadId,
    slot: CapId,
    method: Method,
) -> Result<[u64; MSG_REGISTERS], SysError> {
    let hhdm = crate::ipc::hhdm();
    crate::ipc::with_caps(|ipc, reg| {
        let c = reg.of(t).ok_or(SysError::InvalidCap)?;
        let cap = reg.lookup(t, slot).map_err(|_| SysError::InvalidCap)?;
        method.authorize(&cap)?;
        if method.target_kind() == ObjectKind::CNode && cap.object != reg.object(c) {
            return Err(SysError::Unsupported);
        }
        Ok(match invoke(ipc, reg, c, slot, method, hhdm)? {
            InvokeOk::Done | InvokeOk::Created(_) => [0; MSG_REGISTERS],
            InvokeOk::Phys(_) => [kind_code(ObjectKind::Frame), hal::paging::PAGE, 0, 0],
        })
    })
}
