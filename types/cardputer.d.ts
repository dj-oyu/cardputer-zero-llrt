// Type declarations for the Cardputer Zero LLRT builtin modules (`cz-js`, rquickjs 0.14).
// Target: M5Stack CardputerZero — 320x170 RGB565 LCD, TCA8418C keyboard, BQ27220 gauge.

/** Modifier key state tracked by the TCA8418C keyboard driver. */
interface CzMods {
  shift: boolean;
  ctrl: boolean;
  alt: boolean;
  sym: boolean;
  fn: boolean;
}

/** One processed keyboard event from `cardputer:input.poll()`. */
interface CzKeyEvent {
  /** Raw Linux evdev keycode. */
  code: number;
  /** Key name (e.g. "A", "Esc", "Up", "F1", symbol char for Sym-layer keys). */
  key: string;
  /** Text produced by the key, or null for control/navigation keys. */
  text: string | null;
  /** True on key press (including synthetic auto-repeat presses). */
  pressed: boolean;
  /** True when emitted by the self-managed repeat timer. */
  repeat: boolean;
  /** Modifier snapshot at the time of the event. */
  mods: CzMods;
  /** Event timestamp in microseconds. */
  timestampUs: number;
}

/** APPLaunch launcher configuration (top-level config.json keys only). */
interface CzAppConfig {
  /** Screen-off idle seconds; null means sleep disabled (`0`). */
  darkTime: number | null;
  /** Startup brightness when present and > 0. */
  brightness: number | null;
  /** Startup volume (clamped to <= 100) when present and >= 0. */
  volume: number | null;
}

/** Static device info reported by `cardputer:app.info()`. */
interface CzAppInfo {
  device: string;
  arch: string;
  cpu: string;
  displayWidth: number;
  displayHeight: number;
  runtime: string;
}

/** BQ27220 battery status from `cardputer:power.status()`. */
interface CzPowerStatus {
  available: boolean;
  present: boolean;
  /** Charge percentage 0..=100, or null when unreadable. */
  capacity: number | null;
  /** Voltage in millivolts (sysfs voltage_now is microvolts). */
  voltageMv: number | null;
  /** Current in milliamps (sysfs current_now is microamps; negative = discharging). */
  currentMa: number | null;
  /** Temperature in degrees Celsius (sysfs temp is tenths of a degree). */
  tempC: number | null;
  /** Charging state: "Charging" | "Discharging" | "Not charging" | "Full" | "Unknown" | "Unavailable". */
  status: string;
  /** Battery health (e.g. "Good"), or null when unavailable. */
  health: string | null;
}

declare module "cardputer:display" {
  /** Display width in pixels (320). */
  export const WIDTH: number;
  /** Display height in pixels (170). */
  export const HEIGHT: number;

  /** Pack 8-bit RGB channels into an RGB565 pixel value. */
  export function rgb565(r: number, g: number, b: number): number;

  /** Fill the whole canvas with an RGB565 color. */
  export function clear(color: number): void;

  /** Set one pixel (out-of-bounds coordinates are clipped silently). */
  export function setPixel(x: number, y: number, color: number): void;

  /** Read one pixel; returns 0 when out of bounds. */
  export function getPixel(x: number, y: number): number;

  /** Fill a rectangle (silently clipped to the display bounds). */
  export function fillRect(x: number, y: number, w: number, h: number, color: number): void;

  /** Draw a 1px outline rectangle (interior untouched). */
  export function drawRect(x: number, y: number, w: number, h: number, color: number): void;

  /** Draw a line between two points (Bresenham, clipped). */
  export function drawLine(x0: number, y0: number, x1: number, y1: number, color: number): void;

  /** Draw a circle outline (clipped). */
  export function drawCircle(cx: number, cy: number, r: number, color: number): void;

  /** Draw a solid circle (clipped). */
  export function fillCircle(cx: number, cy: number, r: number, color: number): void;

  /** Render text with the built-in 8x8 ASCII bitmap font; scale defaults to 1. */
  export function drawText(x: number, y: number, text: string, color: number, scale?: number): void;

  /** Blit a little-endian RGB565 byte region onto the canvas. */
  export function blit(x: number, y: number, w: number, h: number, data: Uint8Array): void;

