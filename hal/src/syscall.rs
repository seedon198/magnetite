//! The user→kernel system-call boundary and first ring-3 entry.
//!
//! Uses the `syscall`/`sysret` fast path: [`init`] programs the MSRs
//! (`EFER.SCE`, `STAR`, `LSTAR`, `FMASK`), [`syscall_entry`] is the kernel entry
//! stub, and [`enter_user_once`] drops to ring 3 via `iretq`. The
//! message-register ABI is [`capability::abi`]: `rdi` = invoked capability id,
//! `rsi` = packed `MessageInfo`, `rdx`/`r8`/`r9`/`r10` = message registers 0-3
//! (`rcx`/`r11` are reserved by the CPU for the return RIP/RFLAGS); `rax`
//! returns a status word.
//!
//! The entry stub switches to a dedicated kernel stack (single-core bring-up; a
//! per-CPU/GS scheme comes with SMP). A registered kernel [`Handler`] does the
//! work. See [ADR 0017](../../docs/adr/0017-threads-and-scheduling.md).

use core::arch::global_asm;
use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use crate::cell::Racy;
use crate::cpu::{rdmsr, wrmsr};

const IA32_EFER: u32 = 0xC000_0080;
const IA32_STAR: u32 = 0xC000_0081;
const IA32_LSTAR: u32 = 0xC000_0082;
const IA32_FMASK: u32 = 0xC000_0084;
const EFER_SCE: u64 = 1 << 0;

/// `STAR`: `syscall` loads kernel CS 0x08 / SS 0x10; the sysret base 0x18 yields
/// user CS 0x28|3 and SS 0x20|3 (the GDT in `hal::gdt` is ordered for this).
const STAR_VALUE: u64 = (0x08u64 << 32) | (0x18u64 << 48);
/// Clear on syscall entry: IF (no interrupts in the stub), TF, DF, AC and NT.
/// User code can set NT with `popfq`; `syscall` would carry it into the
/// kernel, the context switch does not save RFLAGS, and a later `iretq` with NT
/// set faults in ring 0. Linux masks NT for the same reason.
const FMASK_VALUE: u64 = 0x200 | 0x100 | 0x400 | 0x4_0000 | 0x4000;

/// Raw system-call arguments, as the entry stub lays them out.
#[repr(C)]
pub struct SyscallArgs {
    /// The syscall number (`rax`).
    pub number: u64,
    /// Invoked capability id (`rdi`).
    pub cap: u64,
    /// Packed `MessageInfo` (`rsi`).
    pub msg_info: u64,
    /// Message registers 0-3 (`rdx`, `r8`, `r9`, `r10`).
    pub mr: [u64; 4],
}

/// A fresh ring-3 entry: where to start and the initial message.
#[repr(C)]
pub struct UserEntry {
    /// User instruction pointer.
    pub rip: u64,
    /// User stack pointer.
    pub rsp: u64,
    /// Invoked capability id placed in `rdi`.
    pub cap: u64,
    /// Packed `MessageInfo` placed in `rsi`.
    pub msg_info: u64,
    /// Message registers 0-3 placed in `rdx`/`r8`/`r9`/`r10`.
    pub mr: [u64; 4],
}

/// A kernel system-call handler: given the arguments, returns a status word
/// (delivered to the caller in `rax`).
pub type Handler = fn(&SyscallArgs) -> u64;

static HANDLER: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Dedicated kernel stack for the syscall entry stub (single-core bring-up).
#[repr(C, align(16))]
struct KStack([u8; 16 * 1024]);
static SYSCALL_KSTACK: Racy<KStack> = Racy::new(KStack([0; 16 * 1024]));

/// Top of the syscall kernel stack; read by the entry stub. (`no_mangle` so the
/// assembly can reach it by name.)
#[unsafe(no_mangle)]
static SYSCALL_KSTACK_TOP: AtomicU64 = AtomicU64::new(0);
/// Scratch slot the entry stub uses for one instruction, to move the user
/// `rsp` onto the kernel stack. Never held across the call: a thread that
/// blocks inside a syscall keeps its user `rsp` on its own kernel stack.
#[unsafe(no_mangle)]
static SYSCALL_USER_RSP: AtomicU64 = AtomicU64::new(0);

/// Register the kernel's system-call handler. Call before entering ring 3.
pub fn register(handler: Handler) {
    HANDLER.store(handler as *mut (), Ordering::Release);
}

/// Set the kernel stack the entry stub switches to. The kernel calls this on
/// every context switch with the next thread's kernel stack (ADR 0019).
pub fn set_kernel_stack(top: u64) {
    SYSCALL_KSTACK_TOP.store(top, Ordering::Relaxed);
}

/// Program the syscall MSRs and the kernel stack top. Call once, after the GDT
/// and TSS are installed.
pub fn init() {
    let top = {
        let p = SYSCALL_KSTACK.get() as u64;
        (p + core::mem::size_of::<KStack>() as u64) & !0xf
    };
    SYSCALL_KSTACK_TOP.store(top, Ordering::Relaxed);

    let efer = rdmsr(IA32_EFER);
    // SAFETY: enabling SCE only turns on the `syscall`/`sysret` instructions;
    // STAR/LSTAR/FMASK then define a valid kernel entry per the GDT layout.
    unsafe {
        wrmsr(IA32_EFER, efer | EFER_SCE);
        wrmsr(IA32_STAR, STAR_VALUE);
        wrmsr(IA32_LSTAR, syscall_entry as *const () as u64);
        wrmsr(IA32_FMASK, FMASK_VALUE);
    }
}

