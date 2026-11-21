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