  /** Write the canvas to the framebuffer; lazily opens the device on the first call. */
  export function flush(): void;

  /** Get a copy of the whole canvas as 108800 little-endian RGB565 bytes. */
  export function getBuffer(): Uint8Array;

  /** Current backlight brightness (fresh sysfs read). */
  export function getBrightness(): number;

  /** Clamp the level into `0..=max_brightness`, write it, and return the clamped value. */
  export function setBrightness(level: number): number;

  /** Turn the backlight off (0) and remember the current level for wake(). */
  export function sleep(): void;

  /** Restore the remembered brightness (at least 1, clamped to max) and return it. */
  export function wake(): number;

  /** Whether the backlight is currently off. */
  export function isAsleep(): boolean;

  /** Load a TTF/OTF/TTC font from disk for rendering; returns false when load fails. */
  export function loadFont(path: string, family?: string): boolean;

  const display: {
    WIDTH: typeof WIDTH;
    HEIGHT: typeof HEIGHT;
    rgb565: typeof rgb565;
    clear: typeof clear;
    setPixel: typeof setPixel;
    getPixel: typeof getPixel;
    fillRect: typeof fillRect;
    drawRect: typeof drawRect;
    drawLine: typeof drawLine;
    drawCircle: typeof drawCircle;
    fillCircle: typeof fillCircle;
    drawText: typeof drawText;
    blit: typeof blit;
    flush: typeof flush;
    getBuffer: typeof getBuffer;
    getBrightness: typeof getBrightness;
    setBrightness: typeof setBrightness;
    sleep: typeof sleep;
    wake: typeof wake;
    isAsleep: typeof isAsleep;
    loadFont: typeof loadFont;
  };
  export default display;
}

declare module "cardputer:input" {
  /** Read all pending keyboard events (evdev / fixture file) since the last poll. */
  export function poll(): CzKeyEvent[];

  /** Whether a keycode is currently held down. */
  export function isDown(code: number): boolean;

  /** Whether the keycode was pressed during the most recent poll(). */
  export function wasPressed(code: number): boolean;

  /** Whether the keycode was released during the most recent poll(). */
  export function wasReleased(code: number): boolean;

  /** Current modifier key state. */
  export function getMods(): CzMods;

  /** Sym-layer mapping: stringified keycode -> single character (all 32 symbols). */
  export function getSymMap(): Record<string, string>;

  /** Override self-managed key repeat timing (defaults: delay 400ms, interval 33ms; scroll keys use 50ms unless overridden here). */
  export function setRepeat(delayMs: number, intervalMs: number): void;

  /** Clear tracked keyboard state and close the cached input handle. */
  export function reset(): void;

  const input: {
    poll: typeof poll;
    isDown: typeof isDown;
    wasPressed: typeof wasPressed;
    wasReleased: typeof wasReleased;
    getMods: typeof getMods;
    getSymMap: typeof getSymMap;
    setRepeat: typeof setRepeat;
    reset: typeof reset;
  };
  export default input;
}

declare module "cardputer:app" {
  /** Static device/runtime info (device, arch, cpu, displayWidth, displayHeight, runtime). */
  export function info(): CzAppInfo;

  /** Parse the APPLaunch config.json honoring only top-level keys; broken files fall back to defaults. */
  export function getConfig(): CzAppConfig;

  /** Whether a quit has been requested (via requestQuit() or SIGINT/SIGTERM). */
  export function shouldQuit(): boolean;

  /** Set the shared quit flag. */
  export function requestQuit(): void;

  /** Install SIGINT/SIGTERM handlers that set the quit flag; returns success. */
  export function installSignalHandlers(): boolean;

  const app: {
    info: typeof info;
    getConfig: typeof getConfig;
    shouldQuit: typeof shouldQuit;
    requestQuit: typeof requestQuit;
    installSignalHandlers: typeof installSignalHandlers;
  };
  export default app;
}

declare module "cardputer:power" {
  /** Read the BQ27220 battery gauge; returns an `available: false` shape when no gauge exists. */
  export function status(): CzPowerStatus;

  const power: {
    status: typeof status;
  };
  export default power;
}
