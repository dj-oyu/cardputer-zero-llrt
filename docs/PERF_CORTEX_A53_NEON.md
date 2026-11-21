# Cortex-A53 ARMv8.0 NEON Optimization Record (`cz-js`)

Real-hardware A/B measurement record on M5Stack CardputerZero (ARM Cortex-A53 r0p4, ARMv8.0-A, 1.0 GHz, Linux 6.18 aarch64, 512 MB RAM).

All kernels use **baseline ARMv8.0 NEON only** (`vld3q`/`vld4q`/`vst2q`/`vst3q`/`vst4q`, `vqtbl1q`/`vqtbl4q`, `veorq`, `vminvq`). Zero LSE atomics added, zero RCpc/dotprod instructions (`CZ_JS_LSE=40 <= 200`, `CZ_JS_RCPC=0`).

## Round 1 — Base64 / Base64URL / Hex / WebSocket Masking

### Kernels Added
- **Base64 / Base64URL Encode** (`crates/cz-modules/src/crypto_module.rs`, shared with `buffer_module.rs`):
  - 48 input bytes -> 64 ASCII bytes per iteration via `vld3q_u8` + bitfield extract + `vqtbl4q_u8` + `vst4q_u8`, with exact pre-allocated output capacity and scalar tail.
- **Base64 / Base64URL Decode** (`crates/cz-modules/src/crypto_module.rs`):
  - 64 ASCII bytes -> 48 raw bytes per iteration via `vld4q_u8` + disjoint range classification + single `vminvq_u8` validation across all 64 bytes + `vst3q_u8`.
  - Any whitespace, `'='`, or invalid byte breaks immediately to the exact scalar state machine for 100% WHATWG/Node semantic parity.
- **Hex Encode** (`crates/cz-modules/src/crypto_module.rs`, shared with `buffer_module.rs`):
  - 16 input bytes -> 32 ASCII hex bytes per iteration via `vqtbl1q_u8` + `vst2q_u8`.
- **WebSocket 4-byte XOR Masking** (`crates/cz-net/src/ws.rs` `apply_mask_in_place`):
  - 64 bytes/iter (4x16B unrolled `vld1q_u8` / `veorq_u8` / `vst1q_u8` with `vdupq_n_u32` mask vector) + 16B step + 4B/1B tail. Zero heap allocations (`alloc_audit_ws_frame_encode_zero == 0`).

### Exactness Verification (on Cortex-A53 Hardware)
- Exhaustive length sweep `0..=260` (every 16B / 48B / 64B boundary and 1/2/3-byte remainder) verified bit-exact against scalar JS reference for `base64`, `base64url`, and `hex`.
- All 256 byte values (`0x00..=0xFF`) round-trip verified.
- Whitespace injection (`\r\n \t`) at lanes `0, 1, 15, 31, 63, 64, 127` across 64B NEON boundaries verified.
- Invalid-character rejection verified across lanes `0, 1, 15, 32, 63, 64` for `atob()`.

### Real-Hardware A/B Results (CardputerZero Cortex-A53 @ 1.0 GHz)
Method: same device, 20 warmup iterations, median of 5 trials, CPU frequency verified at 1000 MHz (`49.4–51.0 °C`, no throttling). Binary size unchanged at `4.7 MB` stripped.

| Workload | Baseline A (Scalar) | Treatment B (ARMv8.0 NEON) | Speedup |
|---|---:|---:|---:|
| `b64 encode 1 KB` | 41.60 us (23.5 MB/s) | **16.09 us (60.7 MB/s)** | **2.59x** |
| `b64 encode 16 KB` | 357.38 us (43.7 MB/s) | **45.20 us (345.7 MB/s)** | **7.91x** |
| `b64 encode 64 KB` | 1721.65 us (36.3 MB/s) | **270.27 us (231.2 MB/s)** | **6.37x** |
| `b64 decode 1 KB` | 74.05 us (17.6 MB/s) | **21.90 us (59.6 MB/s)** | **3.38x** |
| `b64 decode 16 KB` | 827.33 us (25.2 MB/s) | **95.56 us (218.0 MB/s)** | **8.66x** |
| `b64 decode 64 KB` | 3309.27 us (25.2 MB/s) | **322.61 us (258.3 MB/s)** | **10.26x** |
| `b64url encode 16 KB` | 293.99 us (53.1 MB/s) | **45.22 us (345.5 MB/s)** | **6.50x** |
| `b64url decode 16 KB` | 820.92 us (25.4 MB/s) | **95.93 us (217.2 MB/s)** | **8.56x** |
| `hex encode 16 KB` | 338.22 us (46.2 MB/s) | **48.64 us (321.2 MB/s)** | **6.95x** |

### Static ARMv8.0 Audit
- `CZ_JS_LSE=40` (<= 200 threshold; `ring`/std runtime-dispatched outline atomics only)
- `CZ_JS_RCPC=0`
- `NEON_TBL=206`, `NEON_LD4=13`, `NEON_ST4=16`

---

## Round 2 — Hardware PMU Instruction Profile + `hex_decode` NEON + Native `structuredClone` + Single-Pass `cz-json`

### Hardware PMU + PC-Level Disassembly (`JSON.stringify` / `JSON.parse`)
Measured on CardputerZero Cortex-A53 @ 1.0 GHz via `perf_event_open` (hardware PMU counters + 4 kHz `SIGPROF` PC sampling mapped to unstripped `aarch64-linux-gnu-objdump -d`):

