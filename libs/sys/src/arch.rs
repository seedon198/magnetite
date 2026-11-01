//! The `syscall` instruction, the entry point and a raw read: the only
//! `unsafe` user programs reach (ADR 0007's audited exception for `libs/sys`).
#![allow(unsafe_code)]

use capability::MSG_REGISTERS;

/// Issue `syscall` with the ABI's registers; returns `rax` (the status),
/// `rdi` (badge), `rsi` (message info) and the message registers.
pub fn syscall(
    number: u64,
    cap: u64,
    info: u64,
    mr: [u64; MSG_REGISTERS],
    recv_slot: u64,
    send_slot: u64,
) -> (u64, u64, u64, [u64; MSG_REGISTERS]) {
    let (status, badge, info_out, m0, m1, m2, m3);
    // SAFETY: `syscall` enters the kernel, which preserves every register but
    // the outputs named here and `rcx`/`r11` (clobbered by the instruction).
    // It reads no memory of ours.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number => status,
            inlateout("rdi") cap => badge,
            inlateout("rsi") info => info_out,
            inlateout("rdx") mr[0] => m0,
            inlateout("r8") mr[1] => m1,
            inlateout("r9") mr[2] => m2,
            inlateout("r10") mr[3] => m3,
            in("r12") recv_slot,
            in("r13") send_slot,
            out("rcx") _,
            out("r11") _,
            options(nostack),
        );
    }
    (status, badge, info_out, [m0, m1, m2, m3])
}

/// Read 8 bytes at `addr`, which may fault (that is the point).
pub fn read_u64(addr: u64) -> u64 {
    // SAFETY: a read through an arbitrary address. In a user task a bad
    // address raises a page fault that ends the task; it never corrupts
    // anything.
    unsafe { core::ptr::read_volatile(addr as *const u64) }
}

// The entry point the linker script names. The kernel starts a task here with
// `rsp` at the top of its stack; align it, clear the frame pointer and call
// the program's `entry!` function, which never returns.
#[cfg(target_os = "none")]
core::arch::global_asm!(
    ".global _start",
    "_start:",
    "    xor ebp, ebp",
    "    and rsp, -16",
    "    call __magnetite_main",
    "    ud2",
);
