# Cardputer Zero LLRT — Phase 1 Technical Specification

Target device: **M5Stack CardputerZero** (Raspberry Pi Compute Module 0, Cortex-A53 ARMv8.0, 320×170 ST7789V RGB565 LCD, TCA8418C keyboard, TI BQ27220 battery gauge, APPLaunch launcher).

This workspace implements the standalone, testable Rust crates (`cz-hal`, `cz-gfx`, `cz-modules`, `cz-runner`) and TypeScript declarations (`types/cardputer.d.ts`) that plug directly into the LLRT (`rquickjs 0.14`) runtime.

---

## 1. Architecture & Constraints

- **Workspace crates**:
  - `crates/cz-hal`: Linux hardware abstraction layer with environment-variable path overrides so every device can be tested deterministically against mock files/sysfs without root or real hardware.
  - `crates/cz-gfx`: Pure-Rust 320×170 RGB565 (little-endian) offscreen canvas, geometric primitives, built-in 8×8 ASCII bitmap font, and `swash` TTF/OTF/TTC font rasterizer.
  - `crates/cz-modules`: `rquickjs` (`0.14`) `ModuleDef` implementations for:
    - `cardputer:display`
    - `cardputer:input`
    - `cardputer:app`
    - `cardputer:power`
  - `crates/cz-runner`: `cz-js` CLI binary executing ES modules (`cz-js <file.mjs>` or `cz-js -e "<code>"`) with the 4 builtin `cardputer:*` modules registered.
- **Lazy initialization**: Importing `cardputer:display` MUST NOT open `/dev/fb0` (or `CARDPUTER_FB_PATH`). The framebuffer file/device MUST only be opened lazily on the first `flush()` call.
- **No panics / no stubs**: Never use `todo!()`, `unimplemented!()`, or `#[ignore]` anywhere in the workspace. All hardware I/O errors must be returned cleanly as `Result` / JS exceptions (or structured `{ available: false }` where specified).

---

## 2. Crate Specifications

### 2.1 `crates/cz-hal`

#### A. Framebuffer (`fb`)
- Constants: `SCREEN_W: usize = 320`, `SCREEN_H: usize = 170`, `FB_BYTES: usize = 320 * 170 * 2` (`108_800`).
- Device path resolution:
  1. `CARDPUTER_FB_PATH` env var if set and non-empty.
  2. `/dev/fb_lcd` if it exists.
  3. `/dev/fb0` fallback.
- Behavior:
  - Open with read+write.
  - If the path is a character device (`S_IFCHR`), query `FBIOGET_VSCREENINFO` (`0x4600`) and `FBIOGET_FSCREENINFO` (`0x4602`), verify `bpp == 16`, `mmap` with explicit length `FB_BYTES`, save initial contents for restore on `Drop`, write pixel rows, and call `libc::fsync(fd)` on `flush`.
  - If the path is a regular file (used in tests/mocking), ensure/write `FB_BYTES` (108,800 bytes) starting at offset 0 and call `file.sync_data()`. Do NOT overwrite with saved contents on `Drop` for regular files (so test harnesses can inspect the flushed framebuffer bytes after process exit!).

#### B. Backlight (`backlight`)
- Directory resolution:
  1. `CARDPUTER_BACKLIGHT_DIR` env var if set and non-empty.
  2. First sorted entry in `/sys/class/backlight/*`.
- Reads `max_brightness` (default 100 if missing) and `brightness`.
- Operations:
  - `get_brightness() -> io::Result<u32>`
  - `max_brightness() -> u32`
  - `set_brightness(level: u32) -> io::Result<u32>`: clamps `level` to `0..=max`, writes `"{clamped}\n"` to `<dir>/brightness`, updates internal awake state (`asleep = clamped == 0`), if `clamped > 0` remembers it as `saved_level`. Returns clamped value.
  - `sleep() -> io::Result<()>`: if not asleep, reads current brightness; if `> 0`, stores in `saved_level`; writes `"0\n"`, sets `asleep = true`.
  - `wake() -> io::Result<u32>`: sets `asleep = false`, restores `saved_level` (at least `1`, clamped to `max`), writes `"{level}\n"`, returns restored level.
  - `is_asleep() -> bool`.

