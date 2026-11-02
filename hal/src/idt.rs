//! Interrupt Descriptor Table and the interrupt/exception dispatch path.
//!
//! We install a 256-entry IDT whose gates point at hand-written assembly stubs
//! (generated below). Each stub normalizes the stack - pushing a dummy error
//! code for vectors the CPU does not supply one for - pushes its vector number,
//! and jumps to a common stub that saves all general-purpose registers into a
//! [`TrapFrame`] and calls [`idt_dispatch`].
//!
//! Dispatch is table-driven ([ADR 0009] hand-rolled): a per-vector table of
//! registered handlers lets IRQ drivers (M3), IPIs, and others hook in at
//! runtime via [`register_handler`]. Vectors without a handler fall back to a
//! default that prints a diagnostic report; CPU exceptions (0-31) that are not
//! recoverable then halt.
//!
//! References: Intel SDM Vol. 3A ch. 6 (interrupt & exception handling).
//!
//! [ADR 0009]: ../../../docs/adr/0009-hand-rolled-cpu-tables.md

use core::arch::{asm, global_asm};
use core::fmt::Write;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::cell::Racy;
use crate::gdt::KERNEL_CODE;
use crate::serial::SerialPort;

/// Saved machine state at the point of an interrupt or exception.
///
/// Field order matches the layout the stubs build on the stack: general
/// registers (pushed by the common stub), then the vector and error code
/// (pushed by the per-vector stub / CPU), then the CPU-pushed interrupt frame.
#[repr(C)]
#[derive(Debug)]
#[allow(missing_docs)] // general-purpose registers; names are self-describing
pub struct TrapFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    /// Interrupt/exception vector number.
    pub vector: u64,
    /// CPU error code, or 0 for vectors that do not supply one.
    pub error_code: u64,
    /// Instruction pointer at the point of interrupt.
    pub rip: u64,
    /// Code segment selector.
    pub cs: u64,
    /// Saved flags.
    pub rflags: u64,
    /// Stack pointer at the point of interrupt.
    pub rsp: u64,
    /// Stack segment selector.
    pub ss: u64,
}

/// A handler for one vector.
pub type Handler = fn(&mut TrapFrame);

const IDT_LEN: usize = 256;

/// Per-vector handler table. `0` means "no handler"; otherwise the value is a
/// [`Handler`] function pointer.
static HANDLERS: [AtomicU64; IDT_LEN] = [const { AtomicU64::new(0) }; IDT_LEN];

/// Register `handler` for `vector`, replacing any previous one. Drivers use
/// this for IRQ vectors (M3); the architecture reserves 0-31 for exceptions.
pub fn register_handler(vector: u8, handler: Handler) {
    HANDLERS[vector as usize].store(handler as usize as u64, Ordering::Release);
}

/// The kernel's handler for CPU exceptions raised in ring 3 (0 = none). It
/// never returns: it ends the faulting thread (ADR 0019).
static USER_FAULT: AtomicU64 = AtomicU64::new(0);

/// A handler for an exception taken in ring 3; it must not return.
pub type UserFaultHandler = fn(&mut TrapFrame) -> !;

/// Route CPU exceptions (vectors 0 to 31) raised **in ring 3** to `handler`,
/// instead of the fatal kernel report. Exceptions in ring 0 are unaffected.
pub fn register_user_fault(handler: UserFaultHandler) {
    USER_FAULT.store(handler as usize as u64, Ordering::Release);
}

/// Whether exception `vector` is one user code can cause, and so, raised in
/// ring 3, ends the thread: #DE, #DB, #BP, #OF, #BR, #UD, #NM, #TS, #NP, #SS,
/// #GP, #PF, #MF, #AC, #XM, #VE and #CP. Never NMI (2), #DF (8) or #MC (18).
#[must_use]
pub const fn user_fault_vector(vector: u64) -> bool {
    matches!(vector, 0 | 1 | 3..=7 | 10..=17 | 19..=21)
}

// The exceptions user code can cause are routed to the user-fault handler;
// NMI (2), #DF (8) and #MC (18) never are, even when they land in ring 3:
// they are not the thread's fault, and a #DF's saved CS is undefined.
const _: () = {
    assert!(user_fault_vector(14) && user_fault_vector(13) && user_fault_vector(6));
    assert!(user_fault_vector(0) && user_fault_vector(3));
    assert!(!user_fault_vector(2) && !user_fault_vector(8) && !user_fault_vector(18));
    assert!(!user_fault_vector(32) && !user_fault_vector(0x30));
};

/// The faulting linear address of the last page fault (`CR2`).
#[must_use]
pub fn fault_address() -> u64 {
    read_cr2()
}

