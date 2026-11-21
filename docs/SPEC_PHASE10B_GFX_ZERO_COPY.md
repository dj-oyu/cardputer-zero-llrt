# Cardputer Zero LLRT — Phase 10B Technical Specification (Display/Font Layer Zero-Copy: `cz-gfx` & `cardputer:display`)

Target device: M5Stack CardputerZero (Cortex-A53 ARMv8.0, little-endian, 320×170 RGB565 `/dev/fb0`).
All existing Phase 1–10A tests MUST continue to pass without regression (pixel-exact rendering).

---

## 1. Goal

Eliminate per-frame 108.8 KiB framebuffer allocations and per-character font parsing / `ScaleContext` / `Image` allocations in `cz-gfx` and `cardputer:display`.

---

## 2. `crates/cz-gfx/src/lib.rs`

### 2.1 Zero-Copy Little-Endian Framebuffer View (`Canvas::as_le_bytes`)
- Add `pub fn as_le_bytes(&self) -> &[u8]` (on `#[cfg(target_endian = "little")]`, safely view `&self.pixels` (`&[u16]`) as `&[u8]` of length `self.pixels.len() * 2` via `std::slice::from_raw_parts(self.pixels.as_ptr() as *const u8, self.pixels.len() * 2)` — zero allocations, zero copies!).
- Keep `pub fn to_le_bytes(&self) -> Vec<u8>` for backward compatibility with existing unit tests (on LE it can simply do `self.as_le_bytes().to_vec()`).

### 2.2 `FontFace` Persistent `ScaleContext`, Cached Glyph Rasterizer, and Move-Based `from_file`
- Currently, `FontFace::draw_text` re-parses `SwashFontRef`/`RawFontRef`, constructs a brand-new `ScaleContext::new()`, and allocates a fresh `swash::scale::image::Image` **for every single character**!
- Refactor `FontFace` so that:
  1. `from_bytes(data: Vec<u8>, family: String)` (or `from_owned_bytes` called by `from_file`) takes ownership of `Vec<u8>` directly without `data.to_vec()` (eliminates temporary 2× font file RAM spike on multi-MB CJK `.ttc` files!). Keep `from_bytes` or `from_slice` compatible with existing callers/tests.
  2. `FontFace` holds a reusable `ScaleContext` + bounded glyph cache (e.g., `Mutex<FontCache>` containing `scaler: ScaleContext` and a bounded `HashMap<(u16, u16), CachedGlyph>` capped at e.g. `256` entries storing `(left, top, width, height, advance_px, alpha: Vec<u8>)` or `Box<[u8]>`).
  3. On a cache hit for `(glyph_id, pixel_size)`, `draw_text` performs **zero heap allocations** and **zero swash rasterizations**, blitting directly from the cached alpha bitmap!
  4. On a cache miss, it reuses the persistent `ScaleContext` (never calling `ScaleContext::new()` per glyph), caches the rasterized glyph, and blits with **pixel-identical** output to the previous implementation.

---

## 3. `crates/cz-modules/src/lib.rs` (`cardputer:display`)

- **`js_flush`**: write `canvas.as_le_bytes()` directly to `/dev/fb0` (eliminates the 108,800-byte `Vec<u8>` allocation on every single `display.flush()`!).
- **`blit`**: borrow the JS `Uint8Array` / `ArrayBuffer` slice directly (`ta.as_slice()` / `ab.as_bytes()`) and pass `&[u8]` straight into `canvas.blit_rgb565(...)` without copying into a temporary `Vec<u8>`!
- **`getBuffer`**: copy once directly from `canvas.as_le_bytes()` into `TypedArray::<u8>::new_copy` (down from 2 × 108.8 KiB copies to 1).

---

## 4. Verification Requirements

1. `bash /refs/verify/check_integrity.sh` passes.
2. `node /refs/phase10/verify_phase10.mjs --phase=10b` passes.
3. `RUSTFLAGS="-D warnings" cargo test --workspace -- --test-threads=1` passes with zero warnings, including `>= 4` new unit tests in `cz-gfx` verifying `as_le_bytes()` pointer/content parity with `to_le_bytes()`, `FontFace` glyph cache hit parity (pixel-identical repeated `draw_text`), and bounded cache eviction.
4. Phase 1 E2E regression (`bash /refs/verify/check_e2e.sh`) passes.
