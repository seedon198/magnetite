//! Core CPU control primitives.
//!
//! The asm here that changes how memory behaves, or lets an interrupt handler
//! run (`cli`, `sti`, `hlt`, CR4 writes), is deliberately **not** `nomem`:
//! `nomem` lets the compiler move loads and stores across the instruction, and
//! atomic orderings do not order such a block. Without it the block is a
//! compiler barrier, so a `SpinLock`'s acquire stays after its `cli` and its
//! release store before its `sti` (#110). Pure register reads stay `nomem`.

use core::arch::asm;

/// Halt the logical CPU until the next interrupt, forever.
///
/// Used as the kernel's final resting state (and by the panic handler) until
/// a scheduler exists.
pub fn halt_loop() -> ! {
    loop {
        // SAFETY: `hlt` halts the logical CPU until the next interrupt. It has
        // no memory operands and no effect on program state besides stopping
        // until an interrupt, so it is always sound to execute in ring 0. Not
        // `nomem`: interrupt handlers that run while it waits write memory.
        unsafe {
            asm!("hlt", options(nostack, preserves_flags));
        }
    }
}

/// Halt the logical CPU until the next interrupt, once.
///
/// With interrupts enabled this idles until the next IRQ (e.g. a timer tick,
/// which may preempt the caller) and then returns. With interrupts disabled it
/// waits for an NMI/SMI; callers that want to idle must enable them first.
pub fn halt() {
    // SAFETY: as in `halt_loop` - `hlt` only pauses until the next interrupt.
    unsafe {
        asm!("hlt", options(nostack, preserves_flags));
    }
}

/// Disable maskable interrupts on the current CPU.
pub fn disable_interrupts() {
    // SAFETY: `cli` only clears the interrupt flag. Not `nomem`, so it is a
    // compiler barrier: the critical section's accesses stay after it.
    unsafe {
        asm!("cli", options(nostack));
    }
}

/// Enable maskable interrupts on the current CPU.
pub fn enable_interrupts() {
    // SAFETY: `sti` only sets the interrupt flag. Not `nomem`, so it is a
    // compiler barrier: the critical section's accesses stay before it.
    unsafe {
        asm!("sti", options(nostack));
    }
}

/// Whether maskable interrupts are currently enabled (RFLAGS.IF).
#[must_use]
pub fn interrupts_enabled() -> bool {
    let flags: u64;
    // SAFETY: reads RFLAGS via the stack; no lasting memory or flag effects.
    unsafe {
        asm!("pushfq", "pop {}", out(reg) flags, options(preserves_flags));
    }
    flags & (1 << 9) != 0
}

/// Run `f` with maskable interrupts disabled on this core, then restore the
/// interrupt flag to what it was on entry (never enabling interrupts that were
/// already disabled).
///
/// Used for critical sections that are not a single lock - e.g. picking the
/// next thread and switching to it, which must not be split by a timer
/// preemption. A context switch inside `f` is fine: each thread restores its
/// own saved flag when it eventually returns from here.
pub fn without_interrupts<R>(f: impl FnOnce() -> R) -> R {
    let saved_if = interrupts_enabled();
    disable_interrupts();
    let result = f();
    if saved_if {
        enable_interrupts();
    }
    result
}

/// Read model-specific register `msr`.
///
/// Callers pass only architectural MSRs that exist on every x86-64 CPU we
/// support (reading an unimplemented MSR raises `#GP`).
pub(crate) fn rdmsr(msr: u32) -> u64 {
    let (hi, lo): (u32, u32);
    // SAFETY: a side-effect-free MSR read; callers pass only MSRs that exist
    // on x86-64 (see the contract above).
    unsafe {
        asm!("rdmsr", in("ecx") msr, out("edx") hi, out("eax") lo,
             options(nomem, nostack, preserves_flags));
    }
    (u64::from(hi) << 32) | u64::from(lo)
}

/// Write `value` to model-specific register `msr`.
///
/// # Safety
/// `msr`/`value` must be valid to write, and the resulting CPU configuration
/// must keep the kernel running correctly.
pub(crate) unsafe fn wrmsr(msr: u32, value: u64) {
    // Deliberate truncation: WRMSR takes the value split into EDX:EAX.
    let (hi, lo) = ((value >> 32) as u32, value as u32);
    // SAFETY: guaranteed by the caller.
    unsafe {
        asm!("wrmsr", in("ecx") msr, in("edx") hi, in("eax") lo,
             options(nostack, preserves_flags));
    }
}

/// `CR4.SMEP` - Supervisor Mode Execution Prevention (bit 20).
const CR4_SMEP: u64 = 1 << 20;
/// `CR4.SMAP` - Supervisor Mode Access Prevention (bit 21).
const CR4_SMAP: u64 = 1 << 21;

/// Enable SMEP and/or SMAP in `CR4`, where the CPU supports them.
///
/// SMEP faults on supervisor *execution* of user-accessible pages; SMAP faults
/// on supervisor *data access* to them (unless `EFLAGS.AC` is set via
/// `stac`/`clac`). With no user mappings yet, enabling both is safe; once user
/// memory exists (M4), deliberate accesses to it must bracket with `stac`/
/// `clac`. Returns the `(smep, smap)` bits actually enabled.
pub fn enable_smep_smap(smep: bool, smap: bool) -> (bool, bool) {
    if !smep && !smap {
        return (false, false);
    }
    let mut cr4 = read_cr4();
    if smep {
        cr4 |= CR4_SMEP;
    }
    if smap {
        cr4 |= CR4_SMAP;
    }
    // SAFETY: we only set the SMEP/SMAP enable bits (read-modify-write preserves
    // all others). Both only *add* supervisor-access faults for user pages, of
    // which the kernel currently maps none, so enabling them cannot fault any
    // existing access.
    unsafe { write_cr4(cr4) }
    (smep, smap)
}

/// Read `CR4`.
fn read_cr4() -> u64 {
    let value: u64;
    // SAFETY: reading CR4 is a side-effect-free privileged register read.
    unsafe { asm!("mov {}, cr4", out(reg) value, options(nomem, nostack, preserves_flags)) }
    value
}

/// Write `CR4`.
///
/// # Safety
/// `value` must be a valid `CR4` configuration for the current mode (e.g. it
/// must not clear bits required to stay in long mode).
unsafe fn write_cr4(value: u64) {
    // SAFETY: guaranteed by the caller's contract above. Not `nomem`: SMEP,
    // SMAP and PGE change how the accesses around it behave.
    unsafe { asm!("mov cr4, {}", in(reg) value, options(nostack, preserves_flags)) }
}
