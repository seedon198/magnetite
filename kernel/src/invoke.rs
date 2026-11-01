//! Kernel object invocation (#57, #82): the methods a capability names, and
//! the path a ring-3 `Call` on a non-endpoint capability takes
//! ([ADR 0016](../../docs/adr/0016-capability-implementation.md),
//! [ADR 0019](../../docs/adr/0019-first-user-task.md) decision 6).

use capability::abi::kind_code;
use capability::{CapError, CapId, InvokeOk, Method, ObjectKind, SysError, MSG_REGISTERS};
use sched::ThreadId;

/// The kernel-side capability invocation entry (#57): resolve the invoked
/// capability in `cspace`, authorize it (kind + rights), then perform the
/// method's effect.
///
/// CNode methods act on the acting `cspace`: [`invoke_current`] only lets a
/// task name its own CNode.
pub fn invoke(
    cspace: &mut capability::CapSpace<capability::ObjectRef>,
    invoked: capability::CapId,
    method: capability::Method,
    hhdm: u64,
) -> Result<capability::InvokeOk, capability::InvokeError> {
    use capability::{Capability, InvokeError, InvokeOk, Method, ObjectRef, Rights};

    let cap = cspace.get(invoked).map_err(InvokeError::Cap)?;
    method.authorize(&cap)?;

    match method {
        Method::CNodeCopy { src, dst, rights } => {
            cspace.copy(src, dst, rights).map_err(InvokeError::Cap)?;
            Ok(InvokeOk::Done)
        }
        Method::CNodeMint {
            src,
            dst,
            rights,
            badge,
        } => {
            cspace
                .mint(src, dst, rights, badge)
                .map_err(InvokeError::Cap)?;
            Ok(InvokeOk::Done)
        }
        Method::CNodeMove { src, dst } => {
            cspace.move_cap(src, dst).map_err(InvokeError::Cap)?;
            Ok(InvokeOk::Done)
        }
        Method::CNodeDelete { id } => {
            cspace.delete(id).map_err(InvokeError::Cap)?;
            Ok(InvokeOk::Done)
        }
        Method::FrameIdentify => Ok(InvokeOk::Phys(cap.object.phys)),
        Method::NotificationSignal { bits } => {
            hal::object::notification_signal(cap.object.phys, bits, hhdm);
            Ok(InvokeOk::Done)
        }
        Method::UntypedRetype { kind, count, dst } => {
            let (size, align) = kind.fixed_layout().ok_or(InvokeError::Unsupported)?;
            // Check the destination before taking any memory: a refused retype
            // must leave the Untyped as it was.
            match cspace.get(dst) {
                Err(CapError::Empty) => {}
                Ok(_) => return Err(InvokeError::Cap(CapError::Occupied)),
                Err(e) => return Err(InvokeError::Cap(e)),
            }
            // The Untyped capability points at its control block (an `Untyped`).
            let mut ut = hal::object::load_untyped(cap.object.phys, hhdm);
            let base = hal::object::retype(&mut ut, size, align, count, hhdm)
                .ok_or(InvokeError::OutOfMemory)?;
            hal::object::store_untyped(cap.object.phys, hhdm, ut);
            let obj = ObjectRef::new(kind, base);
            cspace
                .insert_at(dst, Capability::new(obj, Rights::ALL))
                .map_err(InvokeError::Cap)?;
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
    crate::ipc::with_cspace(t, |cs, own| {
        let cap = cs.get(slot).map_err(|_| SysError::InvalidCap)?;
        method.authorize(&cap)?;
        if method.target_kind() == ObjectKind::CNode && cap.object != own {
            return Err(SysError::Unsupported);
        }
        Ok(match invoke(cs, slot, method, hhdm)? {
            InvokeOk::Done | InvokeOk::Created(_) => [0; MSG_REGISTERS],
            InvokeOk::Phys(_) => [kind_code(ObjectKind::Frame), hal::paging::PAGE, 0, 0],
        })
    })
    .unwrap_or(Err(SysError::InvalidCap))
}
