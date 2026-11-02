//! Kernel logging: leveled macros over the serial and framebuffer sinks.
//!
//! Records at or above the current level are formatted as `[LEVEL] message`
//! into a **pending ring** under a short interrupts-off lock, then **drained**
//! to COM1 and the framebuffer console with interrupts on (#116). One caller
//! drains at a time (a try-acquired flag): anyone who logs while another is
//! draining, an interrupt handler included, only queues its record, and the
//! drainer writes it out. So the slow part, a byte at a time over the UART
//! and a framebuffer scroll, never runs with interrupts masked. If the ring
//! fills, new bytes are dropped and the drainer reports how many.
//!
//! The logger is deliberately **not** used by the panic handler: a panic can
//! occur while the ring lock is held, so the handler writes COM1 directly.

use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use hal::serial::SerialPort;
use hal::sync::SpinLock;

/// Log severity, ordered from most to least severe.
///
/// Not every variant is constructed yet - the full set is used across M1 as
/// subsystems start logging at their natural levels.
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// A fault or failure.
    Error = 1,
    /// Something unexpected that is not fatal.
    Warn = 2,
    /// Normal, noteworthy progress.
    Info = 3,
    /// Detailed information for debugging.
    Debug = 4,
    /// Very verbose tracing.
    Trace = 5,
}

impl Level {
    /// The level's short uppercase name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        }
    }
}

static MAX_LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);

/// Bytes the pending ring holds: many records' worth.
const PENDING_BYTES: usize = 8192;
/// Bytes the drainer moves per turn of the lock.
const CHUNK: usize = 256;

/// Formatted bytes waiting to be written to the sinks.
struct Pending {
    buf: [u8; PENDING_BYTES],
    /// Index of the oldest byte.
    head: usize,
    len: usize,
    /// Bytes dropped because the ring was full, not yet reported.
    lost: usize,
}

impl Pending {
    /// Move up to `out.len()` of the oldest bytes into `out`, stopping before
    /// an incomplete UTF-8 sequence at the end so text is never split inside a
    /// character. Returns how many were moved.
    fn take(&mut self, out: &mut [u8]) -> usize {
        let mut n = self.len.min(out.len());
        for (i, b) in out[..n].iter_mut().enumerate() {
            *b = self.buf[(self.head + i) % PENDING_BYTES];
        }
        if let Err(e) = core::str::from_utf8(&out[..n]) {
            if e.error_len().is_none() && e.valid_up_to() > 0 && n < self.len {
                n = e.valid_up_to(); // the rest of that character is still queued
            }
        }
        self.head = (self.head + n) % PENDING_BYTES;
        self.len -= n;
        n
    }
}

impl fmt::Write for Pending {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &b in s.as_bytes() {
            if self.len == PENDING_BYTES {
                self.lost += 1;
                continue;
            }
            self.buf[(self.head + self.len) % PENDING_BYTES] = b;
            self.len += 1;
        }
        Ok(())
    }
}

static PENDING: SpinLock<Pending> = SpinLock::new(Pending {
    buf: [0; PENDING_BYTES],
    head: 0,
    len: 0,
    lost: 0,
});

/// Set while one caller drains the ring to the sinks.
static DRAINING: AtomicBool = AtomicBool::new(false);

/// Initialize the logging backend (the serial sink). Call once, early in
/// `kmain`, before using the logging macros.
pub fn init() {
    SerialPort::com1().init();
}

/// Queue `args` as one record, then drain the ring unless someone else is.
fn emit(args: fmt::Arguments) {
    use fmt::Write;
    // Formatting runs with interrupts off, but only copies into memory.
    let _ = PENDING.lock().write_fmt(args);
    drain();
}

/// Write everything queued to the serial port and the framebuffer console,
/// with interrupts as the caller had them. If another caller is draining, it
/// will write what was just queued, so return at once.
fn drain() {
    use fmt::Write;
    loop {
        if DRAINING.swap(true, Ordering::Acquire) {
            return;
        }
        let serial = SerialPort::com1_configured();
        let mut chunk = [0u8; CHUNK];
        loop {
            let (n, lost) = {
                let mut p = PENDING.lock();
                (p.take(&mut chunk), core::mem::take(&mut p.lost))
            };
            if n == 0 && lost == 0 {
                break;
            }
            let text = core::str::from_utf8(&chunk[..n]).unwrap_or("\u{fffd}");
            serial.write_bytes(text.as_bytes());
            hal::framebuffer::write_str(text);
            if lost > 0 {
                let mut note = SerialPort::com1_configured();
                let _ = writeln!(note, "[WARN] log: {lost} bytes dropped (ring full)");
            }
        }
        DRAINING.store(false, Ordering::Release);
        // A record queued after the last take but before the release would
        // wait for the next log call: go round again if one is there.
        if PENDING.lock().len == 0 {
            return;
        }
    }
}

/// Set the maximum severity that will be emitted; records above it are dropped.
pub fn set_max_level(level: Level) {
    MAX_LEVEL.store(level as u8, Ordering::Relaxed);
}

/// Whether a record at `level` would currently be emitted.
#[must_use]
pub fn enabled(level: Level) -> bool {
    (level as u8) <= MAX_LEVEL.load(Ordering::Relaxed)
}

/// Emit one formatted record to every sink (serial, and the framebuffer console
/// once initialized). Prefer the [`info!`](crate::info) family of macros over
/// calling this directly.
pub fn write(level: Level, args: fmt::Arguments) {
    if !enabled(level) {
        return;
    }
    emit(format_args!("[{}] {}\n", level.as_str(), args));
}

/// Write one line to every sink with no level prefix, regardless of the
/// current level. For the boot banner, not for log records.
pub fn raw(args: fmt::Arguments) {
    emit(format_args!("{args}\n"));
}

/// Emit a log record at an explicit [`Level`]. The level-specific macros
/// ([`info!`](crate::info), etc.) are usually clearer.
#[macro_export]
macro_rules! log {
    ($level:expr, $($arg:tt)*) => {
        $crate::log::write($level, core::format_args!($($arg)*))
    };
}

/// Log at [`Level::Error`](crate::log::Level::Error).
#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => {
        $crate::log::write($crate::log::Level::Error, core::format_args!($($arg)*))
    };
}

/// Log at [`Level::Warn`](crate::log::Level::Warn).
#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => {
        $crate::log::write($crate::log::Level::Warn, core::format_args!($($arg)*))
    };
}

/// Log at [`Level::Info`](crate::log::Level::Info).
#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {
        $crate::log::write($crate::log::Level::Info, core::format_args!($($arg)*))
    };
}

/// Log at [`Level::Debug`](crate::log::Level::Debug).
#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {
        $crate::log::write($crate::log::Level::Debug, core::format_args!($($arg)*))
    };
}

/// Log at [`Level::Trace`](crate::log::Level::Trace).
#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => {
        $crate::log::write($crate::log::Level::Trace, core::format_args!($($arg)*))
    };
}