#### C. Keyboard Input (`input`)
- Device path resolution:
  1. `CARDPUTER_INPUT_PATH` env var if set and non-empty.
  2. Scan `/sys/class/input/event{0..31}/device/name` for `"tca8418c"`, returning `/dev/input/event{i}`.
  3. Fallback `/dev/input/event3`.
- Keymap resolution:
  1. `CARDPUTER_KEYMAP_PATH` env var if set and non-empty.
  2. `/usr/share/keymaps/tca8418_keypad_m5stack_keymap.map` if present.
  3. Built-in fallback table (exact 32 Sym symbols + standard Linux keycodes).
- Modifier keycodes:
  - `KEY_LEFTSHIFT = 42`, `KEY_RIGHTSHIFT = 54` -> `shift`
  - `KEY_LEFTCTRL = 29`, `KEY_RIGHTCTRL = 97` -> `ctrl`
  - `KEY_LEFTALT = 56`, `KEY_RIGHTALT = 100` -> `alt`
  - `KEY_SYM = 383` -> `sym`
  - `KEY_FN = 389` -> `fn`
- Built-in Sym 32-symbol mapping (`code -> char`, exact match to `/usr/share/keymaps/tca8418_keypad_m5stack_keymap.map`):
  - `26:'!'`, `27:'@'`, `39:'#'`, `40:'$'`, `41:'%'`, `43:'^'`, `51:'&'`, `52:'*'`, `53:'('`, `94:')'`
  - `55:'~'`, `69:'`'`, `70:'_'`, `71:'-'`, `72:'+'`, `73:'='`, `74:'['`, `75:']'`, `76:'{'`, `77:'}'`
  - `79:';'`, `80:':'`, `81:'\''`, `82:'"'`, `83:'<'`, `85:'>'`, `86:'\\'`, `89:'|'`
  - `90:','`, `91:'.'`, `92:'/'`, `93:'?'`
- Standard Linux keycodes to name & text:
  - Letters `A`–`Z` (Linux codes 30,48,46,32,18,33,34,35,23,36,37,38,50,49,24,25,16,19,31,20,22,47,17,45,21,44): lowercase without Shift, uppercase with Shift (`None` if Ctrl or Alt held).
  - Digits `1`–`9`, `0` (codes 2..=11): `'1'`..`'0'`.
  - Special keys: `1:"Esc"`, `14:"Backspace"`, `15:"Tab"`, `28:"Enter"`, `57:"Space"` (text `" "`), `102:"Home"`, `103:"Up"`, `104:"PageUp"`, `105:"Left"`, `106:"Right"`, `107:"End"`, `108:"Down"`, `109:"PageDown"`, `110:"Insert"`, `111:"Delete"`, `59..=68:"F1".."F10"`, `87:"F11"`, `88:"F12"`.
- Linux `input_event` binary parsing (64-bit little-endian, 24 bytes per event):
  - `tv_sec: i64`, `tv_usec: i64`, `type_: u16`, `code: u16`, `value: i32`.
  - Process `EV_KEY` (`type_ == 1`): `value == 1` (press), `value == 0` (release). Ignore kernel repeat (`value == 2`).
  - Open with `O_NONBLOCK` (for real devices); for regular files (test fixtures), read sequentially from current file offset until EOF so `poll()` returns all newly available events without blocking.
  - NEVER call `EVIOCGRAB` by default.
- Self-managed key repeat:
  - Default `delay_ms = 400`, `interval_ms = 33` (scroll keys `Up`/`Down`/`PageUp`/`PageDown` use `50` ms unless overridden).
  - Track held key state (`down`), per-poll edge transitions (`pressed`, `released`), and repeat timing using event timestamps or monotonic clock (`advance_repeat(now_us)`).
  - If a key is held and `now_us >= next_repeat_us` (where `now_us` can come from synthetic timestamps or system monotonic clock), emit a `KeyEvent { repeat: true, pressed: true, ... }`.

#### D. APPLaunch Config (`config`)
- Path resolution:
  1. `CARDPUTER_CONFIG_PATH` env var if set and non-empty.
  2. `$HOME/.config/cardputerzero/config.json`.
