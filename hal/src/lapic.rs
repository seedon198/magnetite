//! The Local APIC and its timer - the source of preemption (#80).
//!
//! [`init`] maps the xAPIC register page (uncached, at a kernel virtual address
//! the caller chooses), enables the APIC, and installs the spurious-interrupt
//! vector. [`start_periodic`] calibrates the APIC timer against the PIT and
//! starts it firing a vector at a fixed rate; the kernel's handler for that
//! vector drives the scheduler and must call [`eoi`].
//!
//! We use xAPIC (MMIO) mode, which every x86-64 CPU and QEMU's default CPU
//! models support; x2APIC (MSR access) is a later refinement.
//!
//! References: Intel SDM Vol. 3A ch. 11 (APIC) - §11.4 (the APIC base MSR and
//! register map), §11.5.4 (the APIC timer), §11.9 (spurious interrupts).

use core::sync::atomic::{AtomicU64, Ordering};

use crate::cpu::{rdmsr, wrmsr};
use crate::idt::{self, TrapFrame};
use crate::paging::{AddressSpace, Flags, MapError};

const IA32_APIC_BASE: u32 = 0x1B;
/// `IA32_APIC_BASE` bit 11: the APIC global enable.
const APIC_BASE_ENABLE: u64 = 1 << 11;
/// `IA32_APIC_BASE` bit 10: x2APIC mode (MMIO access is disabled when set).
const APIC_BASE_X2APIC: u64 = 1 << 10;
/// `IA32_APIC_BASE` bits 12..: the register page's physical base.
const APIC_BASE_ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

// xAPIC register offsets (SDM Vol. 3A Table 11-1).
const REG_ID: usize = 0x020;
const REG_TPR: usize = 0x080;
const REG_EOI: usize = 0x0B0;
const REG_SVR: usize = 0x0F0;
const REG_LVT_TIMER: usize = 0x320;
const REG_TIMER_INITIAL: usize = 0x380;
const REG_TIMER_CURRENT: usize = 0x390;
const REG_TIMER_DIVIDE: usize = 0x3E0;

/// Spurious-interrupt vector register bit 8: APIC software enable.
const SVR_ENABLE: u32 = 1 << 8;
/// LVT bit 16: entry masked.
const LVT_MASKED: u32 = 1 << 16;
/// LVT timer bits 17-18 = 01: periodic mode.
const LVT_TIMER_PERIODIC: u32 = 1 << 17;
/// Divide-configuration encoding for "divide by 16".
const TIMER_DIVIDE_BY_16: u32 = 0b0011;

/// The spurious-interrupt vector. Its low four bits must be all ones on older
/// (P6-era) APICs, so the conventional choice is 0xFF.
pub const SPURIOUS_VECTOR: u8 = 0xFF;

/// Calibration window, in microseconds, measured with the PIT.
const CALIBRATION_US: u32 = 10_000;

/// Plausible APIC timer ticks per millisecond at divide-by-16: a timer clock
/// from 10 MHz to 10 GHz. Real parts run it from a 19.2 to 100 MHz crystal or
/// bus clock, and QEMU at 1 GHz; a reading outside this range means the PIT
/// window was not real (#109).
const MIN_TICKS_PER_MS: u32 = 625; // 10 MHz / 16 / 1000
const MAX_TICKS_PER_MS: u32 = 625_000; // 10 GHz / 16 / 1000

/// Virtual base of the mapped register page; 0 until [`init`] succeeds.
static BASE: AtomicU64 = AtomicU64::new(0);

/// Why bringing up the Local APIC or its timer failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LapicError {
    /// CPUID reports no on-chip APIC.
    NotPresent,
    /// Firmware left the APIC in x2APIC mode, where the MMIO page is inert.
    X2apicMode,
    /// Mapping the register page failed.
    Map(MapError),
    /// [`init`] has not run (or failed), so there are no registers to program.
    NotInitialized,
    /// Timer frequency of 0 Hz requested, or too high for the measured clock.
    BadFrequency,
    /// No usable PIT window, or the APIC timer measured outside the
    /// plausible range during calibration.
    CalibrationFailed,
}

/// The programmed timer configuration, for logging.
#[derive(Debug, Clone, Copy)]
pub struct TimerConfig {
    /// APIC timer ticks per millisecond (after the divide-by-16).
    pub ticks_per_ms: u32,
    /// The periodic initial count (ticks per interrupt).
    pub initial_count: u32,
}

