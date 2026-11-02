//! x87 FPU and SSE state, saved and restored per thread (#83).
//!
//! [`init`] turns the FPU and SSE on for the whole system (CR0: native FPU
//! errors, no emulation, no lazy-switch trap; CR4: `OSFXSR` and `OSXMMEXCPT`)
//! and records a clean default state. Each thread owns an [`FpuArea`]; the
//! kernel saves the outgoing thread's state and restores the incoming one's on
//! every context switch, **eagerly**, with `fxsave`/`fxrstor`. The kernel
//! itself is built soft-float (`x86_64-unknown-none` has no SSE), so interrupt
//! and syscall handlers never touch these registers and a switch is the only
//! place they change hands.
//!
//! `fxsave` covers x87 and SSE state, everything XCR0 would hold with only
//! those enabled. AVX and wider state need `xsave` with a larger area; that
//! extends this module when a task needs it (ADR 0021).

use core::arch::asm;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::cell::Racy;

/// CR0.MP: `wait`/`fwait` honour TS.
const CR0_MP: u64 = 1 << 1;
/// CR0.EM: emulate the FPU (must be clear for SSE).
const CR0_EM: u64 = 1 << 2;
/// CR0.TS: trap on the next FPU use (lazy switching; we switch eagerly).
const CR0_TS: u64 = 1 << 3;
/// CR0.NE: report x87 errors as `#MF`, not through the legacy IRQ.
const CR0_NE: u64 = 1 << 5;
/// CR4.OSFXSR: the OS uses `fxsave`/`fxrstor`, enabling SSE.
const CR4_OSFXSR: u64 = 1 << 9;
/// CR4.OSXMMEXCPT: the OS handles SIMD floating-point exceptions (`#XM`).
const CR4_OSXMMEXCPT: u64 = 1 << 10;

/// The 512-byte `fxsave` image (Intel SDM Vol. 1 §10.5.1), 16-byte aligned as
/// `fxsave` requires.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct FxState([u8; 512]);

/// The state every thread starts with: `fninit`'s x87 defaults and MXCSR
/// `0x1F80` (all SIMD exceptions masked), captured by [`init`].
static DEFAULT: Racy<FxState> = Racy::new(FxState([0; 512]));
static READY: AtomicBool = AtomicBool::new(false);

/// Turn on the FPU and SSE and record the default state. Call once, early,
/// before any thread can run code that uses them.
pub fn init() {
    let cr0 = crate::cpu::read_cr0();
    let cr4 = crate::cpu::read_cr4();
    // SAFETY: these bits only select how the FPU and SSE units behave (Intel
    // SDM Vol. 3A §13.1); the kernel touches no FPU or SSE register itself, so
    // nothing in flight is disturbed.
    unsafe {
        crate::cpu::write_cr0((cr0 & !(CR0_EM | CR0_TS)) | CR0_MP | CR0_NE);
        crate::cpu::write_cr4(cr4 | CR4_OSFXSR | CR4_OSXMMEXCPT);
    }
    let mxcsr: u32 = 0x1F80;
    // SAFETY: the FPU and SSE are enabled above. `fninit` resets the x87
    // state, `ldmxcsr` loads a valid MXCSR (reserved bits clear), and
    // `fxsave` writes 512 bytes into `DEFAULT`, which is 16-byte aligned and
    // written only here, once, during single-threaded boot (see `Racy`).
    unsafe {
        asm!(
            "fninit",
            "ldmxcsr [{mxcsr}]",
            "fxsave64 [{area}]",
            mxcsr = in(reg) &mxcsr,
            area = in(reg) DEFAULT.get(),
            options(nostack, preserves_flags),
        );
    }
    READY.store(true, Ordering::Release);
}

/// One thread's saved FPU and SSE state. Fresh areas, and areas after
/// [`reset`](Self::reset), restore to the default state.
pub struct FpuArea {
    state: UnsafeCell<FxState>,
    /// Whether `state` holds a saved image; if not, restoring loads the default.
    saved: AtomicBool,
}

// SAFETY: an area is used only by context switches on this single core, with
// interrupts off: one switch at a time saves into the outgoing thread's area
// and restores from the incoming one's, so no two accesses overlap.
unsafe impl Sync for FpuArea {}

impl FpuArea {
    /// An area that restores to the default state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: UnsafeCell::new(FxState([0; 512])),
            saved: AtomicBool::new(false),
        }
    }

    /// Forget any saved state: the next restore loads the default, so a new
    /// thread in a reused slot never sees its predecessor's registers.
    pub fn reset(&self) {
        self.saved.store(false, Ordering::Relaxed);
    }

    /// Save the FPU and SSE registers into this area. Call during a context
    /// switch, with interrupts off, for the outgoing thread.
    pub fn save(&self) {
        if !READY.load(Ordering::Acquire) {
            return;
        }
        // SAFETY: SSE is enabled (`init` ran); `state` is a 16-byte-aligned
        // 512-byte area that only this switch touches (see the `Sync` impl).
        unsafe {
            asm!("fxsave64 [{}]", in(reg) self.state.get(), options(nostack, preserves_flags));
        }
        self.saved.store(true, Ordering::Relaxed);
    }

    /// Load this area into the FPU and SSE registers (the default state if
    /// nothing was saved). Call during a context switch, with interrupts off,
    /// for the incoming thread.
    pub fn restore(&self) {
        if !READY.load(Ordering::Acquire) {
            return;
        }
        let image = if self.saved.load(Ordering::Relaxed) {
            self.state.get().cast_const()
        } else {
            DEFAULT.get().cast_const()
        };
        // SAFETY: SSE is enabled; `image` is a valid `fxsave` image (saved by
        // `save`, or the default `init` captured), 16-byte aligned; reading it
        // does not race (see the `Sync` impl).
        unsafe {
            asm!("fxrstor64 [{}]", in(reg) image, options(nostack, preserves_flags, readonly));
        }
    }
}

impl Default for FpuArea {
    fn default() -> Self {
        Self::new()
    }
}