- Parsing rules (exact match to `zcode/launcher_config.rs`):
  - Parse ONLY top-level JSON keys (object depth == 1). Ignore identically named keys inside nested objects (e.g. `{"appstore":{"dark_time":999},"dark_time":60}` -> `dark_time = 60`).
  - Accept both integer literals (`60`) and numeric strings (`"80"`).
  - `dark_time`: if missing or invalid -> `Some(30)`; if `<= 0` -> `None` (sleep disabled); if `> 0` -> `Some(v)`.
  - `brightness`: if present and `> 0` -> `Some(v)`; otherwise `None`.
  - `volume`: if present and `>= 0` -> `Some(v.min(100))`; otherwise `None`.
  - Missing or malformed JSON file MUST return defaults (`dark_time: Some(30), brightness: None, volume: None`), never panic or error.

#### E. Power / Battery (`power`)
- Directory resolution:
  1. `CARDPUTER_POWER_DIR` env var if set and non-empty.
  2. `/sys/class/power_supply/bq27220-0` if it exists.
  3. First entry in `/sys/class/power_supply/*` whose `type` file equals `"Battery"`.
- Fields parsed from sysfs files (trimmed text):
  - `present`: `"1"` -> `true`, else `false`.
  - `capacity`: `Option<u8>` (0..=100).
  - `voltage_now`: `i64` in µV -> `voltage_mv: Option<i64>` (`voltage_now / 1000`).
  - `current_now`: `i64` in µA -> `current_ma: Option<i64>` (`current_now / 1000`).
  - `temp`: `i64` in 0.1°C -> `temp_c: Option<f64>` (`temp as f64 / 10.0`).
  - `status`: `String` (e.g. `"Charging"`, `"Discharging"`, `"Not charging"`, `"Full"`, or `"Unknown"`).
  - `health`: `Option<String>` (e.g. `"Good"`).
- If no battery directory exists, return `PowerStatus { available: false, present: false, capacity: None, voltage_mv: None, current_ma: None, temp_c: None, status: "Unavailable".into(), health: None }`.

---

### 2.2 `crates/cz-gfx`

- `pub const WIDTH: usize = 320;`
- `pub const HEIGHT: usize = 170;`
- `pub fn rgb565(r: u8, g: u8, b: u8) -> u16`:
  `(((r as u16) >> 3) << 11) | (((g as u16) >> 2) << 5) | ((b as u16) >> 3)`
- `Canvas`:
  - Stores `pixels: Vec<u16>` of length `320 * 170` (row-major, `idx = y * 320 + x`).
  - Out-of-bounds coordinates in all drawing methods MUST be clipped silently (never panic).
  - Methods:
    - `new() -> Self` (initialized to 0 / black)
    - `clear(&mut self, color: u16)`
    - `set_pixel(&mut self, x: i32, y: i32, color: u16)`
    - `get_pixel(&self, x: i32, y: i32) -> u16` (returns 0 if out of bounds)
    - `fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u16)`
    - `draw_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u16)` (1px outline)
    - `draw_line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, color: u16)` (Bresenham)
    - `draw_circle(&mut self, cx: i32, cy: i32, r: i32, color: u16)`
    - `fill_circle(&mut self, cx: i32, cy: i32, r: i32, color: u16)`
    - `blit_rgb565(&mut self, x: i32, y: i32, w: i32, h: i32, data: &[u8])` (little-endian RGB565 source bytes)
    - `draw_text_builtin(&mut self, x: i32, y: i32, text: &str, color: u16, scale: u32)`: renders printable ASCII (`0x20..=0x7E`) using an embedded 8×8 bitmap font (non-empty glyphs for A-Z, a-z, 0-9, punctuation), `scale.max(1)`, advancing `8 * scale` per char and handling `\n`.
    - `to_le_bytes(&self) -> Vec<u8>` (108,800 bytes, little-endian `u16::to_le_bytes`).
  - Optional `FontFace` wrapper using `swash` to load `.ttf` / `.otf` / `.ttc` from disk and rasterize glyphs (including CJK) onto `Canvas`.

---

### 2.3 `crates/cz-modules` (`rquickjs = "0.14"` `ModuleDef`)

All 4 modules MUST implement `rquickjs::module::ModuleDef` and export both named exports AND a `default` object containing all exports.