| Metric | `JSON.stringify` eager (5.7 KB x 3000) | `JSON.parse` lazy (5.7 KB x 6000) |
|---|---:|---:|
| `IPC` (`CPI`) | `0.866` (`1.155`) | `0.864` (`1.158`) |
| Branch miss rate | `5.61%` | `7.33%` |
| `A53_AGU_STALL` (`0xE5`) | `6.3%` of cycles | `9.9%` of cycles |
| `A53_LD_STALL` (`0xE7`) | `5.3%` of cycles | `4.3%` of cycles |
| `A53_NEON_STALL` (`0xE6`) | `0.4%` of cycles | `1.7%` of cycles |
| **Top hot PC instructions** | `JS_NewStringLen` (`0x2c94c8..0x2c95e4`: scalar `ldrb`/`tbz`/`strh` UTF-8 -> UTF-16 loop because of 1 CJK string in payload), `JS_FreeValueRT`, `JS_GetPropertyInternal` | **`JS_ToCStringLen2`** (`0x2c7c08..0x2c7c28`: **15.0% of all samples**, scalar `ldrh`/`cmp #0x7f`/`strb` UTF-16 -> UTF-8), **`cz_json::Scanner::scan_string`** (`0x7f320`: redundant scalar `ldrsb`/`tbnz` ASCII re-scan) |

### Round 2 Changes
- **`hex_decode` ARMv8.0 NEON** (`crates/cz-modules/src/buffer_module.rs`): `vld2q_u8` + disjoint nibble range decode + `vminvq_u8` validation + `vst1q_u8` (32B -> 16B), breaking to the exact scalar state machine on any whitespace or non-hex byte.
- **Native Rust `structuredClone`** (`crates/cz-modules/src/web_globals.rs`): QuickJS C API fast path (`JS_GetOwnPropertyNames`, `JS_NewObject`/`JS_NewArray`, `JS_DefinePropertyValue`, `HashMap<usize, Value>` cycle map) for primitives, plain objects (`Object.prototype` and `null`-prototype), and arrays; delegates exotic built-ins (`Date`/`RegExp`/`Map`/`Set`/`ArrayBuffer`/`TypedArray`/`DataView`) cleanly to JS fallback.
- **Single-pass `cz-json` `scan_unescaped_run_ascii`** (`crates/cz-json/src/simd.rs`, `parse.rs`): tracks high-bit `< 0x80` during the initial string scan, eliminating the redundant `0x7f320` ASCII re-scan in `scan_string` and `scan_number`.

### Real-Hardware A/B Results (Back-to-Back, CardputerZero Cortex-A53 @ 1.0 GHz)
Method: same device, back-to-back A vs B, 15 warmup iterations, median of 5 trials, 1000 MHz (`49.9–52.1 °C`), `CZ_JS_LSE=40`, `CZ_JS_RCPC=0`, `4.7 MB` stripped, RSS `5.13 -> 5.13 MB`.

| Workload | Baseline A (`aba195e`) | Treatment B (Round 2) | Speedup |
|---|---:|---:|---:|
| `hex decode 1 KB` | 61.66 us (31.7 MB/s) | **20.84 us (93.7 MB/s)** | **2.96x** |
| `hex decode 16 KB` | 745.02 us (41.9 MB/s) | **93.20 us (335.3 MB/s)** | **7.99x** |
| `hex decode 64 KB` | 2954.54 us (42.3 MB/s) | **315.29 us (396.5 MB/s)** | **9.37x** |
| `structuredClone (5.7 KB obj)` | 2957.39 us (1.8 MB/s) | **523.65 us (10.4 MB/s)** | **5.65x** |
| `JSON.parse lazy (5.7 KB)` | 242.81 us (22.4 MB/s) | **232.93 us (23.3 MB/s)** | **1.04x (-4.1%)** |

---

## Round 2B — O(1) Persistent-Tree / Rope-Style CoW `structuredClone(LazyJSON)`

### Design
- `LazyDoc` (`crates/cz-json/src/lazy.rs`) now holds `pub tape: Arc<TapeDoc>` and exposes `LazyDoc::fork(&self) -> Arc<Self>`:
  - Structurally shares the immutable `Arc<TapeDoc>` (0 bytes copied of raw JSON or TapeNode arena) while giving each forked version an independent `BTreeMap<u32, Edit>` CoW overlay.
  - `structuredClone()` (`crates/cz-modules/src/web_globals.rs`) checks `qjs::JS_IsProxy(raw)` (~0 overhead on plain objects) and calls `json_module::try_clone_lazy()` to fork `LazyJSON` handles in O(1) — including nested `LazyJSON` handles inside plain wrapper objects.
  - Each fork carries its own `LazyDocGuard` finalizer; `Arc<TapeDoc>` stays alive until the last version is GC'd.

### Real-Hardware Results (CardputerZero Cortex-A53 @ 1.0 GHz)
- **Version isolation**: clean fork, dirty fork, chained fork (`orig -> c1 -> c2`), and wrapped `{ doc: lazy }` fork all verified 100% isolated.
- **Zero-leak proof**: 5,000 × 64 KB forks (335 MB logical data cloned) → RSS **`5.37 MB -> 5.37 MB`** (`+0.00 MB`).
- **True O(1) scaling**:
  - `structuredClone(LazyJSON 5.7 KB)`: **32.60 us/op** (164.0 MB/s — **90.7x faster** than original 2,957 us JS `structuredClone`)
  - `structuredClone(LazyJSON 64.0 KB)`: **30.20 us/op** (**2,116.0 MB/s** — identical constant time at 12x payload size)


