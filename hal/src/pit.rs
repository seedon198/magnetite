//! The 8254 Programmable Interval Timer, used only as a fixed-frequency
//! reference to calibrate the Local APIC timer (see [`crate::lapic`]).
//!
//! We use **channel 2** in mode 0 (interrupt on terminal count): its gate and
//! output are wired to port 0x61, so the countdown can be started and polled
//! without an IRQ - and without unmasking the legacy PIC.
//!
//! Reference: Intel 8254 datasheet; port 0x61 is the PC "system control port B".

use crate::io::{inb, outb};

/// The PIT's input clock, in Hz (fixed on PC-compatibles).
const PIT_HZ: u64 = 1_193_182;

const CH2_DATA: u16 = 0x42;
const MODE_CMD: u16 = 0x43;
/// System control port B: bit 0 = channel-2 gate, bit 1 = speaker enable,
/// bit 5 = channel-2 output (read-only).
const PORT_B: u16 = 0x61;
const PORT_B_GATE2: u8 = 1 << 0;
const PORT_B_SPEAKER: u8 = 1 << 1;
const PORT_B_OUT2: u8 = 1 << 5;

/// Mode command: channel 2, lobyte/hibyte access, mode 0, binary.
const CH2_MODE0: u8 = 0b1011_0000;

/// Start a one-shot countdown of `micros` microseconds (at most ~54 ms, the
/// 16-bit counter's range) on channel 2. Poll [`oneshot_expired`] to wait for
/// it.
///
/// Returns `false` if channel 2's output did not go low once programmed: in
/// mode 0 it drops when the mode is written and rises only at terminal count,
/// so an output already high means no working PIT (absent, gated, or port 0x61
/// floating high), and [`oneshot_expired`] would report a window that never
/// ran (#109).
#[must_use]
pub(crate) fn start_oneshot(micros: u32) -> bool {
    // Deliberate truncation: clamp to the 16-bit counter range.
    let count = (PIT_HZ * u64::from(micros) / 1_000_000).clamp(1, 0xffff) as u16;
    let [lo, hi] = count.to_le_bytes();
    // SAFETY: ports 0x42/0x43/0x61 are the 8254 channel-2 data, mode-command,
    // and system-control registers on every PC-compatible machine. We raise the
    // channel-2 gate with the speaker kept off, then program a mode-0 count;
    // channel 2 is wired to nothing but the speaker, so this has no other
    // side effect.
    unsafe {
        let b = inb(PORT_B);
        outb(PORT_B, (b & !PORT_B_SPEAKER) | PORT_B_GATE2);
        outb(MODE_CMD, CH2_MODE0);
        outb(CH2_DATA, lo);
        outb(CH2_DATA, hi); // counting starts on the next PIT clock
    }
    !oneshot_expired()
}

/// Whether the countdown started by [`start_oneshot`] has reached zero.
pub(crate) fn oneshot_expired() -> bool {
    // SAFETY: reading system control port B has no side effects.
    unsafe { inb(PORT_B) & PORT_B_OUT2 != 0 }
}