/// Common dispatch routine, called by the assembly common stub with a pointer
/// to the freshly built [`TrapFrame`].
///
/// Exported unmangled so the assembly can `call` it.
#[unsafe(no_mangle)]
extern "C" fn idt_dispatch(frame: &mut TrapFrame) {
    let slot = HANDLERS[(frame.vector as usize) & (IDT_LEN - 1)].load(Ordering::Acquire);
    // A CPU exception raised in ring 3 with no specific handler ends the user
    // thread, not the kernel. Keyed on the trap frame's CS, never on which
    // thread is current, so a kernel-mode fault is always fatal.
    if slot == 0 && user_fault_vector(frame.vector) && frame.cs & 3 == 3 {
        let user = USER_FAULT.load(Ordering::Acquire);
        if user != 0 {
            // SAFETY: `user` is non-zero only because `register_user_fault`
            // stored a valid `UserFaultHandler` there; transmuted back to it.
            let handler: UserFaultHandler =
                unsafe { core::mem::transmute::<usize, UserFaultHandler>(user as usize) };
            handler(frame);
        }
    }
    if slot != 0 {
        // SAFETY: `slot` is non-zero only because `register_handler` stored a
        // valid `Handler` (`fn(&mut TrapFrame)`) pointer there; we transmute it
        // back to that exact type.
        let handler: Handler = unsafe { core::mem::transmute::<usize, Handler>(slot as usize) };
        handler(frame);
        return;
    }
    default_exception(frame);
}

/// Human-readable name for a CPU exception vector.
#[must_use]
pub fn exception_name(vector: u64) -> &'static str {
    match vector {
        0 => "#DE divide error",
        1 => "#DB debug",
        2 => "NMI",
        3 => "#BP breakpoint",
        4 => "#OF overflow",
        5 => "#BR bound range exceeded",
        6 => "#UD invalid opcode",
        7 => "#NM device not available",
        8 => "#DF double fault",
        10 => "#TS invalid TSS",
        11 => "#NP segment not present",
        12 => "#SS stack-segment fault",
        13 => "#GP general protection fault",
        14 => "#PF page fault",
        16 => "#MF x87 floating-point",
        17 => "#AC alignment check",
        18 => "#MC machine check",
        19 => "#XM SIMD floating-point",
        20 => "#VE virtualization",
        21 => "#CP control protection",
        _ => "(reserved/unknown)",
    }
}

