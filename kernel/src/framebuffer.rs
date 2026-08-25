// kernel/src/framebuffer.rs
//
// Pixel framebuffer driver — Phase 1.
//
// WHY NOT VGA TEXT MODE (0xb8000)?
// ───────────────────────────────
// VGA text mode gives you 80×25 characters, hardwired. That's fine for a hello-world
// demo but it's a dead end: you can't draw pixels, you're stuck with a 9×16 font,
// and you can't implement windows, a cursor, or any real UI later.
//
// The `bootloader` 0.11 crate requests a VESA/VBE framebuffer from the BIOS and passes
// it to us via `BootInfo::framebuffer`. We get a pointer to a flat array of pixels at
// whatever resolution the firmware supports (QEMU default: 1280×800 or 800×600).
//
// The tradeoff: we must render text ourselves. There is no "put a character at (col, row)"
// hardware instruction — we render each glyph pixel-by-pixel using a bitmap font.
//
// FONT
// ────
// We use `noto-sans-mono-bitmap` which provides pre-rasterized glyphs as const arrays.
// At 16px height, each glyph is about 9–10px wide. Rendering is: for each lit pixel
// in the glyph bitmap, write the corresponding pixel in the framebuffer.
//
// PIXEL FORMAT
// ────────────
// The framebuffer pixel format is reported in `FrameBufferInfo::pixel_format`.
// Common values: Rgb (red at offset 0), Bgr (blue at offset 0), or U8 (greyscale).
// We handle Rgb and Bgr (QEMU uses Bgr by default with VESA).
//
// SCROLLING
// ─────────
// When we reach the bottom of the screen we scroll: copy all rows up by one character
// height using a single `copy_within` on the raw byte buffer, then clear the last row.

use bootloader_api::info::{FrameBuffer, FrameBufferInfo, PixelFormat};
use core::fmt;
use noto_sans_mono_bitmap::{
    get_raster, get_raster_width, FontWeight, RasterHeight, RasterizedChar,
};
use spin::Mutex;

// ─── Font configuration ───────────────────────────────────────────────────────

/// Height of each rendered glyph in pixels.
const CHAR_RASTER_HEIGHT: RasterHeight = RasterHeight::Size16;

/// Width of each glyph in pixels (constant for a monospace font at a fixed height).
const CHAR_RASTER_WIDTH: usize = get_raster_width(FontWeight::Regular, CHAR_RASTER_HEIGHT);

/// Pixels of padding between characters and between lines.
const LETTER_SPACING: usize = 0;
const LINE_SPACING: usize = 2;
const BORDER_PADDING: usize = 1;

/// Total height consumed per line of text.
const LINE_HEIGHT: usize = CHAR_RASTER_HEIGHT as usize + LINE_SPACING;

// ─── Global framebuffer writer ────────────────────────────────────────────────

/// The global kernel text renderer, protected by a spinlock.
///
/// Initialized exactly once by `framebuffer::init()` during `kernel_main`.
/// We use `Mutex<Option<...>>` rather than `lazy_static` because initialization
/// requires a `FrameBuffer` reference that only exists at runtime (passed by bootloader).
pub static FRAMEBUFFER_WRITER: Mutex<Option<FrameBufferWriter>> = Mutex::new(None);

/// Initialize the global framebuffer writer from the bootloader-provided `FrameBuffer`.
///
/// # Safety
/// Must be called exactly once, before any `print!` usage.
/// The `FrameBuffer` reference must be valid for the entire kernel lifetime.
pub fn init(framebuffer: &'static mut FrameBuffer) {
    let info = framebuffer.info();
    let buffer = framebuffer.buffer_mut();
    let writer = FrameBufferWriter::new(buffer, info);
    *FRAMEBUFFER_WRITER.lock() = Some(writer);
}

// ─── FrameBufferWriter ────────────────────────────────────────────────────────

/// Renders text to a pixel framebuffer.
///
/// Maintains a cursor (x_pos, y_pos in pixels) and handles newlines + scrolling.
pub struct FrameBufferWriter {
    /// Raw pixel byte buffer. Layout: `buffer[y * stride * bpp + x * bpp + color_offset]`.
    buffer: &'static mut [u8],
    /// Metadata: resolution, pixel format, bytes-per-pixel, stride.
    info: FrameBufferInfo,
    /// Current cursor X position in pixels.
    x_pos: usize,
    /// Current cursor Y position in pixels (top of the current line).
    y_pos: usize,
}

impl FrameBufferWriter {
    pub fn new(buffer: &'static mut [u8], info: FrameBufferInfo) -> Self {
        let mut writer = Self {
            buffer,
            info,
            x_pos: BORDER_PADDING,
            y_pos: BORDER_PADDING,
        };
        writer.clear();
        writer
    }

    /// Fill the entire framebuffer with black.
    pub fn clear(&mut self) {
        self.buffer.fill(0);
        self.x_pos = BORDER_PADDING;
        self.y_pos = BORDER_PADDING;
    }

    /// Framebuffer width in pixels.
    #[inline]
    fn width(&self) -> usize {
        self.info.width
    }

    /// Framebuffer height in pixels.
    #[inline]
    fn height(&self) -> usize {
        self.info.height
    }

    /// Write a single character to the framebuffer at the current cursor position.
    fn write_char(&mut self, c: char) {
        match c {
            '\n' => self.newline(),
            '\r' => { self.x_pos = BORDER_PADDING; }
            c => {
                // Wrap to next line if the glyph won't fit on the current row.
                let new_xpos = self.x_pos + CHAR_RASTER_WIDTH + LETTER_SPACING;
                if new_xpos >= self.width() {
                    self.newline();
                }
                // Also scroll if we've somehow reached the bottom (shouldn't happen
                // if newline() handles it, but defensive check).
                if self.y_pos + LINE_HEIGHT >= self.height() {
                    self.scroll_up();
                }
                self.render_glyph(self.get_char_raster(c));
            }
        }
    }

