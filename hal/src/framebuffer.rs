//! Framebuffer text console.
//!
//! Renders the bundled 8x8 bitmap [`font`](crate::font) onto a linear
//! framebuffer set up by the bootloader, so kernel output is visible on screen
//! (and on real hardware without a serial port). Registered as a second log
//! sink alongside serial.
//!
//! Only 32-bits-per-pixel framebuffers are supported (the common case for UEFI
//! GOP and QEMU); other depths are ignored, leaving serial as the only sink.

use core::fmt;

use crate::font::FONT8X8_BASIC;
use crate::sync::TryLock;

const GLYPH_W: usize = 8;
const GLYPH_H: usize = 8;

/// Parameters describing a bootloader-provided linear framebuffer.
#[derive(Clone, Copy)]
pub struct FramebufferInfo {
    /// Virtual address of the framebuffer.
    pub address: u64,
    /// Visible width in pixels.
    pub width: usize,
    /// Visible height in pixels.
    pub height: usize,
    /// Bytes per scanline.
    pub pitch: usize,
    /// Bits per pixel.
    pub bpp: u16,
    /// Channel bit positions within a pixel.
    pub red_shift: u8,
    /// Channel bit positions within a pixel.
    pub green_shift: u8,
    /// Channel bit positions within a pixel.
    pub blue_shift: u8,
}

struct Console {
    info: FramebufferInfo,
    cols: usize,
    rows: usize,
    col: usize,
    row: usize,
    fg: u32,
    bg: u32,
}

/// The console. Only the logger's single drainer writes it, so a `TryLock`
/// suffices, and a scroll runs with interrupts on (#116).
static CONSOLE: TryLock<Option<Console>> = TryLock::new(None);

/// Initialize the framebuffer console. A no-op for unsupported pixel formats,
/// in which case screen output is simply skipped.
pub fn init(info: FramebufferInfo) {
    if info.bpp != 32 {
        return;
    }
    let fg = pack(&info, 0xAA, 0xAA, 0xAA);
    let bg = pack(&info, 0x00, 0x00, 0x00);
    let console = Console {
        cols: info.width / GLYPH_W,
        rows: info.height / GLYPH_H,
        col: 0,
        row: 0,
        fg,
        bg,
        info,
    };
    console.clear();
    if let Some(mut slot) = CONSOLE.try_lock() {
        *slot = Some(console);
    }
}

/// Write a string to the framebuffer console, if one is initialized. Used by
/// the kernel logger's drainer; a no-op when there is no framebuffer, or if
/// another writer holds the console.
pub fn write_str(s: &str) {
    if let Some(mut console) = CONSOLE.try_lock() {
        if let Some(console) = console.as_mut() {
            console.write(s);
        }
    }
}

/// Pack an (r, g, b) triple into a pixel using the channel shifts.
fn pack(info: &FramebufferInfo, r: u32, g: u32, b: u32) -> u32 {
    (r << info.red_shift) | (g << info.green_shift) | (b << info.blue_shift)
}

impl Console {
    fn put_pixel(&self, x: usize, y: usize, color: u32) {
        let offset = y * self.info.pitch + x * 4;
        // SAFETY: `x < width` and `y < height` are guaranteed by callers (glyph
        // cells and scroll stay within bounds), so `offset` is within the
        // framebuffer the bootloader mapped. 32bpp was checked in `init`.
        unsafe {
            let ptr = (self.info.address as *mut u8).add(offset).cast::<u32>();
            ptr.write_volatile(color);
        }
    }

    fn clear(&self) {
        for y in 0..self.info.height {
            for x in 0..self.info.width {
                self.put_pixel(x, y, self.bg);
            }
        }
    }

    fn draw_glyph(&self, c: u8, col: usize, row: usize) {
        let glyph = &FONT8X8_BASIC[(c & 0x7F) as usize];
        let x0 = col * GLYPH_W;
        let y0 = row * GLYPH_H;
        for (dy, bits) in glyph.iter().enumerate() {
            for dx in 0..GLYPH_W {
                let color = if bits & (1 << dx) != 0 {
                    self.fg
                } else {
                    self.bg
                };
                self.put_pixel(x0 + dx, y0 + dy, color);
            }
        }
    }

    fn scroll(&mut self) {
        let row_bytes = self.info.pitch * GLYPH_H;
        let total = self.info.pitch * self.info.height;
        // SAFETY: both ranges lie within the mapped framebuffer; we move all but
        // the top glyph row up by one glyph row, then clear the exposed bottom.
        unsafe {
            let base = self.info.address as *mut u8;
            core::ptr::copy(base.add(row_bytes), base, total - row_bytes);
        }
        let last = self.rows - 1;
        for y in (last * GLYPH_H)..self.info.height {
            for x in 0..self.info.width {
                self.put_pixel(x, y, self.bg);
            }
        }
    }

    fn newline(&mut self) {
        self.col = 0;
        if self.row + 1 >= self.rows {
            self.scroll();
        } else {
            self.row += 1;
        }
    }

    fn write(&mut self, s: &str) {
        for byte in s.bytes() {
            match byte {
                b'\n' => self.newline(),
                b'\r' => self.col = 0,
                b'\t' => {
                    let spaces = 4 - (self.col % 4);
                    for _ in 0..spaces {
                        self.put_char(b' ');
                    }
                }
                0x20..=0x7E => self.put_char(byte),
                _ => self.put_char(b'?'),
            }
        }
    }

    fn put_char(&mut self, c: u8) {
        if self.col >= self.cols {
            self.newline();
        }
        self.draw_glyph(c, self.col, self.row);
        self.col += 1;
    }
}

/// A [`fmt::Write`] adapter over the framebuffer console.
pub struct Writer;

impl fmt::Write for Writer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write_str(s);
        Ok(())
    }
}