/// Read CR2 (the faulting linear address for a page fault).
fn read_cr2() -> u64 {
    let value: u64;
    // SAFETY: reading CR2 has no side effects and is always valid in ring 0.
    unsafe {
        asm!("mov {}, cr2", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Fallback handler for vectors with no registered handler: print a diagnostic
/// report over serial. A breakpoint (#BP) is a trap and resumes; any other CPU
/// exception is treated as unrecoverable and halts.
fn default_exception(frame: &mut TrapFrame) {
    // Recoverable memory probe: a #PF at the probe site resumes at its fixup,
    // silently, instead of reporting and halting (see `crate::fault`).
    if crate::fault::try_recover(frame) {
        return;
    }

    // Use a fresh serial port (like the panic handler) rather than the kernel
    // logger, so a fault while the log lock is held still reports.
    let mut com1 = SerialPort::com1();
    com1.init();

    let _ = writeln!(com1);
    let _ = writeln!(
        com1,
        "*** EXCEPTION: {} (vector {}) ***",
        exception_name(frame.vector),
        frame.vector
    );
    let _ = writeln!(com1, "  error_code = {:#018x}", frame.error_code);
    if frame.vector == 14 {
        let cr2 = read_cr2();
        let region = crate::fault::classify(cr2).unwrap_or("unmapped/unknown");
        let _ = writeln!(com1, "  cr2        = {cr2:#018x}  ({region})");

        let e = page_table::PageFaultError::new(frame.error_code);
        let kind = if e.protection_violation() {
            "protection violation"
        } else {
            "not present"
        };
        let op = if e.instruction_fetch() {
            "instruction fetch"
        } else if e.write() {
            "write"
        } else {
            "read"
        };
        let mode = if e.user() { "user" } else { "supervisor" };
        let _ = writeln!(com1, "  cause      = {kind}, {op}, {mode} mode");
        if e.reserved_write() {
            let _ = writeln!(com1, "               (reserved bit set in a page table)");
        }
    }
    let _ = writeln!(
        com1,
        "  rip = {:#018x}  cs = {:#06x}  rflags = {:#018x}",
        frame.rip, frame.cs, frame.rflags
    );
    let _ = writeln!(com1, "  rsp = {:#018x}  ss = {:#06x}", frame.rsp, frame.ss);
    let _ = writeln!(
        com1,
        "  rax={:#018x} rbx={:#018x} rcx={:#018x}",
        frame.rax, frame.rbx, frame.rcx
    );
    let _ = writeln!(
        com1,
        "  rdx={:#018x} rsi={:#018x} rdi={:#018x}",
        frame.rdx, frame.rsi, frame.rdi
    );
    let _ = writeln!(
        com1,
        "  rbp={:#018x}  r8={:#018x}  r9={:#018x}",
        frame.rbp, frame.r8, frame.r9
    );

    if frame.vector == 3 {
        let _ = writeln!(com1, "  (breakpoint - resuming)");
        return;
    }

    crate::backtrace::print_from(&mut com1, frame.rbp);
    let _ = writeln!(com1, "  unrecoverable exception - halting");
    crate::cpu::halt_loop();
}

/// Trigger a breakpoint exception (`int3`). Used to exercise the trap path.
pub fn breakpoint() {
    // SAFETY: `int3` raises a #BP trap, which our IDT handles and resumes from.
    unsafe {
        asm!("int3", options(nomem, nostack));
    }
}

// --- IDT structures -------------------------------------------------------

/// A 64-bit IDT gate descriptor (Intel SDM Vol. 3A §6.14.1).
#[repr(C)]
#[derive(Clone, Copy)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    type_attr: u8,
    offset_mid: u16,
    offset_high: u32,
    reserved: u32,
}

const _: () = assert!(core::mem::size_of::<IdtEntry>() == 16);

impl IdtEntry {
    const fn missing() -> Self {
        Self {
            offset_low: 0,
            selector: 0,
            ist: 0,
            type_attr: 0,
            offset_mid: 0,
            offset_high: 0,
            reserved: 0,
        }
    }

    /// Build a present 64-bit interrupt gate (DPL 0) for `handler`, using IST
    /// index `ist` (0 = none).
    fn interrupt_gate(handler: u64, ist: u8) -> Self {
        Self {
            offset_low: handler as u16,
            selector: KERNEL_CODE,
            ist: ist & 0x7,
            type_attr: 0x8E, // present, DPL 0, type 0xE (64-bit interrupt gate)
            offset_mid: (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
            reserved: 0,
        }
    }
}

static IDT: Racy<[IdtEntry; IDT_LEN]> = Racy::new([IdtEntry::missing(); IDT_LEN]);

#[repr(C, packed)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

extern "C" {
    /// Table of the 256 per-vector stub entry points, generated in asm below.
    static VECTOR_STUBS: [u64; IDT_LEN];
}

/// Install the IDT: fill each gate from the stub table, assign IST stacks to
/// the critical faults, and load the IDTR. Call once after [`crate::gdt::init`].
pub fn init() {
    // SAFETY: one-time, single-threaded boot initialization (see `Racy`).
    unsafe {
        let idt = IDT.get();
        for (vector, &stub) in VECTOR_STUBS.iter().enumerate() {
            // IST stacks set up in gdt.rs: IST1=#DF, IST2=NMI, IST3=#MC.
            let ist = match vector {
                8 => 1,
                2 => 2,
                18 => 3,
                _ => 0,
            };
            (*idt)[vector] = IdtEntry::interrupt_gate(stub, ist);
        }

        let pointer = DescriptorTablePointer {
            limit: (core::mem::size_of::<[IdtEntry; IDT_LEN]>() - 1) as u16,
            base: core::ptr::addr_of!((*idt)[0]) as u64,
        };
        // SAFETY: `pointer` describes our freshly filled, 'static IDT.
        asm!("lidt [{}]", in(reg) &pointer, options(readonly, nostack, preserves_flags));
    }
}

// --- Assembly stubs -------------------------------------------------------

global_asm!(
    r#"
.text

// Common tail: save GPRs, call the Rust dispatcher with &mut TrapFrame, restore.
common_stub:
    push rax
    push rbx
    push rcx
    push rdx
    push rsi
    push rdi
    push rbp
    push r8
    push r9
    push r10
    push r11
    push r12
    push r13
    push r14
    push r15
    mov rdi, rsp          // &mut TrapFrame
    mov rbp, rsp          // save unaligned rsp
    and rsp, -16          // 16-byte align for the SysV call
    call idt_dispatch
    mov rsp, rbp          // restore
    pop r15
    pop r14
    pop r13
    pop r12
    pop r11
    pop r10
    pop r9
    pop r8
    pop rbp
    pop rdi
    pop rsi
    pop rdx
    pop rcx
    pop rbx
    pop rax
    add rsp, 16           // discard vector + error_code
    iretq

// Per-vector stubs. Vectors with a CPU-pushed error code skip the dummy push.
.altmacro
.macro stub n
.align 16
vector_stub_\n :
.if (\n == 8) || (\n == 10) || (\n == 11) || (\n == 12) || (\n == 13) || (\n == 14) || (\n == 17) || (\n == 21)
.else
    push 0
.endif
    push \n
    jmp common_stub
.endm

.set i, 0
.rept 256
    stub %i
    .set i, i+1
.endr

// Table of stub addresses, indexed by vector. Push and pop the section: module
// asm from several `global_asm!` blocks shares one section state, and a bare
// `.section .rodata` here would put whatever asm follows it into `.rodata`
// (non-executable), as it did the syscall stubs on CI.
.pushsection .rodata
.align 8
.global VECTOR_STUBS
VECTOR_STUBS:
.macro stubaddr n
    .quad vector_stub_\n
.endm
.set j, 0
.rept 256
    stubaddr %j
    .set j, j+1
.endr
.popsection
"#
);