/// Bring up the Local APIC: map its register page at `mmio_virt` in `space`
/// (uncached, RW+NX), set the global and software enables, accept all
/// priorities (TPR = 0), and install a handler for [`SPURIOUS_VECTOR`]. The
/// timer stays masked until [`start_periodic`]. Returns the APIC id.
///
/// `space` must be the active address space and `mmio_virt` a page-aligned
/// higher-half address not otherwise in use.
///
/// # Errors
/// [`LapicError`] if there is no APIC, it is in x2APIC mode, or the mapping
/// fails.
pub fn init(space: &mut AddressSpace, mmio_virt: u64) -> Result<u32, LapicError> {
    if !crate::cpuid::detect().features.apic {
        return Err(LapicError::NotPresent);
    }
    let msr = rdmsr(IA32_APIC_BASE);
    if msr & APIC_BASE_X2APIC != 0 {
        return Err(LapicError::X2apicMode);
    }
    let phys = msr & APIC_BASE_ADDR_MASK;
    // Intel SDM Vol. 3A §11.4.1: the APIC register page must be mapped
    // strong-uncacheable (PCD | PWT).
    space
        .map(
            mmio_virt,
            phys,
            Flags::WRITABLE | Flags::NO_EXECUTE | Flags::NO_CACHE | Flags::WRITE_THROUGH,
        )
        .map_err(LapicError::Map)?;
    BASE.store(mmio_virt, Ordering::Release);

    // SAFETY: setting the global-enable bit (preserving the base address and
    // every other bit) only turns on the APIC we just mapped; it raises no
    // interrupt until an LVT entry is unmasked and IF is set.
    unsafe { wrmsr(IA32_APIC_BASE, msr | APIC_BASE_ENABLE) };

    idt::register_handler(SPURIOUS_VECTOR, spurious);
    write(REG_LVT_TIMER, LVT_MASKED);
    write(REG_TPR, 0);
    write(REG_SVR, SVR_ENABLE | u32::from(SPURIOUS_VECTOR));
    Ok(read(REG_ID) >> 24)
}

/// Calibrate the APIC timer against the PIT, then start it in periodic mode
/// raising `vector` `hz` times per second. Interrupts are delivered once the
/// caller enables them (RFLAGS.IF).
///
/// # Errors
/// [`LapicError`] if [`init`] has not succeeded, `hz` is unusable, or the
/// timer failed to count during calibration.
pub fn start_periodic(vector: u8, hz: u32) -> Result<TimerConfig, LapicError> {
    if BASE.load(Ordering::Acquire) == 0 {
        return Err(LapicError::NotInitialized);
    }
    if hz == 0 {
        return Err(LapicError::BadFrequency);
    }
    let ticks_per_ms = calibrate()?;
    let initial_count = u32::try_from(u64::from(ticks_per_ms) * 1000 / u64::from(hz))
        .map_err(|_| LapicError::BadFrequency)?;
    if initial_count == 0 {
        return Err(LapicError::BadFrequency);
    }
    write(REG_TIMER_DIVIDE, TIMER_DIVIDE_BY_16);
    write(REG_LVT_TIMER, LVT_TIMER_PERIODIC | u32::from(vector));
    write(REG_TIMER_INITIAL, initial_count); // writing the count starts it
    Ok(TimerConfig {
        ticks_per_ms,
        initial_count,
    })
}

/// Signal end-of-interrupt for the in-service APIC interrupt. Every handler of
/// an APIC-delivered vector (except the spurious one) must call this, or no
/// interrupt of equal or lower priority is delivered again.
pub fn eoi() {
    write(REG_EOI, 0);
}

/// Measure APIC timer ticks (at divide-by-16) per millisecond: run the timer
/// one-shot from its maximum count, masked, across a PIT-timed window. Fails
/// if the PIT does not start a real window or the result is implausible, so a
/// missing PIT can never program a timer hundreds of times too fast (#109).
fn calibrate() -> Result<u32, LapicError> {
    const WINDOW_MS: u32 = CALIBRATION_US / 1000;
    write(REG_TIMER_DIVIDE, TIMER_DIVIDE_BY_16);
    write(REG_LVT_TIMER, LVT_MASKED); // one-shot, no interrupt
    if !crate::pit::start_oneshot(CALIBRATION_US) {
        return Err(LapicError::CalibrationFailed);
    }
    write(REG_TIMER_INITIAL, u32::MAX);
    // Bound the wait by the APIC count: past twice the fastest plausible
    // window, the PIT is not going to fire.
    let limit = MAX_TICKS_PER_MS * WINDOW_MS * 2;
    let elapsed = || u32::MAX - read(REG_TIMER_CURRENT);
    while !crate::pit::oneshot_expired() && elapsed() < limit {
        core::hint::spin_loop();
    }
    let per_ms = elapsed() / WINDOW_MS;
    write(REG_TIMER_INITIAL, 0); // stop
    if !crate::pit::oneshot_expired() || !(MIN_TICKS_PER_MS..=MAX_TICKS_PER_MS).contains(&per_ms) {
        return Err(LapicError::CalibrationFailed);
    }
    Ok(per_ms)
}

/// Spurious APIC interrupt: not a real interrupt, so no EOI (SDM §11.9).
fn spurious(_frame: &mut TrapFrame) {}

/// Pointer to register `reg`, or `None` before [`init`].
fn reg_ptr(reg: usize) -> Option<*mut u32> {
    let base = BASE.load(Ordering::Acquire);
    (base != 0).then(|| (base as usize + reg) as *mut u32)
}

/// Read a 32-bit APIC register (0 before [`init`]).
fn read(reg: usize) -> u32 {
    reg_ptr(reg).map_or(0, |p| {
        // SAFETY: `BASE` is non-zero only after `init` mapped the APIC register
        // page there (uncached, writable); `reg` is one of the in-page,
        // 16-byte-aligned register offsets above, accessed as a 32-bit word as
        // the SDM requires.
        unsafe { p.read_volatile() }
    })
}

/// Write a 32-bit APIC register (a no-op before [`init`]).
fn write(reg: usize, value: u32) {
    if let Some(p) = reg_ptr(reg) {
        // SAFETY: as in `read`; the register page is mapped writable, and every
        // write here is a documented register programming.
        unsafe { p.write_volatile(value) }
    }
}
