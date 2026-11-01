//! The system-call dispatcher (#82,
//! [ADR 0019](../../docs/adr/0019-first-user-task.md) decision 6).
//!
//! Every syscall from ring 3 lands in [`handle`], which decodes the registers
//! through `capability::abi`, runs IPC or a kernel-object method on the
//! calling thread's capability space, and writes the results back for the
//! return to ring 3. Nothing a task passes can make it panic: every refusal is
//! an error code in `rax`.

use ::ipc::{IpcError, Message, Received};
use capability::abi::{self, sys, SysError, NO_SLOT};
use capability::{CapId, MessageInfo, ObjectKind, MSG_REGISTERS};
use hal::syscall::SyscallArgs;
use sched::ThreadId;

use crate::{invoke, ipc, user, SCHED};

/// Handle one system call from ring 3; returns `rax` (0 or an error code).
pub fn handle(args: &mut SyscallArgs) -> u64 {
    let t =
        hal::cpu::without_interrupts(|| SCHED.lock().current()).expect("syscall: a running thread");
    if let Some(status) = user::debug_syscall(t, args) {
        return status;
    }
    match dispatch(t, args) {
        Ok(()) => 0,
        Err(e) => e.code(),
    }
}

fn dispatch(t: ThreadId, args: &mut SyscallArgs) -> Result<(), SysError> {
    let msg = Message {
        info: MessageInfo::from_bits(args.msg_info),
        mr: args.mr,
    };
    match args.number {
        sys::YIELD => {
            crate::yield_now();
            Ok(())
        }
        sys::EXIT => crate::exit_current(),
        sys::REPLY => ipc::reply(msg, send_slot(args, msg)?).map_err(ipc_error),
        sys::SEND | sys::RECV | sys::CALL | sys::REPLY_RECV => {
            let slot = abi::slot(args.cap)?;
            let kind = ipc::with_cspace(t, |cs, _| cs.get(slot).map(|c| c.object.kind))
                .and_then(Result::ok)
                .ok_or(SysError::InvalidCap)?;
            if kind != ObjectKind::Endpoint {
                if args.number != sys::CALL {
                    return Err(SysError::InvalidCap);
                }
                let method = abi::decode_method(msg.info, msg.mr)?;
                args.mr = invoke::invoke_current(t, slot, method)?;
                args.cap = 0;
                args.msg_info = MessageInfo::new(0, MSG_REGISTERS as u8, 0).bits();
                return Ok(());
            }
            let received = match args.number {
                sys::SEND => {
                    ipc::send(slot, msg, send_slot(args, msg)?).map_err(ipc_error)?;
                    return Ok(());
                }
                sys::RECV => ipc::recv(slot, recv_slot(args)?),
                sys::CALL => ipc::call(slot, msg, send_slot(args, msg)?, recv_slot(args)?),
                _ => ipc::reply_recv(slot, msg, send_slot(args, msg)?, recv_slot(args)?),
            }
            .map_err(ipc_error)?;
            deliver(args, received);
            Ok(())
        }
        _ => Err(SysError::InvalidSyscall),
    }
}

/// The capability to send (`r13`), when the message carries one.
fn send_slot(args: &SyscallArgs, msg: Message) -> Result<Option<CapId>, SysError> {
    match msg.info.caps() {
        0 => Ok(None),
        _ => abi::slot(args.send_slot).map(Some),
    }
}

/// The receive slot (`r12`); [`NO_SLOT`] refuses an incoming capability.
fn recv_slot(args: &SyscallArgs) -> Result<Option<CapId>, SysError> {
    match args.recv_slot {
        NO_SLOT => Ok(None),
        raw => abi::slot(raw).map(Some),
    }
}

/// Hand a received message back to ring 3: badge in `rdi`, message info in
/// `rsi`, message registers in `rdx`, `r8` to `r10`.
fn deliver(args: &mut SyscallArgs, r: Received) {
    args.cap = r.badge;
    args.msg_info = r.info.bits();
    args.mr = r.mr;
}

fn ipc_error(e: IpcError) -> SysError {
    e.into()
}