#### Module 1: `"cardputer:display"`
- Constants: `WIDTH` (`320`), `HEIGHT` (`170`)
- Functions:
  - `rgb565(r: number, g: number, b: number): number`
  - `clear(color: number): void`
  - `setPixel(x: number, y: number, color: number): void`
  - `getPixel(x: number, y: number): number`
  - `fillRect(x: number, y: number, w: number, h: number, color: number): void`
  - `drawRect(x: number, y: number, w: number, h: number, color: number): void`
  - `drawLine(x0: number, y0: number, x1: number, y1: number, color: number): void`
  - `drawCircle(cx: number, cy: number, r: number, color: number): void`
  - `fillCircle(cx: number, cy: number, r: number, color: number): void`
  - `drawText(x: number, y: number, text: string, color: number, scale?: number): void`
  - `blit(x: number, y: number, w: number, h: number, data: Uint8Array): void`
  - `flush(): void` (lazily opens fb on first call, writes 108,800 LE bytes, syncs)
  - `getBuffer(): Uint8Array` (returns 108,800-byte LE copy of canvas)
  - `getBrightness(): number`
  - `setBrightness(level: number): number`
  - `sleep(): void`
  - `wake(): number`
  - `isAsleep(): boolean`
  - `loadFont(path: string, family?: string): boolean`

#### Module 2: `"cardputer:input"`
- Functions:
  - `poll(): KeyEvent[]` — reads all pending events from evdev (or `CARDPUTER_INPUT_PATH`) and returns an array of objects:
    ```ts
    {
      code: number,
      key: string,
      text: string | null,
      pressed: boolean,
      repeat: boolean,
      mods: { shift: boolean, ctrl: boolean, alt: boolean, sym: boolean, fn: boolean },
      timestampUs: number
    }
    ```
  - `isDown(code: number): boolean`
  - `wasPressed(code: number): boolean` (true if pressed during most recent `poll()`)
  - `wasReleased(code: number): boolean` (true if released during most recent `poll()`)
  - `getMods(): { shift: boolean, ctrl: boolean, alt: boolean, sym: boolean, fn: boolean }`
  - `getSymMap(): Record<string, string>` (returns mapping of stringified keycode -> single-char symbol, all 32 symbols)
  - `setRepeat(delayMs: number, intervalMs: number): void`
  - `reset(): void` (clears internal state and closes cached input handle so tests can re-open cleanly)

#### Module 3: `"cardputer:app"`
- Functions:
  - `info(): { device: string, arch: string, cpu: string, displayWidth: number, displayHeight: number, runtime: string }`
    - `device`: `"M5Stack CardputerZero"`
    - `arch`: `"aarch64"`
    - `cpu`: `"cortex-a53"`
    - `displayWidth`: `320`
    - `displayHeight`: `170`
    - `runtime`: `"cardputer-zero-llrt"`
  - `getConfig(): { darkTime: number | null, brightness: number | null, volume: number | null }`
  - `shouldQuit(): boolean`
  - `requestQuit(): void` (sets quit flag)
  - `installSignalHandlers(): boolean` (installs SIGINT/SIGTERM handlers that set the atomic quit flag)

#### Module 4: `"cardputer:power"`
- Functions:
  - `status(): { available: boolean, present: boolean, capacity: number | null, voltageMv: number | null, currentMa: number | null, tempC: number | null, status: string, health: string | null }`

---

### 2.4 `crates/cz-runner` (`cz-js`)

- CLI interface:
  - `cz-js <script.mjs>` — loads and evaluates the file as an ES module.
  - `cz-js -e "<code>"` — evaluates inline code as an ES module.
  - `cz-js --version` — prints `cardputer-zero-llrt 0.1.0` and exits 0.
- Registers a `BuiltinResolver` + `BuiltinLoader` for `"cardputer:display"`, `"cardputer:input"`, `"cardputer:app"`, `"cardputer:power"`, chained with `FileResolver` + `ScriptLoader` so multi-file relative ESM imports (e.g. `import { drawHud } from "./hud.mjs"`) work directly on-device without any `node_modules` bundler.
- Provides `globalThis.console = { log, error, warn }` writing to stdout/stderr.
- Runs pending QuickJS jobs (`rt.execute_pending_job()`).
- On any JS exception or module resolution error, prints the error message/stack to stderr and exits with code `1`.

---

### 2.5 `types/cardputer.d.ts`

- Complete TypeScript ambient module declarations for `"cardputer:display"`, `"cardputer:input"`, `"cardputer:app"`, and `"cardputer:power"` matching §2.3 exactly.
