# Cardputer Zero LLRT — Phase 6A Technical Specification (Peripheral Hardware Modules: IMU, Audio, IR, IO)

Target device: M5Stack CardputerZero (`jimmy` belongs to `video`, `input`, `audio`, `spi`, `i2c`, `gpio`, `dialout`).
All Phase 1, 2, 2.5, 3A, 4A, and 5 tests (191+ Rust tests, 34 E2E tests) MUST continue to pass without regression.

---

## 1. New Built-in Modules (named exports AND `default` export, registered in `cz-modules`, `cz-runner`, and `types/cardputer.d.ts`)

### 1.1 `"cardputer:imu"` (BMI270 Accel/Gyro + BMM150 Mag via Linux IIO sysfs)
- IIO root resolution: `CARDPUTER_IIO_DIR` env var if set, else `/sys/bus/iio/devices`.
- Scans `iio:device*` directories and reads `name`:
  - `"bmi270"` -> reads `in_accel_{x,y,z}_raw` * `in_accel_scale` and `in_anglvel_{x,y,z}_raw` * `in_anglvel_scale` (falling back to per-axis scale if needed).
  - `"bmm150"` -> reads `in_magn_{x,y,z}_raw` * `in_magn_scale`.
- Exported functions:
  - `devices(): string[]` — sorted list of detected IIO device names (e.g. `["bmi270", "bmm150", "m5ioe1"]`).
  - `read(): { available: boolean, accel: { x: number, y: number, z: number } | null, gyro: { x: number, y: number, z: number } | null, mag: { x: number, y: number, z: number } | null }`.
  - Missing directory or sensors MUST degrade gracefully (`available: false`, fields `null`), never panic.

### 1.2 `"cardputer:audio"` (ES8389-Audio / PipeWire + Pure-Rust PCM WAV Synthesizer)
- Pure-Rust 44.1 kHz, 16-bit signed LE, mono RIFF/WAVE generator (zero external audio crate required!).
- Exported functions:
  - `generateWav(freqHz: number, durationMs: number, volume?: number): Uint8Array`
    - `freqHz` clamped to `20..=20000`, `durationMs` clamped to `1..=10000`, `volume` clamped to `0.0..=1.0` (default `0.5`).
    - Returns valid RIFF/WAVE bytes (`"RIFF"`, `"WAVE"`, `"fmt "`, `"data"` chunks, 44-byte header + `num_samples * 2` bytes).
  - `beep(freqHz: number, durationMs: number, volume?: number): boolean`
    - Generates WAV via `generateWav`.
    - If `CARDPUTER_AUDIO_OUT` env var is set and non-empty, writes the WAV bytes directly to that file path and returns `true` (deterministic testing without hardware speakers!).
    - Otherwise attempts playback via `pw-play` (with `XDG_RUNTIME_DIR=/run/user/1000` fallback) or `aplay`, returning `true` on success or `false` if unavailable.
  - `playWav(path: string): boolean`
    - Verifies file exists and starts with `"RIFF"`. If `CARDPUTER_AUDIO_OUT` is set, copies bytes to `CARDPUTER_AUDIO_OUT` and returns `true`; else invokes `pw-play`/`aplay`.

### 1.3 `"cardputer:ir"` (NEC Protocol Encoder/Decoder + LIRC `/dev/lirc*` TX)
- TX device resolution: `CARDPUTER_IR_TX_PATH` env var if set, else `/dev/lirc0`.
- NEC protocol specification (exact):
  - Leader: `9000` µs pulse, `4500` µs space.
  - 32 bits LSB-first: `address` (8-bit), `!address` (8-bit), `command` (8-bit), `!command` (8-bit).
  - Each bit: `562` µs pulse followed by `562` µs space (`0`) or `1687` µs space (`1`).
  - Trailing stop pulse: `562` µs.
  - Total length: `2 + 32 * 2 + 1 = 67` u32 timing values (odd count, ends on pulse, matching Linux LIRC `LIRC_MODE_PULSE`!).
- Exported functions:
  - `encodeNec(address: number, command: number): Uint32Array` (67 elements).
  - `decodeNec(pulses: Uint32Array | number[]): { address: number, command: number, valid: boolean } | null` (validates leader ~9000/4500 with ±25% tolerance, decodes 32 bits, checks `addr == !inv_addr && cmd == !inv_cmd`).
  - `sendRaw(pulses: Uint32Array | number[]): number` — writes little-endian `u32` timing values to TX path, returns count of pulses written.
  - `sendNec(address: number, command: number): number` — encodes NEC and calls `sendRaw`.

### 1.4 `"cardputer:io"` (Safe Port Discovery + I2C / UART Helpers)
- Dev directory resolution: `CARDPUTER_DEV_DIR` env var if set, else `/dev`.
- Reserved I2C addresses (must reject writes to prevent bricking onboard power gauge): `0x55` (BQ27220).
- Exported functions:
  - `listPorts(): { i2c: string[], spi: string[], gpio: string[], uart: string[] }`
    - Scans dev dir for `i2c-*`, `spidev*`, `gpiochip*`, and `ttyS*`/`ttyAMA*`/`serial*` (sorted).
  - `isReservedI2cAddr(addr: number): boolean` (`addr == 0x55`).
  - `uartWrite(devicePath: string, data: string | Uint8Array): number` — opens file/device for writing, writes bytes, returns bytes written.
  - `uartRead(devicePath: string, maxBytes: number): Uint8Array` — opens file/device for reading, reads up to `maxBytes` (clamped to `0..=65536`), returns `Uint8Array`.

---

## 2. LLRT Integration & Types
- Re-export `ImuModule`, `AudioModule`, `IrModule`, `IoModule` from `crates/cz-modules/src/lib.rs`, add them to `CARDPUTER_MODULE_NAMES`, and update `scripts/build-llrt-a53.sh` so all 10+ Cardputer modules are registered when patching `awslabs/llrt`.
- Update `types/cardputer.d.ts` with complete declarations for `"cardputer:imu"`, `"cardputer:audio"`, `"cardputer:ir"`, and `"cardputer:io"`.
