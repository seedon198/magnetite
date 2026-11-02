//! Page-fault support: a region registry (so a fault can be told *where* it
//! landed) and a recoverable memory probe.
//!
//! The kernel registers named virtual ranges (its sections, the HHDM, the heap,
//! stack guards) with [`register_region`]; the #PF reporter in
//! [`crate::idt`] calls [`classify`] to label the faulting address.
//!
//! [`probe_read`] attempts a read that may fault: a one-entry exception table
//! lets the #PF handler recognize the faulting instruction and resume at a
//! fixup instead of halting. This proves a mapping is absent without crashing,
//! and is the seed of the fault-tolerant accessors user-memory access will need
//! (M4). See [ADR 0014](../../docs/adr/0014-page-fault-diagnostics.md).

use core::arch::global_asm;

use crate::idt::TrapFrame;
use crate::sync::SpinLock;

/// A named virtual-address range, for classifying a faulting address.
#[derive(Clone, Copy)]
struct NamedRegion {
    name: &'static str,
    start: u64,
    end: u64,
}

const MAX_REGIONS: usize = 32;

static REGIONS: SpinLock<([Option<NamedRegion>; MAX_REGIONS], usize)> =
    SpinLock::new(([None; MAX_REGIONS], 0));

/// Register `[start, end)` under `name` for fault classification. Ignored if the
/// (small, fixed) table is full.
pub fn register_region(name: &'static str, start: u64, end: u64) {
    let mut g = REGIONS.lock();
    let n = g.1;
    if n < MAX_REGIONS {
        g.0[n] = Some(NamedRegion { name, start, end });
        g.1 = n + 1;
    }
}

/// A classifier for address ranges too regular, or too many, for the region
/// table: given an address, its name, or `None` to fall back to the table.
pub type Classifier = fn(u64) -> Option<&'static str>;

static CLASSIFIER: SpinLock<Option<Classifier>> = SpinLock::new(None);

/// Install a [`Classifier`], consulted before the region table (the kernel
/// names its kernel-stack window, one guard page per slot, this way; #125).
pub fn register_classifier(f: Classifier) {
    *CLASSIFIER.lock() = Some(f);
}

/// The name of the region containing `addr`: what the registered
/// [`Classifier`] says, else the narrowest registered region, so a specific
/// region (e.g. a stack guard) wins over an enclosing one (e.g.
/// `.data/.bss`). `None` if nothing names `addr`.
#[must_use]
pub fn classify(addr: u64) -> Option<&'static str> {
    let hook = *CLASSIFIER.lock();
    if let Some(name) = hook.and_then(|f| f(addr)) {
        return Some(name);
    }
    let g = REGIONS.lock();
    let mut best: Option<&NamedRegion> = None;
    for region in g.0.iter().take(g.1).flatten() {
        if addr >= region.start && addr < region.end {
            let narrower = best.is_none_or(|b| region.end - region.start < b.end - b.start);
            if narrower {
                best = Some(region);
            }
        }
    }
    best.map(|r| r.name)
}

// A one-entry exception table. `probe_read_byte` reads one byte; if that read
// faults, the #PF handler (via `try_recover`) redirects execution to
// `probe_read_fixup`, which returns 1. A successful read returns 0.
global_asm!(
    ".global probe_read_byte",
    ".global probe_read_fixup",
    ".p2align 4",
    "probe_read_byte:",
    "    movzx eax, byte ptr [rdi]", // may #PF - this is the recorded fault site
    "    xor eax, eax",              // success -> 0
    "    ret",
    "probe_read_fixup:",
    "    mov eax, 1", // recovered fault -> 1
    "    ret",
);

extern "C" {
    fn probe_read_byte(addr: u64) -> u64;
    fn probe_read_fixup();
}

/// Attempt to read one byte at `addr`. Returns `true` if the access faulted
/// (and was recovered), `false` if it succeeded. Never crashes: a fault at the
/// probe site is caught and redirected by [`try_recover`].
#[must_use]
pub fn probe_read(addr: u64) -> bool {
    // SAFETY: `probe_read_byte` either reads one byte (if `addr` is mapped) or
    // raises #PF at a known instruction, which the fault handler recognizes and
    // resumes at the fixup; either way it returns normally (0 or 1).
    unsafe { probe_read_byte(addr) != 0 }
}

/// If `frame` is a recoverable probe fault (a #PF at the probe's read
/// instruction), redirect it to the fixup and return `true`; otherwise `false`.
/// Called first by the #PF handler so probe faults resume silently.
pub fn try_recover(frame: &mut TrapFrame) -> bool {
    if frame.vector == 14 && frame.rip == probe_read_byte as *const () as u64 {
        frame.rip = probe_read_fixup as *const () as u64;
        true
    } else {
        false
    }
}
