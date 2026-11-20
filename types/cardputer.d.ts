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

/** QuickJS heap usage snapshot from `cardputer:app.memoryUsage()` (`JS_ComputeMemoryUsage`). */
interface CzMemoryUsage {
  /** Total bytes currently malloc'd by the QuickJS runtime (JS-tracked allocations only). */
  mallocSize: number;
  /** Number of live JS-tracked allocations. */
  mallocCount: number;
  /** Memory used by the GC-visible arena. */
  memoryUsedSize: number;
  /** Count of JS objects. */
  objCount: number;
  /** Count of JS strings. */
  strCount: number;
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

  /** QuickJS runtime memory usage via `JS_ComputeMemoryUsage` (Rust-native KV data never counts here). */
  export function memoryUsage(): CzMemoryUsage;

  const app: {
    info: typeof info;
    getConfig: typeof getConfig;
    shouldQuit: typeof shouldQuit;
    requestQuit: typeof requestQuit;
    installSignalHandlers: typeof installSignalHandlers;
    memoryUsage: typeof memoryUsage;
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

/** A value stored natively in `cardputer:kv` (never a JS Map/object). */
type CzKvValue = null | boolean | number | string | Uint8Array;

/** Bookkeeping snapshot from `store.stats()`. */
interface CzKvStats {
  name: string;
  entries: number;
  bytes: number;
  maxBytes: number;
  maxEntries: number;
  evictions: number;
  hits: number;
  misses: number;
}

/** Options accepted by `kv.open(name, options?)`. */
interface CzKvOpenOptions {
  /** Native byte budget; defaults to 1 MiB (1048576). */
  maxBytes?: number;
  /** Entry budget; defaults to 1024. */
  maxEntries?: number;
  /** Power-loss-safe persistence file (`tmp` -> `fsync` -> atomic `rename`). */
  persistPath?: string;
}

/** A named Rust-native KV store handle. */
interface KvStore {
  /** Insert/replace a scalar value with optional fractional-second TTL. */
  set(key: string, value: CzKvValue, ttlSec?: number): void;
  /** Read a scalar value; `undefined` when missing or expired. */
  get(key: string): CzKvValue | undefined;
  has(key: string): boolean;
  del(key: string): boolean;
  /** Live keys, optionally prefix-filtered. */
  keys(prefix?: string): string[];
  /** Set one field on a structured record (optional TTL). */
  setField(key: string, field: string, value: CzKvValue, ttlSec?: number): void;
  /** Read one field; `undefined` when missing/expired. */
  getField(key: string, field: string): CzKvValue | undefined;
  /** Increment a numeric field (missing treated as 0); default delta is 1. Returns the new value. */
  incr(key: string, field: string, delta?: number, ttlSec?: number): number;
  /** Zero-copy native row handle (a Proxy over an empty target; no data fields copied onto JS). */
  row(key: string, ttlSec?: number): Record<string, any>;
  stats(): CzKvStats;
  /** Atomic persist; `true` when a `persistPath` exists, `false` for in-memory-only stores. */
  flush(): boolean;
  /** Drop all entries from memory (does not delete a persisted file). */
  clear(): void;
}

declare module "cardputer:kv" {
  /** Open (or reuse) a named Rust-native store; loads an existing snapshot when `persistPath` is set. */
  export function open(name: string, options?: CzKvOpenOptions): KvStore;
  /** Remove/free a named store from the native registry; returns whether it existed. */
  export function drop(name: string): boolean;

  const kv: {
    open: typeof open;
    drop: typeof drop;
  };
  export default kv;
}

/** Diagnostics snapshot for a Lazy JSON handle (`cardputer:json.lazyStats`). */
interface CzLazyJsonStats {
  /** Total `TapeNode` arena nodes in the document (each exactly 16 bytes). */
  nodeCount: number;
  /** Size of the shared immutable raw source bytes held by the document. */
  rawBytes: number;
  /** True once Copy-on-Write has materialized this subtree into real JS state. */
  materialized: boolean;
}

declare module "cardputer:json" {
  /**
   * Force a transparent Lazy Tape handle regardless of document size (`>= 4096` byte documents
   * already arrive lazy from the replaced `JSON.parse`). Untouched members are read straight from
   * the Rust tape; writing any member copies it through a Copy-on-Write override layer.
   */
  export function parseLazy(text: string): any;
  /** True while `value` is an untouched (clean) Lazy JSON handle. */
  export function isLazy(value: any): boolean;
  /** Arena diagnostics for a Lazy handle, or null for anything else. */
  export function lazyStats(value: any): CzLazyJsonStats | null;

  const czJson: {
    parseLazy: typeof parseLazy;
    isLazy: typeof isLazy;
    lazyStats: typeof lazyStats;
  };
  export default czJson;
}

/** One 3-axis physical IMU reading (m/s^2, rad/s or µT depending on the sensor). */
interface CzVec3 {
  x: number;
  y: number;
  z: number;
}

/** Snapshot from `cardputer:imu.read()`; missing sensors degrade to `null` fields. */
interface CzImuReading {
  /** False when no BMI270/BMM150 data could be read at all (missing IIO root included). */
  available: boolean;
  /** Accelerometer in m/s^2 (BMI270 `in_accel_*_raw * in_accel_scale`), or null. */
  accel: CzVec3 | null;
  /** Angular velocity in rad/s (BMI270 `in_anglvel_*_raw * in_anglvel_scale`), or null. */
  gyro: CzVec3 | null;
  /** Magnetic field in µT (BMM150 `in_magn_*_raw * in_magn_scale`), or null. */
  mag: CzVec3 | null;
}

declare module "cardputer:imu" {
  /** Sorted names of every IIO device found under `CARDPUTER_IIO_DIR` (default `/sys/bus/iio/devices`). */
  export function devices(): string[];
  /** Read accel/gyro/mag with `raw * scale` conversions; never throws on missing hardware. */
  export function read(): CzImuReading;

  const imu: {
    devices: typeof devices;
    read: typeof read;
  };
  export default imu;
}

declare module "cardputer:audio" {
  /**
   * Synthesize a pure-Rust 44.1 kHz, 16-bit signed LE mono RIFF/WAVE sine wave (44-byte header +
   * `sampleCount * 2` bytes). `freqHz` clamps to 20..20000, `durationMs` to 1..10000 and
   * `volume` to 0..1 (default 0.5).
   */
  export function generateWav(freqHz: number, durationMs: number, volume?: number): Uint8Array;

  /**
   * Play a tone. When `CARDPUTER_AUDIO_OUT` points at a file the WAV bytes are written there
   * deterministically; otherwise `pw-play` (with an `XDG_RUNTIME_DIR=/run/user/1000` fallback) or
   * `aplay` is invoked. Returns whether playback succeeded.
   */
  export function beep(freqHz: number, durationMs: number, volume?: number): boolean;

  /** Replay a WAV file from disk (must exist and start with "RIFF"); returns success. */
  export function playWav(path: string): boolean;

  const audio: {
    generateWav: typeof generateWav;
    beep: typeof beep;
    playWav: typeof playWav;
  };
  export default audio;
}

/** Frame decoded from NEC timings (`cardputer:ir.decodeNec`). */
interface CzNecFrame {
  address: number;
  command: number;
  /** True only when both complements match AND every timing sits inside the ±25% window. */
  valid: boolean;
}

declare module "cardputer:ir" {
  /**
   * Encode one NEC frame: leader `9000`/`4500`, 32 LSB-first bits (`addr`, `~addr`, `cmd`,
   * `~cmd`) as `562 µs` pulses with `562`/`1687 µs` spaces, plus a `562 µs` trailing pulse —
   * exactly 67 values ending on a pulse (Linux `LIRC_MODE_PULSE`).
   */
  export function encodeNec(address: number, command: number): Uint32Array;

  /** Decode NEC timings (±25% tolerance); null for unusable/short streams or a bad leader. */
  export function decodeNec(pulses: Uint32Array | number[]): CzNecFrame | null;

  /** Write little-endian `u32` timings to `CARDPUTER_IR_TX_PATH` (default `/dev/lirc0`); returns count. */
  export function sendRaw(pulses: Uint32Array | number[]): number;

  /** Encode an NEC frame and transmit it; returns the number of timings written. */
  export function sendNec(address: number, command: number): number;

  const ir: {
    encodeNec: typeof encodeNec;
    decodeNec: typeof decodeNec;
    sendRaw: typeof sendRaw;
    sendNec: typeof sendNec;
  };
  export default ir;
}

/** Port discovery result from `cardputer:io.listPorts()` (each list sorted). */
interface CzIoPorts {
  /** Character devices matching `i2c-*`. */
  i2c: string[];
  /** Devices matching `spidev*`. */
  spi: string[];
  /** GPIO chips matching `gpiochip*`. */
  gpio: string[];
  /** UART nodes matching `ttyS*`, `ttyAMA*` or `serial*`. */
  uart: string[];
}

declare module "cardputer:io" {
  /** Enumerate safe ports under `CARDPUTER_DEV_DIR` (default `/dev`); missing dir yields empties. */
  export function listPorts(): CzIoPorts;

  /** True for reserved addresses that MUST NOT be written (`0x55` = onboard BQ27220 gauge). */
  export function isReservedI2cAddr(addr: number): boolean;

  /** Write a string or byte payload to a UART/device path; returns the bytes written. */
  export function uartWrite(devicePath: string, data: string | Uint8Array): number;

  /** Read up to `maxBytes` (clamped to 0..65536) from a UART/device path. */
  export function uartRead(devicePath: string, maxBytes?: number): Uint8Array;

  const io: {
    listPorts: typeof listPorts;
    isReservedI2cAddr: typeof isReservedI2cAddr;
    uartWrite: typeof uartWrite;
    uartRead: typeof uartRead;
  };
  export default io;
}

/** Bound socket address reported by `server.address()` (null before listen / after close). */
interface CzServerAddress {
  /** Wildcard bind means external peers (e.g. Tailscale) can connect. */
  address: string;
  family: "IPv4" | "IPv6";
  port: number;
}

/** Peer/local socket info exposed as `req.socket` (Node.js compatible shape). */
interface CzSocketInfo {
  remoteAddress: string;
  remotePort: number;
  localAddress: string;
  localPort: number;
}

/** Incoming request object passed to a `"request"` listener (`node:http`). */
interface IncomingMessage {
  /** Uppercase method token, e.g. "GET", "POST". */
  method: string;
  /** Raw request target including the query string, e.g. "/api/status?v=1". */
  url: string;
  /** HTTP version digits, e.g. "1.1". */
  httpVersion: string;
  /** Header fields with names lowercased (duplicates joined with ", "). */
  headers: Record<string, string>;
  socket: CzSocketInfo;
  /** Set body chunk decoding ("utf8" / "utf-8" yields string chunks). Returns this. */
  setEncoding(encoding: string): IncomingMessage;
  /** Register a body listener; late registration replays already-delivered data/end. */
  on(event: "data", listener: (chunk: string | Uint8Array) => void): IncomingMessage;
  on(event: "end" | "close", listener: () => void): IncomingMessage;
  on(event: "error", listener: (err: Error) => void): IncomingMessage;
}

/** Outgoing response object passed to a `"request"` listener (`node:http`). */
interface ServerResponse {
  statusCode: number;
  statusMessage?: string;
  /** Becomes true once the response has been finalized/sent. */
  headersSent: boolean;
  /** True after `end()` has been called (repeated end() calls are safe no-ops). */
  writableEnded: boolean;
  socket: CzSocketInfo;
  /** Case-insensitive header set; returns this for chaining. */
  setHeader(name: string, value: string | number): ServerResponse;
  getHeader(name: string): string | undefined;
  hasHeader(name: string): boolean;
  removeHeader(name: string): void;
  /** Status plus optional reason phrase or header record. */
  writeHead(statusCode: number): ServerResponse;
  writeHead(statusCode: number, headers: Record<string, string | number>): ServerResponse;
  writeHead(statusCode: number, statusMessage: string, headers?: Record<string, string | number>): ServerResponse;
  /** Append part of the response body. Returns false once the connection has closed; in
   *  streaming mode each non-empty chunk is flushed immediately as a chunked frame. */
  write(chunk: string | Uint8Array): boolean;
  /** Send status line + headers NOW with `Transfer-Encoding: chunked` (auto-added when no
   *  Content-Length) so SSE/long-lived streams start before end(); repeat calls are no-ops. */
  flushHeaders(): void;
  /** Register a disconnect listener (fires at most once when the client hangs up mid-stream). */
  on(event: "close", listener: () => void): ServerResponse;
  /** Finalize and transmit the response. Buffered responses auto-set Content-Length; streamed
   *  responses write the last chunk plus the terminating `0\r\n\r\n` frame. */
  end(chunk?: string | Uint8Array, callback?: () => void): ServerResponse;
  end(callback?: () => void): ServerResponse;
}

/** HTTP server created by `createServer()`; default bind host is "0.0.0.0" (Tailscale-visible). */
declare class CzHttpServer {
  /** Bind immediately: `listen(port)`, `listen(port, cb)` or `listen(port, host, cb)`; port 0 asks the OS for an ephemeral port. */
  listen(port: number, callback?: () => void): this;
  listen(port: number, host: string, callback?: () => void): this;
  /** The real bound address, or null before listen() and after close(). */
  address(): CzServerAddress | null;
  /** Stop accepting new connections (live sockets finish their current request), emit "close". */
  close(callback?: () => void): this;
  on(event: "request", listener: (req: IncomingMessage, res: ServerResponse) => void): this;
  on(event: "listening" | "close", listener: () => void): this;
  on(event: "error", listener: (err: Error) => void): this;
}

declare module "node:http" {
  /** Standard reason phrases keyed by numeric status code (e.g. STATUS_CODES[404]). */
  export const STATUS_CODES: Record<string, string>;
  /** Supported method tokens: GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS. */
  export const METHODS: string[];

  /** Create a server; if `requestListener` is given it is registered as the "request" listener. */
  export function createServer(requestListener?: (req: IncomingMessage, res: ServerResponse) => void): CzHttpServer;

  /** The `Server` constructor function the runtime exports (same shape `createServer` builds). */
  export const Server: {
    new (requestListener?: (req: IncomingMessage, res: ServerResponse) => void): CzHttpServer;
    prototype: CzHttpServer;
  };

  /** Constructor/type for the request object handed to `"request"` listeners. */
  export const IncomingMessage: { prototype: IncomingMessage };
  /** Constructor/type for the response object handed to `"request"` listeners. */
  export const ServerResponse: { prototype: ServerResponse };

  const http: {
    STATUS_CODES: typeof STATUS_CODES;
    METHODS: typeof METHODS;
    createServer: typeof createServer;
    Server: typeof Server;
    IncomingMessage: typeof IncomingMessage;
    ServerResponse: typeof ServerResponse;
  };
  export default http;
}

/** Node.js-style alias of the `node:http` module (named exports plus default object). */
declare module "http" {
  import httpMod from "node:http";
  export * from "node:http";
  export { httpMod as default };
}

/** Result shape of `statSync()`. */
interface CzStats {
  /** Size in bytes. */
  size: number;
  isFile(): boolean;
  isDirectory(): boolean;
}

declare module "node:fs" {
  /** Synchronous read: `string` for "utf8"/"utf-8" (also `{ encoding }` form), otherwise `Uint8Array`. Throws on missing files. */
  export function readFileSync(path: string): Uint8Array;
  export function readFileSync(path: string, encoding: "utf8" | "utf-8"): string;
  export function readFileSync(path: string, options?: { encoding?: string | null }): string | Uint8Array;

  /** Whether a path exists. */
  export function existsSync(path: string): boolean;

  /** Lightweight metadata (`size`, `isFile()`, `isDirectory()`); throws when the path is missing. */
  export function statSync(path: string): CzStats;

  const fs: {
    readFileSync: typeof readFileSync;
    existsSync: typeof existsSync;
    statSync: typeof statSync;
  };
  export default fs;
}

/** Node.js-style alias of the `node:fs` module (named exports plus default object). */
declare module "fs" {
  import fsMod from "node:fs";
  export * from "node:fs";
  export { fsMod as default };
}

/** Node-compatible timers provided on `globalThis` by `cz-js`. */
declare function setTimeout(callback: (...args: unknown[]) => void, delayMs?: number, ...args: unknown[]): number;
declare function clearTimeout(id: number): void;
declare function setInterval(callback: (...args: unknown[]) => void, delayMs?: number, ...args: unknown[]): number;
declare function clearInterval(id: number): void;