    /// Retrieve the rasterized bitmap for a character.
    /// Falls back to a space glyph for unsupported codepoints.
    fn get_char_raster(&self, c: char) -> RasterizedChar {
        get_raster(c, FontWeight::Regular, CHAR_RASTER_HEIGHT)
            .unwrap_or_else(|| {
                // Fall back to '?' for unsupported chars
                get_raster('?', FontWeight::Regular, CHAR_RASTER_HEIGHT)
                    .unwrap_or_else(|| {
                        get_raster(' ', FontWeight::Regular, CHAR_RASTER_HEIGHT)
                            .expect("space glyph must always exist")
                    })
            })
    }

    /// Render a rasterized glyph at the current cursor position and advance the cursor.
    fn render_glyph(&mut self, raster: RasterizedChar) {
        for (y, row) in raster.raster().iter().enumerate() {
            for (x, byte) in row.iter().enumerate() {
                self.write_pixel(self.x_pos + x, self.y_pos + y, *byte);
            }
        }
        self.x_pos += raster.width() + LETTER_SPACING;
    }

    /// Write a single pixel at (x, y) with the given intensity (0=black, 255=white).
    ///
    /// Handles Rgb and Bgr pixel formats — for greyscale font rendering, we just
    /// set all three channels to the same intensity value.
    #[inline]
    fn write_pixel(&mut self, x: usize, y: usize, intensity: u8) {
        // Guard against out-of-bounds writes (can happen at borders)
        if x >= self.width() || y >= self.height() {
            return;
        }

        // Compute the byte offset of the first byte of pixel (x, y).
        // stride = number of PIXELS per row (may be wider than width due to alignment padding).
        let pixel_offset = y * self.info.stride + x;
        let byte_offset = pixel_offset * self.info.bytes_per_pixel;

        // Map intensity → RGB bytes based on the actual pixel format.
        // For white-on-black text, R=G=B=intensity.
        let (r, g, b) = (intensity, intensity, intensity);

        match self.info.pixel_format {
            PixelFormat::Rgb => {
                self.buffer[byte_offset]     = r;
                self.buffer[byte_offset + 1] = g;
                self.buffer[byte_offset + 2] = b;
            }
            PixelFormat::Bgr => {
                self.buffer[byte_offset]     = b;
                self.buffer[byte_offset + 1] = g;
                self.buffer[byte_offset + 2] = r;
            }
            PixelFormat::U8 => {
                // Greyscale: use luminance approximation (or just intensity directly)
                self.buffer[byte_offset] = intensity;
            }
            other => {
                // Unknown format — try to write a single byte and hope for the best.
                // In practice QEMU always reports Bgr for VESA.
                let _ = other;
                self.buffer[byte_offset] = intensity;
            }
        }
    }

    /// Advance to the next line. Scroll up if we've hit the bottom of the screen.
    fn newline(&mut self) {
        self.y_pos += LINE_HEIGHT;
        self.x_pos = BORDER_PADDING;
        if self.y_pos + LINE_HEIGHT >= self.height() {
            self.scroll_up();
        }
    }

    /// Scroll the framebuffer contents up by one line (LINE_HEIGHT pixels).
    ///
    /// Algorithm:
    ///   1. Copy all rows starting at y=LINE_HEIGHT upward by LINE_HEIGHT pixels.
    ///      This is a single bulk memmove using Rust's `copy_within`.
    ///   2. Clear the newly exposed bottom row.
    ///   3. Move the cursor up by LINE_HEIGHT pixels (same logical position, now one line higher).
    fn scroll_up(&mut self) {
        let bpp = self.info.bytes_per_pixel;
        let stride = self.info.stride;

        // Number of bytes in one full line of pixels.
        let line_bytes = LINE_HEIGHT * stride * bpp;

        // Source range: everything from the second line onward.
        let src_start = line_bytes;
        let src_end = self.buffer.len();

        // Destination: beginning of the buffer (shifted up by one line).
        self.buffer.copy_within(src_start..src_end, 0);

        // Zero out the newly exposed last-line bytes.
        let clear_start = src_end - line_bytes;
        self.buffer[clear_start..].fill(0);

        // Move cursor up by one line (it was already at the last line's y_pos).
        if self.y_pos >= LINE_HEIGHT {
            self.y_pos -= LINE_HEIGHT;
        }
    }
}

/// Implement `fmt::Write` so we can use Rust's `write!`/`writeln!` formatting
/// machinery with our FrameBufferWriter. This is what `print!` calls into.
impl fmt::Write for FrameBufferWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.chars() {
            self.write_char(c);
        }
        Ok(())
    }
}

// ─── Macros ───────────────────────────────────────────────────────────────────

/// Kernel `print!` — renders formatted text to the pixel framebuffer.
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::framebuffer::_print(format_args!($($arg)*)));
}

/// Kernel `println!` — renders formatted text + newline to the pixel framebuffer.
#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($($arg:tt)*) => ($crate::print!("{}\n", format_args!($($arg)*)));
}

/// Internal — called by the `print!` macro.
///
/// Disables interrupts while holding the framebuffer lock to avoid deadlock:
/// if an interrupt fires while we hold the lock, and the handler also tries to
/// print (e.g., the keyboard handler), it would spin forever.
#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    use core::fmt::Write;
    use x86_64::instructions::interrupts;

    interrupts::without_interrupts(|| {
        let mut guard = FRAMEBUFFER_WRITER.lock();
        if let Some(writer) = guard.as_mut() {
            writer.write_fmt(args).unwrap();
        }
    });
}
