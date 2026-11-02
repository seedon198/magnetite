//! Stack backtraces via frame-pointer walking.
//!
//! The kernel is compiled with `-C force-frame-pointers=yes`, so each function
//! prologue sets up the standard chain: at `[rbp]` is the caller's saved `rbp`,
//! and at `[rbp + 8]` is the return address. Walking that chain yields the call
//! stack.
//!
//! The walk trusts no link: each frame is probed with
//! [`crate::fault::probe_read`] before it is read, so a corrupt, unmapped or
//! user-chosen `rbp` ends the walk instead of faulting inside the crash
//! reporter (#111). Every kernel entry (the syscall entry and the IDT common
//! stub) zeroes `rbp` before calling Rust, so a chain ends there.
//!
//! Addresses are printed raw. Resolve them to source locations with, e.g.:
//! `llvm-addr2line -e target/x86_64-unknown-none/debug/kernel <addr>`.
//! (Embedding a symbol table for in-kernel symbolization is a later stretch.)

use core::arch::asm;
use core::fmt::Write;

/// Maximum number of frames to print, to bound a walk over a corrupt stack.
const MAX_FRAMES: usize = 32;

/// Print a backtrace starting from the current frame.
pub fn print(w: &mut impl Write) {
    let rbp: u64;
    // SAFETY: reads the frame-base register; no memory access, no side effects.
    unsafe {
        asm!("mov {}, rbp", out(reg) rbp, options(nomem, nostack, preserves_flags));
    }
    print_from(w, rbp);
}

/// Print a backtrace starting from a given frame-base pointer (e.g. a trap
/// frame's `rbp`). Returns the number of frames printed.
pub fn print_from(w: &mut impl Write, rbp: u64) -> usize {
    let _ = writeln!(w, "backtrace (most recent call first):");
    let mut frames = 0;
    walk(rbp, |frame, return_addr| {
        let _ = writeln!(w, "  #{frame:<2} {return_addr:#018x}");
        frames += 1;
    });
    frames
}

/// The frame link that ends the current call chain: 0 when the chain ends at a
/// kernel entry, as it must (#111). The user-mode demo checks it from inside a
/// syscall made with a user-chosen `rbp`.
#[must_use]
pub fn chain_end() -> u64 {
    let rbp: u64;
    // SAFETY: reads the frame-base register; no memory access, no side effects.
    unsafe {
        asm!("mov {}, rbp", out(reg) rbp, options(nomem, nostack, preserves_flags));
    }
    walk(rbp, |_, _| {})
}

/// Walk the frame chain from `rbp`, calling `f` with each frame's index and
/// return address, for at most [`MAX_FRAMES`] frames. Returns the link that
/// ended the walk.
fn walk(mut rbp: u64, mut f: impl FnMut(usize, u64)) -> u64 {
    for frame in 0..MAX_FRAMES {
        let Some((saved_rbp, return_addr)) = read_frame(rbp) else {
            return rbp;
        };
        if return_addr == 0 {
            return rbp;
        }
        f(frame, return_addr);
        // Frame pointers must increase as we move toward the stack base;
        // anything else means a corrupt or finished chain.
        if saved_rbp <= rbp {
            return saved_rbp;
        }
        rbp = saved_rbp;
    }
    rbp
}

/// The saved `rbp` and return address of the frame at `rbp`, or `None` if
/// `rbp` cannot be a kernel frame or either word is unmapped.
fn read_frame(rbp: u64) -> Option<(u64, u64)> {
    // A valid kernel frame pointer is 8-aligned and in the higher half.
    if rbp == 0 || rbp & 0x7 != 0 || !is_higher_half(rbp) {
        return None;
    }
    // Each 8-aligned word lies within one page, so probing its first byte
    // proves the whole word is mapped.
    if crate::fault::probe_read(rbp) || crate::fault::probe_read(rbp + 8) {
        return None;
    }
    // SAFETY: `rbp` is 8-aligned and in the higher half, and both words were
    // just probed as mapped, so these volatile reads cannot fault.
    Some(unsafe {
        (
            core::ptr::read_volatile(rbp as *const u64),
            core::ptr::read_volatile((rbp + 8) as *const u64),
        )
    })
}

/// Whether `addr` is a canonical higher-half address (kernel space).
fn is_higher_half(addr: u64) -> bool {
    addr >= 0xffff_8000_0000_0000
}
