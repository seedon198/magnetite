//! Kernel object invocation (#57, #82): the methods a capability names, and
//! the path a ring-3 `Call` on a non-endpoint capability takes
//! ([ADR 0016](../../docs/adr/0016-capability-implementation.md),
//! [ADR 0019](../../docs/adr/0019-first-user-task.md) decision 6).

use ::ipc::Ipc;
use capability::abi::kind_code;
use capability::{
    cnode_bytes, mdb, slot_align, CapError, CapId, InvokeError, InvokeOk, Method, ObjectKind,
    SlotStore, SysError, MSG_REGISTERS,
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
/// A CNode method addresses two CNodes: a `src` slot is in the acting CNode
/// `cnode`, and a `dst` slot, or the slot a delete or revoke names, is in the
/// CNode the invoked capability names (ADR 0023).
pub fn invoke(
    ipc: &mut Ipc,
    reg: &mut Registry,
    cnode: u16,
    invoked: CapId,
    method: Method,
    hhdm: u64,
) -> Result<InvokeOk, InvokeError> {
    use capability::{Capability, Mapping, ObjectRef, Rights, Untyped};

    let slot = |reg: &Registry, id| reg.slot_in(cnode, id).map_err(InvokeError::Cap);
    let at = slot(reg, invoked)?;
    let cap = reg.cap(at).map_err(InvokeError::Cap)?;
    method.authorize(&cap)?;
    // The CNode a CNode method's destination slots are in.
    let target = |reg: &Registry, id| {
        let c = reg.index_of(cap.object).ok_or(InvokeError::Unsupported)?;
        reg.slot_in(c, id).map_err(InvokeError::Cap)
    };

    match method {
        Method::CNodeCopy { src, dst, rights } => {
            let (src, dst) = (slot(reg, src)?, target(reg, dst)?);
            cspace::copy(reg, src, dst, rights).map_err(InvokeError::Cap)?;
            Ok(InvokeOk::Done)
        }
        Method::CNodeMint {
            src,
            dst,
            rights,
            badge,
        } => {
            let (src, dst) = (slot(reg, src)?, target(reg, dst)?);
            cspace::mint(reg, src, dst, rights, badge).map_err(InvokeError::Cap)?;
            Ok(InvokeOk::Done)
        }
        Method::CNodeMove { src, dst } => {
            let (src, dst) = (slot(reg, src)?, target(reg, dst)?);
            mdb::move_entry(reg, src, dst).map_err(|e| InvokeError::Cap(e.into()))?;
            Ok(InvokeOk::Done)
        }
        Method::CNodeDelete { id } => {
            let id = target(reg, id)?;
            cspace::delete(ipc, reg, id).map_err(InvokeError::Cap)?;
            Ok(InvokeOk::Done)
        }
        Method::CNodeRevoke { id } => {
            let id = target(reg, id)?;
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
            let space = kind == ObjectKind::AddressSpace;
            let thread = kind == ObjectKind::Thread;
            if (space || thread) && count != 1 {
                return Err(InvokeError::Unsupported);
            }
            if (space && !crate::vspace::has_free())
                || (thread && !crate::thread::has_free_object_slot())
            {
                return Err(InvokeError::OutOfMemory);
            }
            // The Untyped capability points at its control block (an `Untyped`).
            let mut ut = hal::object::load_untyped(cap.object, hhdm);
            let base = hal::object::retype(&mut ut, size, align, count, hhdm)
                .ok_or(InvokeError::OutOfMemory)?;
            let obj = ObjectRef::new(kind, base);
            // A Thread's memory becomes a slot's kernel stack; if that fails
            // the Untyped is left as it was.
            if thread && crate::thread::bind_object(obj).is_none() {
                return Err(InvokeError::OutOfMemory);
            }
            hal::object::store_untyped(cap.object, hhdm, ut);
            if space {
                // A new address space has the kernel half, and an ASID.
                hal::cpu::without_interrupts(|| {
                    let kernel = crate::KERNEL_SPACE.lock();
                    let kernel = kernel.as_ref().expect("invoke: the kernel space");
                    hal::vspace::init_root(obj, kernel, hhdm);
                });
                crate::vspace::alloc(obj).expect("invoke: an ASID was free");
            }
            mdb::insert_child(reg, at, dst, Capability::new(obj, Rights::ALL))
                .map_err(|e| InvokeError::Cap(e.into()))?;
            Ok(InvokeOk::Created(obj))
        }
        Method::UntypedRetypeCNode { slots, dst } => {
            let dst = slot(reg, dst)?;
            if reg.get(dst).is_some() {
                return Err(InvokeError::Cap(CapError::Occupied));
            }
            // Refuse before taking memory: a full registry cannot hold it.
            if reg.is_full() {
                return Err(InvokeError::OutOfMemory);
            }
            let slots = usize::try_from(slots).map_err(|_| InvokeError::Unsupported)?;
            let mut ut = hal::object::load_untyped(cap.object, hhdm);
            let (size, align) = (cnode_bytes(slots) as u64, slot_align() as u64);
            let base = hal::object::retype(&mut ut, size, align, 1, hhdm)
                .ok_or(InvokeError::OutOfMemory)?;
            hal::object::store_untyped(cap.object, hhdm, ut);
            let obj = ObjectRef::new(ObjectKind::CNode, base);
            let space = hal::object::cnode_cap_space(obj, slots, hhdm);
            let c = reg
                .register(obj, space)
                .expect("invoke: the registry had room");
            if let Err(e) = mdb::insert_child(reg, at, dst, Capability::new(obj, Rights::ALL)) {
                cspace::clear(ipc, reg, c);
                return Err(InvokeError::Cap(e.into()));
            }
            Ok(InvokeOk::Created(obj))
        }
        Method::AddressSpaceMapTable { table, vaddr } => {
            let t = slot(reg, table)?;
            let mut e = reg.entry(t).ok_or(InvokeError::Cap(CapError::Empty))?;
            if e.cap.object.kind != ObjectKind::PageTable {
                return Err(InvokeError::WrongObject);
            }
            if crate::vspace::mapping(&e.cap).is_some() {
                return Err(InvokeError::AlreadyMapped);
            }
            let (asid, generation) =
                crate::vspace::of(cap.object).ok_or(InvokeError::Unsupported)?;
            let level = hal::vspace::install_table(cap.object, vaddr, e.cap.object, hhdm)
                .map_err(vspace_error)?;
            let m = Mapping {
                asid,
                generation,
                vpn: vaddr >> 12,
                level,
            };
            e.cap.badge = m.pack().expect("invoke: a user-half mapping packs");
            reg.set(t, Some(e));
            Ok(InvokeOk::Done)
        }
        Method::AddressSpaceMapFrame {
            frame,
            vaddr,
            rights,
        } => {
            let f = slot(reg, frame)?;
            let mut e = reg.entry(f).ok_or(InvokeError::Cap(CapError::Empty))?;
            if e.cap.object.kind != ObjectKind::Frame {
                return Err(InvokeError::WrongObject);
            }
            // Within the frame capability's rights, readable, and never both
            // writable and executable (ADR 0012).
            let wx = Rights::WRITE | Rights::EXECUTE;
            if !e.cap.allows(rights) || !rights.contains(Rights::READ) || rights.contains(wx) {
                return Err(InvokeError::Cap(CapError::NotPermitted));
            }
            if crate::vspace::mapping(&e.cap).is_some() {
                return Err(InvokeError::AlreadyMapped);
            }
            let (asid, generation) =
                crate::vspace::of(cap.object).ok_or(InvokeError::Unsupported)?;
            let mut flags = hal::paging::Flags::empty();
            if rights.contains(Rights::WRITE) {
                flags = flags.union(hal::paging::Flags::WRITABLE);
            }
            if !rights.contains(Rights::EXECUTE) {
                flags = flags.union(hal::paging::Flags::NO_EXECUTE);
            }
            hal::vspace::map_frame(cap.object, vaddr, e.cap.object, flags, hhdm)
                .map_err(vspace_error)?;
            let m = Mapping {
                asid,
                generation,
                vpn: vaddr >> 12,
                level: 0,
            };
            e.cap.badge = m.pack().expect("invoke: a user-half mapping packs");
            reg.set(f, Some(e));
            Ok(InvokeOk::Done)
        }
        Method::FrameUnmap => {
            crate::vspace::unmap(&cap, hhdm);
            let mut e = reg.entry(at).expect("invoke: the invoked capability");
            e.cap.badge = 0;
            reg.set(at, Some(e));
            Ok(InvokeOk::Done)
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

/// A mapping refusal as an invocation error.
fn vspace_error(e: hal::vspace::VspaceError) -> InvokeError {
    match e {
        hal::vspace::VspaceError::MissingTable => InvokeError::MissingTable,
        hal::vspace::VspaceError::AlreadyMapped => InvokeError::AlreadyMapped,
        hal::vspace::VspaceError::BadAddress => InvokeError::Unsupported,
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
        Ok(match invoke(ipc, reg, c, slot, method, hhdm)? {
            InvokeOk::Done | InvokeOk::Created(_) => [0; MSG_REGISTERS],
            InvokeOk::Phys(_) => [kind_code(ObjectKind::Frame), hal::paging::PAGE, 0, 0],
        })
    })
}
