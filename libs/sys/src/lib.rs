#![no_std]
#![deny(unsafe_code)]
#![deny(missing_docs)]

//! The user side of the system-call ABI (#82,
//! [ADR 0019](../../docs/adr/0019-first-user-task.md)).
//!
//! Every user program links this crate. It holds the only `unsafe` a user
//! program needs: the `syscall` instruction, the `_start` entry and a raw
//! read for the fault test, all in [`arch`] (ADR 0007's audited exception).
//! A program is `#![forbid(unsafe_code)]` and names its entry with
//! [`entry!`]. The ABI itself (numbers, error codes, labels) is shared with
//! the kernel through `capability::abi`.

mod arch;

pub use capability::abi::{kind_code, label, sys as number, SysError, NO_SLOT};
pub use capability::object::ObjectKind;
pub use capability::{MessageInfo, MSG_REGISTERS};

/// Slot 0: always empty; the kernel refuses it.
pub const NULL: u64 = 0;
/// The root task's own CNode (`READ`, `WRITE`).
pub const CNODE: u64 = 1;
/// The root task's Untyped memory, 64 KiB (`RETYPE`).
pub const UNTYPED: u64 = 2;
/// The console endpoint (badged, `SEND` only).
pub const CONSOLE: u64 = 3;
/// Slots in the root task's CNode.
pub const ROOT_SLOTS: u64 = 64;

/// What a receive or a call returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Received {
    /// The sender's badge.
    pub badge: u64,
    /// Label, length and capability count.
    pub info: MessageInfo,
    /// The message registers.
    pub mr: [u64; MSG_REGISTERS],
}

/// One raw system call. `send_slot` is the capability to send when
/// `info.caps()` is 1; `recv_slot` receives one ([`NO_SLOT`] for none).
pub fn syscall(
    number: u64,
    cap: u64,
    info: MessageInfo,
    mr: [u64; MSG_REGISTERS],
    recv_slot: u64,
    send_slot: u64,
) -> Result<Received, SysError> {
    let (status, badge, info, mr) =
        arch::syscall(number, cap, info.bits(), mr, recv_slot, send_slot);
    match status {
        0 => Ok(Received {
            badge,
            info: MessageInfo::from_bits(info),
            mr,
        }),
        code => Err(SysError::from_code(code).unwrap_or(SysError::InvalidSyscall)),
    }
}

/// Send through the endpoint in `ep`, with the capability in `cap_slot` if
/// `info.caps()` is 1. Blocks until received.
pub fn send(
    ep: u64,
    info: MessageInfo,
    mr: [u64; MSG_REGISTERS],
    cap_slot: u64,
) -> Result<(), SysError> {
    syscall(number::SEND, ep, info, mr, NO_SLOT, cap_slot).map(|_| ())
}

/// Receive from the endpoint in `ep`; an incoming capability lands in
/// `recv_slot`.
pub fn recv(ep: u64, recv_slot: u64) -> Result<Received, SysError> {
    syscall(
        number::RECV,
        ep,
        MessageInfo::new(0, 0, 0),
        [0; 4],
        recv_slot,
        NULL,
    )
}

/// Send, then wait for the reply. On a capability that is not an endpoint,
/// invoke a kernel object method instead (see [`invoke`]).
pub fn call(
    cap: u64,
    info: MessageInfo,
    mr: [u64; MSG_REGISTERS],
    recv_slot: u64,
) -> Result<Received, SysError> {
    syscall(number::CALL, cap, info, mr, recv_slot, NULL)
}

/// Answer the caller in this thread's reply slot.
pub fn reply(info: MessageInfo, mr: [u64; MSG_REGISTERS]) -> Result<(), SysError> {
    syscall(number::REPLY, NULL, info, mr, NO_SLOT, NULL).map(|_| ())
}

/// Reply (if a caller waits), then receive from `ep`.
pub fn reply_recv(
    ep: u64,
    info: MessageInfo,
    mr: [u64; MSG_REGISTERS],
    recv_slot: u64,
) -> Result<Received, SysError> {
    syscall(number::REPLY_RECV, ep, info, mr, recv_slot, NULL)
}

/// Give up the processor.
pub fn yield_now() {
    let _ = syscall(
        number::YIELD,
        NULL,
        MessageInfo::new(0, 0, 0),
        [0; 4],
        NO_SLOT,
        NULL,
    );
}

/// End this thread.
pub fn exit() -> ! {
    let _ = syscall(
        number::EXIT,
        NULL,
        MessageInfo::new(0, 0, 0),
        [0; 4],
        NO_SLOT,
        NULL,
    );
    unreachable!("sys: exit returned")
}

/// Invoke method `label` on the kernel object in `slot` with up to four
/// argument words; returns the result words.
///
/// # Errors
/// [`SysError::InvalidSyscall`], without entering the kernel, if `args` has
/// more words than the ABI's message registers carry: dropping the rest would
/// make the kernel see a different request (#135). Otherwise the kernel's
/// answer.
pub fn invoke(slot: u64, label: u32, args: &[u64]) -> Result<[u64; MSG_REGISTERS], SysError> {
    if args.len() > MSG_REGISTERS {
        return Err(SysError::InvalidSyscall);
    }
    let mut mr = [0; MSG_REGISTERS];
    let n = args.len();
    mr[..n].copy_from_slice(args);
    call(slot, MessageInfo::new(label, n as u8, 0), mr, NO_SLOT).map(|r| r.mr)
}

/// Write `bytes` to the console endpoint in `console`, in calls of at most
/// 32 bytes (label = byte count, message registers = the bytes). The server
/// prints a line at each `\n`.
pub fn write(console: u64, bytes: &[u8]) -> Result<(), SysError> {
    for chunk in bytes.chunks(8 * MSG_REGISTERS) {
        let mut mr = [0u64; MSG_REGISTERS];
        for (i, b) in chunk.iter().enumerate() {
            mr[i / 8] |= u64::from(*b) << (8 * (i % 8));
        }
        let info = MessageInfo::new(chunk.len() as u32, MSG_REGISTERS as u8, 0);
        call(console, info, mr, NO_SLOT)?;
    }
    Ok(())
}

/// Write `s` to the console endpoint in `console`.
pub fn print(console: u64, s: &str) -> Result<(), SysError> {
    write(console, s.as_bytes())
}

/// Debugging aids for the kernel's confinement tests.
pub mod debug {
    /// Read the 8 bytes at `addr`. For tests that must fault: any address the
    /// program does not own ends it.
    pub fn read_u64(addr: u64) -> u64 {
        crate::arch::read_u64(addr)
    }
}

/// Name a program's entry point: `sys::entry!(main)` with `fn main() -> !`.
/// The expansion provides the symbol `_start` calls, so the program itself
/// stays `#![forbid(unsafe_code)]`.
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        #[unsafe(no_mangle)]
        extern "C" fn __magnetite_main() -> ! {
            let main: fn() -> ! = $main;
            main()
        }
    };
}

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    let _ = print(CONSOLE, "panic\n");
    exit()
}
