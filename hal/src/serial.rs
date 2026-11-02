//! 16550-compatible UART serial driver.
//!
//! The serial port is the kernel's primary early-output channel: it works
//! before any graphics are set up, is trivial to capture in QEMU
//! (`-serial stdio`), and is how CI reads kernel output. This driver is polled
//! (no interrupts); it is sufficient for logging and panics.
//!
//! See the OSDev wiki "Serial Ports" article and the PC16550D datasheet for the
//! register layout.

use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::io::{inb, outb};

/// I/O base address of COM1 on PC-compatible hardware.
pub const COM1_BASE: u16 = 0x3F8;

// Register offsets from the port base (with the Divisor Latch Access Bit clear
// unless noted).
const REG_DATA: u16 = 0; // RBR/THR (and divisor low when DLAB=1)
const REG_IER: u16 = 1; // Interrupt Enable (divisor high when DLAB=1)
const REG_FCR: u16 = 2; // FIFO Control (write)
const REG_LCR: u16 = 3; // Line Control
const REG_MCR: u16 = 4; // Modem Control
const REG_LSR: u16 = 5; // Line Status

const LCR_DLAB: u8 = 0x80; // Divisor Latch Access Bit
const LCR_8N1: u8 = 0x03; // 8 data bits, no parity, 1 stop bit
const LSR_THR_EMPTY: u8 = 0x20; // Transmitter holding register empty

/// Whether COM1 has been initialized and found present (#117).
static COM1_PRESENT: AtomicBool = AtomicBool::new(false);

/// Polls of the line-status register before a byte is dropped: a wedged or
/// absent-but-detected UART must not hang the kernel (#116). About 100 ms on
/// real hardware, far longer than one byte takes at 115200 baud.
const TX_SPIN_LIMIT: u32 = 100_000;

/// A 16550 UART at a fixed I/O base.
///
/// Construct with [`SerialPort::com1`] (or [`SerialPort::new`]), call
/// [`SerialPort::init`] once, then write with the [`fmt::Write`] implementation
/// or [`SerialPort::write_bytes`]. Writes are silently dropped if the port is
/// not present (see [`SerialPort::init`]).
pub struct SerialPort {
    base: u16,
    present: bool,
}

impl SerialPort {
    /// Create a handle for the UART at I/O base `base`. Does not touch hardware
    /// until [`init`](Self::init) is called.
    #[must_use]
    pub const fn new(base: u16) -> Self {
        Self {
            base,
            present: false,
        }
    }

    /// Create a handle for COM1.
    #[must_use]
    pub const fn com1() -> Self {
        Self::new(COM1_BASE)
    }

    /// COM1 as the first [`init`](Self::init) configured it, without touching
    /// its registers, so the crash reporters and the logger's drainer do not
    /// reset the FIFO and drop the bytes still in it (#117). Initializes the
    /// port only if nothing has yet.
    #[must_use]
    pub fn com1_configured() -> Self {
        let mut port = Self::com1();
        if COM1_PRESENT.load(Ordering::Acquire) {
            port.present = true;
        } else {
            port.init();
        }
        port
    }

    fn write_reg(&self, offset: u16, value: u8) {
        // SAFETY: `base + offset` addresses a 16550 UART register (offset is one
        // of the small constants above). Writing UART registers has only the
        // intended device side effects.
        unsafe { outb(self.base + offset, value) }
    }

    fn read_reg(&self, offset: u16) -> u8 {
        // SAFETY: as in `write_reg`; reading a 16550 register has no memory
        // effects and no unintended side effects.
        unsafe { inb(self.base + offset) }
    }

    /// Initialize the UART to 115200 baud, 8N1, FIFOs enabled, and run a
    /// loopback self-test. If the self-test fails (no UART present, as can
    /// happen on real hardware), the port is marked absent and all later writes
    /// become no-ops instead of hanging.
    pub fn init(&mut self) {
        self.write_reg(REG_IER, 0x00); // disable interrupts
        self.write_reg(REG_LCR, LCR_DLAB); // enable divisor latch
        self.write_reg(REG_DATA, 0x01); // divisor low  = 1  => 115200 baud
        self.write_reg(REG_IER, 0x00); // divisor high = 0
        self.write_reg(REG_LCR, LCR_8N1); // 8N1, divisor latch off
        self.write_reg(REG_FCR, 0xC7); // enable+clear FIFOs, 14-byte threshold
        self.write_reg(REG_MCR, 0x0B); // DTR, RTS, OUT2

        // Loopback self-test: echo a byte back through the UART.
        self.write_reg(REG_MCR, 0x1E); // loopback mode, RTS, OUT1/OUT2
        self.write_reg(REG_DATA, 0xAE);
        if self.read_reg(REG_DATA) != 0xAE {
            self.present = false;
            return;
        }

        self.write_reg(REG_MCR, 0x0F); // normal operation: DTR, RTS, OUT1, OUT2
        self.present = true;
        if self.base == COM1_BASE {
            COM1_PRESENT.store(true, Ordering::Release);
        }
    }

    /// Whether a working UART was detected by [`init`](Self::init).
    #[must_use]
    pub fn is_present(&self) -> bool {
        self.present
    }

    fn can_transmit(&self) -> bool {
        self.read_reg(REG_LSR) & LSR_THR_EMPTY != 0
    }

    /// Write a single raw byte, waiting until the transmit holding register is
    /// free, but at most [`TX_SPIN_LIMIT`] polls: past that the byte is
    /// dropped (#116). No-op if the port is absent.
    pub fn write_byte(&self, byte: u8) {
        if !self.present {
            return;
        }
        for _ in 0..TX_SPIN_LIMIT {
            if self.can_transmit() {
                self.write_reg(REG_DATA, byte);
                return;
            }
            core::hint::spin_loop();
        }
    }

    /// Write raw bytes, translating `\n` into `\r\n` for terminals.
    pub fn write_bytes(&self, bytes: &[u8]) {
        for &byte in bytes {
            if byte == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(byte);
        }
    }
}

impl fmt::Write for SerialPort {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write_bytes(s.as_bytes());
        Ok(())
    }
}