/// Drop to ring 3 for good with `entry`'s registers, saving nothing. A user
/// thread's kernel trampoline calls this once; the thread then lives in ring 3
/// and returns to the kernel only through syscalls and interrupts.
pub fn enter_user_once(entry: &UserEntry) -> ! {
    // SAFETY: `entry` describes a mapped, user-accessible code page and stack
    // in the current address space; the asm loads the message registers and
    // `iretq`s to ring 3 with IF set, never returning.
    unsafe { enter_user_once_asm(entry as *const UserEntry) }
}

/// The Rust trampoline the entry stub calls: dispatch to the registered handler.
#[unsafe(no_mangle)]
extern "C" fn syscall_dispatch(args: *const SyscallArgs) -> u64 {
    let raw = HANDLER.load(Ordering::Acquire);
    if raw.is_null() {
        return u64::MAX;
    }
    // SAFETY: `raw` is a `Handler` fn pointer stored by `register`, and `args`
    // points at the `SyscallArgs` the entry stub built on the kernel stack.
    unsafe {
        let handler: Handler = core::mem::transmute(raw);
        handler(&*args)
    }
}

extern "C" {
    fn syscall_entry();
    fn enter_user_once_asm(entry: *const UserEntry) -> !;
}

// The syscall entry stub (LSTAR target). `syscall` leaves the user rsp in rsp
// (it does not switch stacks), the return RIP in rcx, and RFLAGS in r11. We park
// the user rsp in a scratch slot for one instruction, switch to the kernel stack
// and push it there (so a thread that blocks in a syscall keeps its own), build a
// `SyscallArgs` on it, call the dispatcher, then restore and `sysretq`. IF is clear
// throughout (FMASK). ABI: rax=number, rdi=cap, rsi=msg_info, rdx/r8/r9/r10 =
// mr0..3; rax = status out. Ten words are pushed, keeping rsp 16-byte aligned for
// the call.
//
// enter_user_once_asm `iretq`s to ring 3 for good with the message registers
// loaded and the scratch registers cleared. User selectors: CS 0x2b, SS 0x23
// (see hal::gdt).
global_asm!(
    ".global syscall_entry",
    ".p2align 4",
    "syscall_entry:",
    "    mov [rip + SYSCALL_USER_RSP], rsp",
    "    mov rsp, [rip + SYSCALL_KSTACK_TOP]",
    "    push qword ptr [rip + SYSCALL_USER_RSP]", // user RSP, kept on this stack
    "    push rcx",                                // user RIP
    "    push r11",                                // user RFLAGS
    "    push r10",                                // mr3
    "    push r9",                                 // mr2
    "    push r8",                                 // mr1
    "    push rdx",                                // mr0
    "    push rsi",                                // msg_info
    "    push rdi",                                // cap
    "    push rax",                                // number  -> rsp = &SyscallArgs
    "    mov rdi, rsp",
    "    call syscall_dispatch",
    // Reload the argument registers from SyscallArgs, so whatever the Rust
    // handler left in caller-saved registers never reaches ring 3. (rbx, rbp
    // and r12 to r15 are callee-saved: the call preserved the user's values.)
    "    mov rdi, [rsp + 8]",
    "    mov rsi, [rsp + 16]",
    "    mov rdx, [rsp + 24]",
    "    mov r8,  [rsp + 32]",
    "    mov r9,  [rsp + 40]",
    "    mov r10, [rsp + 48]",
    "    add rsp, 56", // pop the seven SyscallArgs words (rax holds status)
    "    pop r11",
    "    pop rcx",
    "    pop rsp", // user RSP
    "    sysretq",
    //
    ".global enter_user_once_asm",
    ".p2align 4",
    "enter_user_once_asm:",
    "    mov r11, rdi",        // UserEntry*
    "    mov rax, [r11 + 0]",  // rip
    "    mov rbx, [r11 + 8]",  // rsp
    "    mov rdi, [r11 + 16]", // cap
    "    mov rsi, [r11 + 24]", // msg_info
    "    mov rdx, [r11 + 32]", // mr0
    "    mov r8,  [r11 + 40]", // mr1
    "    mov r9,  [r11 + 48]", // mr2
    "    mov r10, [r11 + 56]", // mr3
    "    push 0x23",           // user SS
    "    push rbx",            // user RSP
    "    push 0x202",          // RFLAGS (IF set)
    "    push 0x2b",           // user CS
    "    push rax",            // user RIP
    // Clear every register that is not an input: the kernel trampoline left
    // kernel addresses in them (r12 holds the entry point, rbp a kernel stack
    // address), which would hand ring 3 the KASLR slide.
    "    xor eax, eax",
    "    xor ebx, ebx",
    "    xor ecx, ecx",
    "    xor ebp, ebp",
    "    xor r11d, r11d",
    "    xor r12d, r12d",
    "    xor r13d, r13d",
    "    xor r14d, r14d",
    "    xor r15d, r15d",
    "    iretq",
);
