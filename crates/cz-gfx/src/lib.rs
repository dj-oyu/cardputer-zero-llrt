//! Cardputer Zero 320x170 RGB565 graphics engine: an offscreen canvas with silently clipped
//! primitives, an embedded 8x8 ASCII bitmap font, and a `swash`-powered TTF/OTF/TTC glyph
//! rasterizer for proportional fonts (including CJK).

mod font_data;

use std::fs;
use std::io::{self, Read};
use std::path::Path;

use swash::scale::{image::Content, image::Image, Render, Source, ScaleContext};
use swash::{FontRef as SwashFontRef, GlyphId};
use read_fonts::{FontRef as RawFontRef, CollectionRef, TableProvider};

/// Screen width in pixels.
pub const WIDTH: usize = 320;
/// Screen height in pixels.
pub const HEIGHT: usize = 170;

/// Pack 8-bit RGB channels into an RGB565 pixel value (little-endian-ready).
pub fn rgb565(r: u8, g: u8, b: u8) -> u16 {
    (((r as u16) >> 3) << 11) | (((g as u16) >> 2) << 5) | ((b as u16) >> 3)
}

// ---------------------------------------------------------------------------
// Embedded font lookup
// ---------------------------------------------------------------------------

/// Look up the 8 row bytes of the built-in 8x8 bitmap glyph for `code` (`None` outside
/// printable ASCII; leftmost pixel is bit 7).
pub fn glyph_rows(code: u8) -> Option<[u8; 8]> {
    if !((0x20..=0x7E).contains(&code)) {
        return None;
    }
    // FONT8X8 is sorted by code.
    let idx = font_data::FONT8X8.partition_point(|(c, _)| *c <= code) - 1;
    let (c, rows) = font_data::FONT8X8[idx];
    if c == code {
        Some(rows)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Canvas
// ---------------------------------------------------------------------------

/// Offscreen 320x170 RGB565 canvas (row-major, little-endian on flush). All drawing methods
/// clip out-of-bounds coordinates silently and never panic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Canvas {
    pixels: Vec<u16>,
}

impl Default for Canvas {
    fn default() -> Self {
        Self::new()
    }
}

impl Canvas {
    /// Create a black (all zero) canvas.
    pub fn new() -> Self {
        Self {
            pixels: vec![0u16; WIDTH * HEIGHT],
        }
    }

    #[inline]
    fn in_bounds(&self, x: i32, y: i32) -> bool {
        x >= 0 && y >= 0 && (x as usize) < WIDTH && (y as usize) < HEIGHT
    }

    #[inline]
    fn idx(x: i32, y: i32) -> usize {
        (y as usize) * WIDTH + (x as usize)
    }

    /// Fill the whole canvas with an RGB565 color.
    pub fn clear(&mut self, color: u16) {
        self.pixels.iter_mut().for_each(|p| *p = color);
    }

    /// Set one pixel (silently clipped when out of bounds).
    pub fn set_pixel(&mut self, x: i32, y: i32, color: u16) {
        if self.in_bounds(x, y) {
            self.pixels[Self::idx(x, y)] = color;
        }
    }

    /// Read one pixel; returns 0 when out of bounds.
    pub fn get_pixel(&self, x: i32, y: i32) -> u16 {
        if self.in_bounds(x, y) {
            self.pixels[Self::idx(x, y)]
        } else {
            0
        }
    }

    /// Clamp a `[start, start+len)` i32 span into `0..limit`, returning an inclusive-empty
    /// range as `(start_usize, end_usize)` where end is exclusive.
    fn clip_span(start: i32, len: i32, limit: usize) -> (usize, usize) {
        let s = start.max(0) as i64;
        let e = (start as i64 + len as i64).clamp(0, limit as i64);
        if e <= s {
            (0, 0)
        } else {
            (s as usize, e as usize)
        }
    }

    /// Fill a rectangle clipped to the display bounds.
    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u16) {
        let (x0, x1) = Self::clip_span(x, w, WIDTH);
        let (y0, y1) = Self::clip_span(y, h, HEIGHT);
        for yy in y0..y1 {
            let row = yy * WIDTH;
            for xx in x0..x1 {
                self.pixels[row + xx] = color;
            }
        }
    }

    /// 1px outline rectangle (interior untouched), clipped silently.
    pub fn draw_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u16) {
        if w <= 0 || h <= 0 {
            return;
        }
        // Border rows/cols only when inside bounds.
        for xx in x..x + w {
            self.set_pixel(xx, y, color);
            self.set_pixel(xx, y + h - 1, color);
        }
        for yy in y..y + h {
            self.set_pixel(x, yy, color);
            self.set_pixel(x + w - 1, yy, color);
        }
    }

    /// Bresenham line, clipped at the display edges.
    pub fn draw_line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, color: u16) {
        if x0 == x1 && y0 == y1 {
            self.set_pixel(x0, y0, color);
            return;
        }
        // Classic integer Bresenham using i64 so extreme coordinates cannot overflow.
        let dx = (x1 as i64 - x0 as i64).abs();
        let dy = (y1 as i64 - y0 as i64).abs();
        let sx = if x0 < x1 { 1i64 } else { -1i64 };
        let sy = if y0 < y1 { 1i64 } else { -1i64 };
        let mut xi = x0 as i64;
        let mut yi = y0 as i64;
        if dx >= dy {
            let mut err = dx / 2;
            loop {
                if self.in_bounds(xi as i32, yi as i32) {
                    self.pixels[Self::idx(xi as i32, yi as i32)] = color;
                }
                if xi == x1 as i64 {
                    break;
                }
                xi += sx;
                err -= dy;
                if err < 0 {
                    yi += sy;
                    err += dx;
                }
            }
        } else {
            let mut err = dy / 2;
            loop {
                if self.in_bounds(xi as i32, yi as i32) {
                    self.pixels[Self::idx(xi as i32, yi as i32)] = color;
                }
                if yi == y1 as i64 {
                    break;
                }
                yi += sy;
                err -= dx;
                if err < 0 {
                    xi += sx;
                    err += dy;
                }
            }
        }
    }

    /// Circle outline using midpoint circle algorithm (clipped).
    pub fn draw_circle(&mut self, cx: i32, cy: i32, r: i32, color: u16) {
        if r < 0 {
            return;
        }
        if r == 0 {
            self.set_pixel(cx, cy, color);
            return;
        }
        let mut x = r;
        let mut y = 0i32;
        let mut err = 1 - r;
        loop {
            self.set_pixel(cx + x, cy + y, color);
            self.set_pixel(cx - x, cy + y, color);
            self.set_pixel(cx + x, cy - y, color);
            self.set_pixel(cx - x, cy - y, color);
            self.set_pixel(cx + y, cy + x, color);
            self.set_pixel(cx - y, cy + x, color);
            self.set_pixel(cx + y, cy - x, color);
            self.set_pixel(cx - y, cy - x, color);
            y += 1;
            if err < 0 {
                err += 2 * y + 1;
            } else {
                x -= 1;
                err += 2 * (y - x) + 1;
            }
            if y > x {
                break;
            }
        }
    }

    /// Solid circle (clipped).
    pub fn fill_circle(&mut self, cx: i32, cy: i32, r: i32, color: u16) {
        if r < 0 {
            return;
        }
        let r64 = r as i64;
        let r2 = r64 * r64;
        let (x0, x1) = Self::clip_span(cx - r, 2 * r + 1, WIDTH);
        let (y0, y1) = Self::clip_span(cy - r, 2 * r + 1, HEIGHT);
        for yy in y0..y1 {
            let dy = yy as i64 - cy as i64;
            for xx in x0..x1 {
                let dx = xx as i64 - cx as i64;
                if dx * dx + dy * dy <= r2 {
                    self.pixels[yy * WIDTH + xx] = color;
                }
            }
        }
    }

    /// Blit a little-endian RGB565 source region; source bytes shorter than the clipped
    /// destination are treated as missing and skipped.
    pub fn blit_rgb565(&mut self, x: i32, y: i32, w: i32, h: i32, data: &[u8]) {
        if w <= 0 || h <= 0 {
            return;
        }
        let (dx0, dx1) = Self::clip_span(x, w, WIDTH);
        let (dy0, dy1) = Self::clip_span(y, h, HEIGHT);
        if dx0 >= dx1 || dy0 >= dy1 {
            return;
        }
        let w64 = w as i64;
        for yy in dy0..dy1 {
            let sy = yy as i64 - y as i64;
            let src_row = sy * w64 * 2;
            for xx in dx0..dx1 {
                let sx = xx as i64 - x as i64;
                let off = src_row + sx * 2;
                if off < 0 {
                    continue;
                }
                let off = off as usize;
                if off + 2 > data.len() {
                    continue;
                }
                let px = u16::from_le_bytes([data[off], data[off + 1]]);
                self.pixels[yy * WIDTH + xx] = px;
            }
        }
    }

    /// Render ASCII text using the embedded 8x8 bitmap font. `scale` multiplies glyph pixels
    /// (minimum 1); each character advances by `8 * scale`. `\n` moves to the next line back
    /// at the starting x.
    pub fn draw_text_builtin(&mut self, x: i32, y: i32, text: &str, color: u16, scale: u32) {
        let scale = scale.max(1) as i32;
        let start_x = x;
        let mut cx = x;
        let mut cy = y;
        for ch in text.chars() {
            if ch == '\n' {
                cx = start_x;
                cy += 8 * scale;
                continue;
            }
            let code = ch as u32;
            if (0x20..=0x7E).contains(&code) {
                if let Some(rows) = glyph_rows(code as u8) {
                    for (ry, row) in rows.iter().enumerate() {
                        for bit in 0..8 {
                            if (row >> bit) & 1 != 0 {
                                // Leftmost pixel is bit 7 -> column = 7 - bit
                                let colx = cx + (7 - bit) * scale;
                                for sy in 0..scale {
                                    for sx in 0..scale {
                                        self.set_pixel(colx + sx, cy + (ry as i32) * scale + sy, color);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            cx += 8 * scale;
        }
    }

    /// Serialize the whole canvas as little-endian RGB565 (`WIDTH * HEIGHT * 2` bytes).
    pub fn to_le_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(WIDTH * HEIGHT * 2);
        for p in &self.pixels {
            out.extend_from_slice(&p.to_le_bytes());
        }
        out
    }

    /// Replace a rectangular region from little-endian bytes (alias of blit for full frames).
    pub fn fill_rows_le(&mut self, x: i32, y: i32, w: i32, h: i32, data: &[u8]) {
        self.blit_rgb565(x, y, w, h, data);
    }
}

// ---------------------------------------------------------------------------
// swash-based TTF/OTF/TTC font support
// ---------------------------------------------------------------------------

/// Font container format detected from file magic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FontFormat {
    /// TrueType (`\x00\x01\x00\x00` or `true` table directory version).
    Ttf,
    /// OpenType/CFF (`OTTO`).
    Otf,
    /// TrueType Collection (`ttcf`).
    Ttc,
}

/// Detect the container format of raw font bytes.
pub fn detect_format(bytes: &[u8]) -> Option<FontFormat> {
    if bytes.len() < 4 {
        return None;
    }
    let tag = &bytes[0..4];
    if tag == b"ttcf" {
        Some(FontFormat::Ttc)
    } else if tag == b"OTTO" {
        Some(FontFormat::Otf)
    } else if tag == [0x00, 0x01, 0x00, 0x00] || tag == b"true" || tag == [0x01, 0x00, 0x00, 0x00] {
        Some(FontFormat::Ttf)
    } else {
        None
    }
}

/// A validated font loaded from disk (TTF/OTF/TTC, index 0).
pub struct FontFace {
    data: Vec<u8>,
    format: FontFormat,
    family: Option<String>,
    num_fonts: usize,
    units_per_em: u16,
    ascent: f32,
    descent: f32,
}

impl std::fmt::Debug for FontFace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FontFace")
            .field("format", &self.format)
            .field("family", &self.family)
            .field("num_fonts", &self.num_fonts)
            .field("units_per_em", &self.units_per_em)
            .finish()
    }
}

impl FontFace {
    /// Load and validate a font file. Returns `Err` on unreadable or unparseable files.
    pub fn from_file(path: &Path, family: Option<&str>) -> io::Result<Self> {
        let mut data = Vec::new();
        fs::File::open(path)?.read_to_end(&mut data)?;
        Self::from_bytes(&data, family)
    }

    /// Validate raw font bytes (works for ttf/otf/ttc containers).
    pub fn from_bytes(data: &[u8], family: Option<&str>) -> io::Result<Self> {
        let format = detect_format(data)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "not a TTF/OTF/TTC font"))?;
        // Validate container parse at index 0.
        let face = SwashFontRef::from_index(data, 0)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "font table parse failed"))?;
        let metrics = face.metrics(&[]);
        let num_fonts = if matches!(format, FontFormat::Ttc) {
            CollectionRef::new(data).map(|c| c.len() as usize).unwrap_or(1)
        } else {
            1
        };
        Ok(Self {
            data: data.to_vec(),
            format,
            family: family.map(|s| s.to_string()),
            num_fonts,
            units_per_em: metrics.units_per_em,
            ascent: metrics.ascent,
            descent: metrics.descent,
        })
    }

    pub fn format(&self) -> FontFormat {
        self.format
    }

    pub fn family(&self) -> Option<&str> {
        self.family.as_deref()
    }

    pub fn num_fonts(&self) -> usize {
        self.num_fonts
    }

    pub fn units_per_em(&self) -> u16 {
        self.units_per_em
    }

    /// Design-space ascent/descent (in font design units).
    pub fn vertical_metrics(&self) -> (f32, f32) {
        (self.ascent, self.descent)
    }

    /// Map a character to a glyph id (`None` when the font has no such glyph mapping).
    pub fn glyph_id(&self, ch: char) -> Option<GlyphId> {
        let face = SwashFontRef::from_index(&self.data, 0)?;
        let gid = face.charmap().map(ch);
        // swash GlyphId is a plain u16; 0 means `.notdef` (unmapped).
        if gid == 0 {
            None
        } else {
            Some(gid)
        }
    }

    /// Horizontal advance of `gid` scaled to `pixel_size`, in pixels (fractional), read from
    /// the font's `hmtx` table via `read-fonts`.
    pub fn advance_px(&self, gid: GlyphId, pixel_size: f32) -> f32 {
        match RawFontRef::from_index(&self.data, 0) {
            Ok(f) => {
                let upem = f.head().map(|h| h.units_per_em()).unwrap_or(self.units_per_em.max(1));
                match f.hmtx() {
                    Ok(hmtx) => match hmtx.advance(read_fonts::types::GlyphId::new(gid as u32)) {
                        Some(a) => a as f32 * pixel_size / upem as f32,
                        None => pixel_size * 0.5,
                    },
                    Err(_) => pixel_size * 0.5,
                }
            }
            Err(_) => pixel_size * 0.5,
        }
    }

    /// Rasterize one glyph into an alpha coverage image at `pixel_size`.
    pub fn rasterize_glyph(&self, gid: GlyphId, pixel_size: f32) -> Option<Image> {
        let face = SwashFontRef::from_index(&self.data, 0)?;
        let mut ctx = ScaleContext::new();
        let mut scaler = ctx.builder(face).size(pixel_size).build();
        Render::new(&[Source::Outline])
            .format(swash::zeno::Format::Alpha)
            .render(&mut scaler, gid)
    }

    /// Alpha-blend a coverage [`Image`] onto `canvas` at absolute (x, y). Coverage above the
    /// threshold paints `color`; mid grays blend toward the destination for readability on
    /// RGB565 LCDs.
    pub fn blend_coverage(canvas: &mut Canvas, x: i32, y: i32, img: &Image, color: u16) {
        let w = img.placement.width as usize;
        let h = img.placement.height as usize;
        if !matches!(img.content, Content::Mask) || w == 0 || h == 0 {
            return;
        }
        for yy in 0..h {
            for xx in 0..w {
                let off = yy * w + xx;
                if off >= img.data.len() {
                    continue;
                }
                let cov = img.data[off];
                if cov == 0 {
                    continue;
                }
                let px = canvas.get_pixel(x + xx as i32, y + yy as i32);
                if cov >= 0xE0 {
                    canvas.set_pixel(x + xx as i32, y + yy as i32, color);
                } else if cov >= 0x40 {
                    // Blend channel-majority approximation for gray coverage.
                    let dst_r = ((px >> 11) & 0x1F) as u32;
                    let dst_g = ((px >> 5) & 0x3F) as u32;
                    let dst_b = (px & 0x1F) as u32;
                    let src_r = ((color >> 11) & 0x1F) as u32;
                    let src_g = ((color >> 5) & 0x3F) as u32;
                    let src_b = (color & 0x1F) as u32;
                    let a = cov as u32;
                    let mix = |s: u32, d: u32| -> u32 { (s * a + d * (255 - a)) / 255 };
                    let blended = ((mix(src_r, dst_r) as u16) << 11)
                        | ((mix(src_g, dst_g) as u16) << 5)
                        | (mix(src_b, dst_b) as u16);
                    canvas.set_pixel(x + xx as i32, y + yy as i32, blended);
                }
            }
        }
    }

    /// Draw one line of text with this face onto `canvas` starting at (x, baseline_y).
    /// Returns the number of characters drawn. Glyph metrics advance through hmtx data and
    /// glyphs clip silently off-canvas (works for CJK codepoints when the font has them).
    pub fn draw_text(&self, canvas: &mut Canvas, x: i32, baseline_y: i32, text: &str, color: u16, pixel_size: f32) -> usize {
        let mut pen_x = x;
        let mut drawn = 0usize;
        for ch in text.chars() {
            if ch == '\n' {
                continue; // single-line rasterizer call
            }
            let gid = match self.glyph_id(ch) {
                Some(g) => g,
                None => {
                    pen_x += (pixel_size * 0.5) as i32;
                    continue;
                }
            };
            if let Some(img) = self.rasterize_glyph(gid, pixel_size) {
                let gx = pen_x + img.placement.left;
                let gy = baseline_y + img.placement.top;
                FontFace::blend_coverage(canvas, gx, gy, &img, color);
                drawn += 1;
            }
            pen_x += self.advance_px(gid, pixel_size).round() as i32;
        }
        drawn
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb565_exact_values() {
        assert_eq!(rgb565(255, 0, 0), 0xF800);
        assert_eq!(rgb565(0, 255, 0), 0x07E0);
        assert_eq!(rgb565(0, 0, 255), 0x001F);
        assert_eq!(rgb565(255, 255, 255), 0xFFFF);
        assert_eq!(rgb565(0, 0, 0), 0);
        assert_eq!(rgb565(12, 16, 28), ((12 >> 3) << 11) | ((16 >> 2) << 5) | (28 >> 3));
    }

    #[test]
    fn canvas_starts_black_and_clear_fills() {
        let mut c = Canvas::new();
        assert_eq!(c.pixels.len(), WIDTH * HEIGHT);
        assert!(c.pixels.iter().all(|p| *p == 0));
        c.clear(0xABCD);
        assert!(c.pixels.iter().all(|p| *p == 0xABCD));
    }

    #[test]
    fn pixel_clip_bounds() {
        let mut c = Canvas::new();
        c.set_pixel(-1, 0, 0xF800);
        c.set_pixel(0, -1, 0xF800);
        c.set_pixel(320, 0, 0xF800);
        c.set_pixel(0, 170, 0xF800);
        assert_eq!(c.get_pixel(0, 0), 0);
        c.set_pixel(0, 0, 0x1234);
        assert_eq!(c.get_pixel(0, 0), 0x1234);
        assert_eq!(c.get_pixel(320, 170), 0);
        assert_eq!(c.get_pixel(-5, -5), 0);
    }

    #[test]
    fn fill_rect_clips_negative_and_overflow() {
        let mut c = Canvas::new();
        c.fill_rect(-10, -10, 15, 15, 0xF800);
        assert_eq!(c.get_pixel(0, 0), 0xF800);
        assert_eq!(c.get_pixel(4, 4), 0xF800);
        assert_eq!(c.get_pixel(5, 5), 0);
        c.fill_rect(315, 165, 20, 20, 0x07E0);
        assert_eq!(c.get_pixel(319, 169), 0x07E0);
        assert_eq!(c.get_pixel(314, 164), 0);
        // degenerate sizes are no-ops
        c.fill_rect(10, 10, 0, 5, 0xFFFF);
        c.fill_rect(10, 10, -3, -3, 0xFFFF);
        assert_eq!(c.get_pixel(10, 10), 0);
    }

    #[test]
    fn draw_rect_outline_only() {
        let mut c = Canvas::new();
        c.clear(0x001F);
        c.draw_rect(50, 20, 10, 10, 0x07E0);
        assert_eq!(c.get_pixel(50, 20), 0x07E0);
        assert_eq!(c.get_pixel(59, 29), 0x07E0);
        assert_eq!(c.get_pixel(55, 25), 0x001F);
        c.draw_rect(-5, -5, 8, 8, 0xF800);
        // The clipped outline keeps only the in-bounds border segments: right edge col 2 and
        // bottom edge row 2 (the corner pixel at (2,2) is part of both borders).
        assert_eq!(c.get_pixel(2, 2), 0xF800);
        assert_eq!(c.get_pixel(2, 0), 0xF800);
        assert_eq!(c.get_pixel(0, 2), 0xF800);
        assert_eq!(c.get_pixel(1, 1), 0x001F);
    }

    #[test]
    fn draw_line_primitives() {
        let mut c = Canvas::new();
        c.draw_line(0, 0, 9, 9, 0xFFFF);
        for i in 0..10 {
            assert_eq!(c.get_pixel(i, i), 0xFFFF, "diagonal (i,i) should be set");
        }
        c.draw_line(10, 30, 60, 30, 0x07E0); // horizontal
        assert_eq!(c.get_pixel(35, 30), 0x07E0);
        c.draw_line(100, 40, 100, 55, 0xF800); // vertical
        assert_eq!(c.get_pixel(100, 50), 0xF800);
        // huge coordinates clip without panic
        c.draw_line(-5000, -5000, 9000, 9000, 0x1234);
    }

    #[test]
    fn circle_fill_and_outline() {
        let mut c = Canvas::new();
        c.fill_circle(100, 80, 10, 0x07E0);
        assert_eq!(c.get_pixel(100, 80), 0x07E0);
        assert_eq!(c.get_pixel(110, 80), 0x07E0);
        assert_eq!(c.get_pixel(112, 80), 0);
        let before = c.get_pixel(50, 50);
        c.draw_circle(140, 80, 10, 0xFFFF);
        assert_eq!(c.get_pixel(140, 70), 0xFFFF);
        assert_eq!(c.get_pixel(140, 80), before);
        c.fill_circle(-10, -10, 30, 0xABCD); // partially clipped circle reaches visible area
        assert_eq!(c.get_pixel(0, 0), 0xABCD);
        let keep = c.get_pixel(60, 60);
        c.fill_circle(-400, -400, 5, 0x1234); // fully off-screen -> untouched
        assert_eq!(c.get_pixel(60, 60), keep);
    }

    #[test]
    fn blit_little_endian_source() {
        let mut c = Canvas::new();
        let sprite = [0x00u8, 0xF8, 0xE0, 0x07];
        c.blit_rgb565(200, 10, 2, 1, &sprite);
        assert_eq!(c.get_pixel(200, 10), 0xF800);
        assert_eq!(c.get_pixel(201, 10), 0x07E0);
        // short source data: second pixel skipped gracefully
        let mut c2 = Canvas::new();
        c2.blit_rgb565(5, 5, 2, 1, &[0x12, 0x34]);
        assert_eq!(c2.get_pixel(5, 5), 0x3412);
        assert_eq!(c2.get_pixel(6, 5), 0);
        // clipped region: negative x drops leading source columns
        c2.blit_rgb565(-1, 20, 3, 1, &[0x01, 0x02, 0x03, 0x04, 0x05, 0x06]);
        assert_eq!(c2.get_pixel(0, 20), 0x0403);
    }

    #[test]
    fn builtin_font_covers_all_printable_ascii() {
        assert_eq!(font_data::FONT8X8.len(), 95);
        for code in 0x20u16..=0x7Eu16 {
            let rows = glyph_rows(code as u8).expect("glyph present");
            if code == 0x20 {
                assert!(rows.iter().all(|b| *b == 0));
            } else {
                assert!(rows.iter().any(|b| *b != 0), "glyph for {:?} empty", char::from_u32(code as u32));
            }
        }
        assert!(glyph_rows(0x1F).is_none());
        assert!(glyph_rows(0x7F).is_none());
    }

    #[test]
    fn text_draws_pixels_and_newline_moves_down() {
        let mut c = Canvas::new();
        c.draw_text_builtin(10, 30, "Hello 09!", 0xFFFF, 2);
        let mut lit = 0usize;
        for yy in 30..30 + 16 {
            for xx in 10..(10 + 9 * 16) {
                if c.get_pixel(xx, yy) == 0xFFFF {
                    lit += 1;
                }
            }
        }
        assert!(lit >= 60, "expected substantial glyph pixels, got {lit}");
        // scale 1 text stays within its 8px rows
        let mut c2 = Canvas::new();
        c2.draw_text_builtin(0, 0, "AB\nCD", 0x07E0, 1);
        // Glyph pixels sit inside each 8px cell; 'A' top row lights columns 1..=4 of cell 0.
        let lit_row0 = (0..8i32).filter(|x| c2.get_pixel(*x, 0) != 0).count();
        assert!(lit_row0 >= 3, "A should light several pixels on row 0, got {lit_row0}");
        assert_eq!(c2.get_pixel(0, 7), 0, "row 7 spacer for A should be blank");
        // Second glyph advances by exactly 8 px at scale 1 and draws there.
        let lit_cell1 = (8..16i32).filter(|x| c2.get_pixel(*x, 0) != 0).count();
        assert!(lit_cell1 >= 3);
        // Newline moves back to x=0 and down one 8px line.
        let lit_second_line = (0..16i32).flat_map(|x| (8..16i32).map(move |y| (x, y)))
            .filter(|(x, y)| c2.get_pixel(*x, *y) != 0)
            .count();
        assert!(lit_second_line >= 6, "CD on second line should light pixels, got {lit_second_line}");
    }

    #[test]
    fn to_le_bytes_shape() {
        let mut c = Canvas::new();
        c.set_pixel(0, 0, 0xF800);
        c.set_pixel(1, 0, 0x1234);
        let b = c.to_le_bytes();
        assert_eq!(b.len(), 108_800);
        assert_eq!(&b[0..4], &[0x00, 0xF8, 0x34, 0x12]);
        assert_eq!(*b.last().unwrap(), 0);
    }

    #[test]
    fn font_format_detection() {
        assert_eq!(detect_format(b"ttcf\x00\x00\x00\x01"), Some(FontFormat::Ttc));
        assert_eq!(detect_format(b"OTTO\x00\x01\x02\x03"), Some(FontFormat::Otf));
        assert_eq!(detect_format(&[0, 1, 0, 0, 0]), Some(FontFormat::Ttf));
        assert_eq!(detect_format(b"true\x00\x00"), Some(FontFormat::Ttf));
        assert_eq!(detect_format(b"junk!!"), None);
        assert_eq!(detect_format(b""), None);
    }

    #[test]
    fn swash_face_rejects_missing_or_garbage_fonts() {
        let missing = Path::new("/tmp/cz-gfx-definitely-missing-font.ttf");
        let err = FontFace::from_file(missing, None).expect_err("missing font must Err");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);

        let bad_path = std::env::temp_dir().join(format!("cz-gfx-badfont-{}", std::process::id()));
        fs::write(&bad_path, b"this is not a font at all").expect("write junk");
        let err = FontFace::from_file(&bad_path, None).expect_err("junk font must Err");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let err2 = FontFace::from_bytes(b"\xff\xff\xff\xff", None).expect_err("bad magic must Err");
        assert_eq!(err2.kind(), io::ErrorKind::InvalidData);
        fs::remove_file(&bad_path).ok();
    }
}
