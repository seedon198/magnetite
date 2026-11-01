//! Thread context switching.
//!
//! A [`Context`] holds a saved kernel stack pointer; [`switch`] saves the
//! current thread's callee-saved registers and `rsp` into one context and
//! restores another's, so execution resumes where that thread last switched
//! away (ADR 0017). A freshly [`init`](Context::init)ed context begins at a
//! chosen entry point on its own stack the first time it is switched to.
//!
//! The scheduling *policy* is the pure [`sched`](../../libs/sched) crate; this
//! is the hardware half, and the only `unsafe` is the register save/restore.

use core::arch::global_asm;
use core::sync::atomic::{AtomicU64, Ordering};

/// A saved thread context: just the kernel stack pointer. The callee-saved
/// registers and return address live on that stack between switches.
#[repr(C)]
pub struct Context {
    rsp: AtomicU64,
}

impl Context {
    /// An empty context. Its `rsp` is filled the first time execution switches
    /// *away* from the thread (for an already-running thread, e.g. the boot
    /// thread), or by [`init`](Self::init) (for a fresh thread).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            rsp: AtomicU64::new(0),
        }
    }

    /// The saved stack pointer.
    #[must_use]
    pub fn rsp(&self) -> u64 {
        self.rsp.load(Ordering::Relaxed)
    }

    /// Prepare a fresh thread: when first switched to, it begins executing
    /// `entry` on the stack whose top (highest address) is `stack_top`, with
    /// interrupts **enabled** (so it can be preempted, #80).
    ///
    /// The caller must own at least 64 bytes of stack below `stack_top`, mapped
    /// writable; `entry` must never return (it runs as a thread body).
    pub fn init(&self, stack_top: u64, entry: extern "C" fn() -> !) {
        // Build the frame `switch` expects: six callee-saved registers then the
        // return address. The "return" goes to `thread_start`, which enables
        // interrupts and jumps to `entry` (parked in the r12 slot): switches
        // happen with interrupts disabled, and a fresh thread has no saved
        // interrupt flag of its own to restore. `frame` is 16-aligned so that
        // after `switch` pops the six registers and `ret`s, `rsp % 16 == 8` -
        // and `thread_start` `jmp`s (no push), so `entry` sees the SysV ABI's
        // function-entry alignment.
        let frame = (stack_top - 7 * 8) & !0xf;
        let base = frame as *mut u64;
        // SAFETY: the caller guarantees `[frame, stack_top)` is writable stack
        // memory they own; we initialize exactly seven words within it.
        unsafe {
            for i in 0..6 {
                base.add(i).write(0); // r15, r14, r13, r12, rbx, rbp
            }
            base.add(3).write(entry as usize as u64); // r12: the thread body
            base.add(6).write(thread_start as *const () as u64); // return address
        }
        self.rsp.store(frame, Ordering::Relaxed);
    }
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

/// Switch from `prev` to `next`: save the current callee-saved registers and
/// stack pointer into `prev`, then restore `next`'s and resume it. Returns to
/// the caller only when some later switch selects `prev` again.
///
/// Call with interrupts disabled: once preemption exists (#80), a timer tick
/// between choosing `next` and switching to it would reschedule underneath the
/// caller. The resumed thread restores its own interrupt state (on return from
/// its own `switch` caller, or via `iretq` if it was preempted).
pub fn switch(prev: &Context, next: &Context) {
    let next_rsp = next.rsp.load(Ordering::Acquire);
    // SAFETY: `prev.rsp` is a valid, aligned `AtomicU64` we may store the saved
    // stack pointer into; `next_rsp` is a stack pointer previously saved by this
    // same routine (or crafted by `init`) that resumes a valid callee-saved
    // frame. The asm touches only the two stacks and callee-saved registers.
    unsafe {
        context_switch(prev.rsp.as_ptr(), next_rsp);
    }
}

extern "C" {
    /// `context_switch(save_rsp_to: *mut u64, load_rsp: u64)` - see the asm.
    fn context_switch(save_rsp_to: *mut u64, load_rsp: u64);
    /// First code a fresh thread runs (never called directly) - see the asm.
    fn thread_start() -> !;
}

// Save callee-saved registers on the current stack, store the resulting rsp
// through `rdi`, load `rsi` as the new rsp, restore callee-saved registers, and
// `ret` into the resumed thread. Pop order mirrors the push order reversed, so
// `init` lays the frame out as [r15, r14, r13, r12, rbx, rbp, return-addr].
global_asm!(
    ".global context_switch",
    ".p2align 4",
    "context_switch:",
    "    push rbp",
    "    push rbx",
    "    push r12",
    "    push r13",
    "    push r14",
    "    push r15",
    "    mov [rdi], rsp",
    "    mov rsp, rsi",
    "    pop r15",
    "    pop r14",
    "    pop r13",
    "    pop r12",
    "    pop rbx",
    "    pop rbp",
    "    ret",
);

// A fresh thread's first "return" lands in `thread_start` with its entry point
// in r12 (see `Context::init`). Enable interrupts - the switch that got us here
// ran with them disabled - and tail-jump into the body.
global_asm!(
    ".global thread_start",
    ".p2align 4",
    "thread_start:",
    "    sti",
    "    jmp r12",
);
