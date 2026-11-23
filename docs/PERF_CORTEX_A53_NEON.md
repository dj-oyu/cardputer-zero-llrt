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

---

## Round 2C — `JSON.stringify` Eager `walk_json` Fast Path

### Changes (`crates/cz-modules/src/json_module.rs`)
- Pre-intern `"toJSON"` `JSAtom` once per `do_stringify` call (`WalkCtx`) instead of `JS_NewAtomLen` + `JS_FreeAtom` on every visited object/array.
- Stack-allocated 32-slot direct-mapped `JSAtom -> "\"key\":"` L1 cache (0 heap allocations) to skip repeated `JS_AtomToCStringLen` + UTF-8 validation + escape + `JS_FreeCString` across homogeneous object arrays.
- Direct `JS_GetProperty` (replacing `JS_GetOwnProperty` + descriptor setup + `JS_FreeValue` × 2), direct `JS_GetPropertyUint32` for arrays, and direct `JS_TAG_INT` / `JS_TAG_FLOAT64` / `JS_TAG_STRING` tag dispatch.

### Real-Hardware A/B Results (Back-to-Back, CardputerZero Cortex-A53 @ 1.0 GHz)
| Workload | Baseline A (`64117a8`) | Treatment B (`walk_json` opt) | Speedup |
|---|---:|---:|---:|
| `stringify eager 5.7 KB (w/ CJK)` | 649.00 us (8.4 MB/s) | **438.73 us (12.4 MB/s)** | **1.48x (-32.4%)** |
| `stringify eager 5.7 KB (ASCII)` | 555.40 us (9.8 MB/s) | **349.81 us (15.5 MB/s)** | **1.59x (-37.0%)** |
| `stringify small 60 B` | 22.47 us (2.2 MB/s) | **19.51 us (2.6 MB/s)** | **1.15x (-13.2%)** |

*(Note: the remaining +88.9 us delta between ASCII and CJK-containing 5.7 KB payloads is `JS_NewStringLen` in QuickJS C, matching the Round 2 PMU profile.)*

---

## Round 3 — Forked `dj-oyu/quickjs` Engine Optimizations (PR #2, #3, #4)

All engine optimizations live in `vendor/quickjs` (`dj-oyu/quickjs`), verified back-to-back A/B on CardputerZero Cortex-A53 @ 1.0 GHz with `LSE=26, RCPC=0` and zero impact on ESP32-S3 / non-NEON targets.

### 3A. AArch64 NEON UTF-8 / String Ops & 128-bit Schubfach/Eisel-Lemire `dtoa`/`atod` (PR #2)
- `cutils.h` / `quickjs.c`: 16-byte NEON ASCII fast paths in `utf8_scan`, `utf8_decode_buf8`, `utf8_decode_buf16`, `JS_ToCStringLen2` (placed in `else` branch with `(p[7]|p[15]) < 0x80` pre-probe so dense CJK has 0 extra branches).
- `quickjs.c`: 16-byte NEON `toUpperCase`/`toLowerCase` (`js_string_toLowerCase`) and 8-bit/16-bit NEON `string_indexof_char` (routed from `js_string_indexOf` / `js_string_includes`).
- `dtoa.c` + `dtoa_schubfach_table.h`: shared 10.7 KB 128-bit normalized `10^e` table (`e ∈ [-342..324]`) powering both 128-bit **Schubfach** (`js_dtoa`, 26,332 floats verified round-trip 0 failures + ECMAScript shortest output on asymmetric powers of 2) and Clinger FPU + 128-bit **Eisel-Lemire** (`js_atod`, 25,090 decimal strings verified 64-bit IEEE-754 bit-exact).

| Workload | Baseline (`ff809dd`) | PR #2 | Speedup |
|---|---:|---:|---:|
| `TextDecoder` mixed 16 KB | — | 259 MB/s | **4.73x** |
| `TextEncoder` mixed 16 KB | — | 402 MB/s | **4.07x** |
| `TextDecoder` / `TextEncoder` dense CJK | — | — | **1.04x** |
| `str.toUpperCase` (4 KB ASCII) | 319.90 us | **5.21 us** (749 MB/s) | **61.4x** |
| `str.toLowerCase` (4 KB ASCII) | 311.26 us | **5.36 us** (729 MB/s) | **58.1x** |
| `str.indexOf` (4 KB ASCII) | 83.02 us | **4.36 us** (897 MB/s) | **19.0x** |
| `str.indexOf` (4 KB CJK) | 80.94 us | **7.08 us** (1,036 MB/s) | **11.4x** |
| `float64.toString` (16 mixed floats) | 410.9–430.3 us | **34.4–35.3 us** | **11.9x–12.2x** |
| `float64.toExponential` (16 mixed) | 411.5–416.5 us | **36.5–36.7 us** | **11.3x** |
| `parseFloat` / `Number(str)` (16 full) | 61.7 / 64.3 us | **30.9 / 33.2 us** | **1.94x–2.00x** |

### 3B. Lazy / CoW `JS_CLASS_ARRAY` Backing (`split`, `slice`, `Object.keys`) (PR #3)
- Architecture-independent `JSLazyArrayData` using unused `p->u.array.u.ptr` when `fast_array == 0 && count == 0` (0 extra bytes in `JSObject`, 0% overhead on normal `fast_array == 1` arrays via `no_inline js_lazy_array_get_elem`, cycle-GC integrated via `JS_GC_OBJ_TYPE_LAZY_SLICE_BUF`).

| Workload | PR #2 Baseline | PR #3 (`JSLazyArray`) | Speedup |
|---|---:|---:|---:|
| `arr(10k).slice(100, 9000)` + `[0]` | 2,202–2,387 us | **6.32–6.52 us** | **338x–378x** |
| `arr(10k).slice(100, 9000)` + full sum | 6,075–6,079 us | **4,782–4,891 us** | **1.24x–1.27x** |
| `split('\n').slice(0, 10)` (15.7 KB / 400 lines) | 542–571 us | **74.2–76.4 us** | **7.1x–7.7x** |
| `split('\n').length` (400 lines) | 444–467 us | **67.6–71.8 us** | **6.2x–6.9x** |
| `for..of split('\n')` (all 400 lines) | 666–702 us | **476–488 us** | **1.36x–1.47x** (+ O(1) peak RAM) |
| `Object.keys(100).length` | 77.1–80.7 us | **19.3–19.7 us** | **3.9x–4.2x** |
| `for..of Object.keys(100)` | 133.8–140.3 us | **82.3–84.4 us** | **1.59x–1.70x** |
| `normalArr(2k)` read+write (sanity) | 1,068.3 us | **1,067.8 us** | **1.00x (0% overhead)** |

### 3C. Allocation Elimination (`RegExp.test`, `Object.values`/`entries`, `fn.apply`, `JSON.stringify`) (PR #4)
- Zero-alloc `RegExp.prototype.test` (`test_only` path + `capture_stack[32]`), plain-object direct `p->prop[i]` path for `Object.values`/`entries`, 16-slot stack buffer for `Function.prototype.apply`, and zero-intermediate-`JSString` native `JSON.stringify` (direct quoting, direct `i32toa`/`js_dtoa`/bool/null, C pointer circular stack, 16-key C-stack plain-object walk).

| Workload | PR #3 Baseline | PR #4 (`alloc-killers`) | Speedup |
|---|---:|---:|---:|
| `RegExp.test` (named groups) | 16.88–17.38 us | **3.14–3.16 us** | **5.4x–5.5x** |
| `RegExp.test` (6 captures) | 16.04–16.42 us | **4.97–4.99 us** | **3.2x–3.3x** |
| `JSON.stringify(500 ints, '')` | 789–797 us | **160.2–161.4 us** | **4.9x–5.0x** |
| `JSON.stringify(200 strs, '')` | 568–571 us | **118.5–121.3 us** | **4.7x–4.8x** |
| `JSON.stringify(200 floats, '')` | 459–461 us | **182.3–185.6 us** | **2.5x** |
| `JSON.stringify(nested, '')` | 1,536–1,608 us | **544.7–560.1 us** | **2.7x–3.0x** |
| `JSON.stringify(nested, 2)` | 1,864–1,994 us | **882.6–930.9 us** | **2.0x–2.3x** |
| `Object.values(100)` / `Object.values(5)` | 74.0 / 9.3 us | **18.6 / 3.7 us** | **4.0x / 2.6x** |
| `Object.entries(5)` / `Object.entries(100)` | 22.2 / 355 us | **10.3 / 232 us** | **2.2x / 1.5x** |
| `fn.apply` (4 / 12 args) | 2.55–3.04 us | **2.18–2.45 us** | **1.17x–1.24x** |

### Parser & `pi-sandbox` Fuzzing (`crates/cz-runner/tests/fuzz_parsers.rs`)
- Deterministic 70,000+ mutation & adversarial fuzz suite (`cz-json` 25k+, `cz-net::url` 25k+, `cz-net::ws` 20k, plus 7 hostile `pi-sandbox` resource bombs & 2,000+ FFI calls under 16 MB memory / 256 KB stack / interrupt limits): **4/4 PASS in 0.08s**.

---

## Round 4: QuickJS VM Main-Loop I-Cache & Dispatch Optimization (No JIT, Cortex-A53 Hardware PMU A/B)

Guided by Cortex-A53 hardware PMU counters (`0x11 CPU_CYCLES`, `0x08 INST_RETIRED`, `0x01 L1I_CACHE_REFILL`, `0x03 L1D_CACHE_REFILL`, `0x10 BR_MIS_PRED`), targeting Cortex-A53's 32 KiB 2-way L1I cache without JIT or new bytecode opcodes:

### 4A. Zero-New-Opcode Comparison & Short-Circuit Fusion (`dj-oyu/quickjs` PR #6 & PR #7)
- **PR #6 (`perf/vm-cmp-branch-fuse`, merged)**: fuses all 8 comparison opcodes (`OP_lt`..`OP_strict_neq`) with `OP_if_false8` via one shared `cmp_fuse_done:` tail (`+64 B` in `JS_CallInternal`):
  - `w_interp`: **-10.4% cycles, -8.5% inst, -54.8% L1I refills, -28.1% branch misses**
  - `w_arith`: **-5.3% cycles, -7.8% inst**
  - `w_lexer`: **-3.1% cycles, -1.9% inst, -38.8% L1I refills**
- **PR #7 (`perf/vm-dup-branch-fuse`, merged)**: extends `cmp_fuse_done:` to fuse the 4-opcode JS short-circuit chain `OP_cmp + OP_dup + OP_if_false8/OP_if_true8 (+ OP_drop)` with an independent `BREAK;` on the `OP_if_false8` fast path (incremental vs. PR #6):
  - `w_lexer`: **-14.9% cycles, -15.0% inst (-25.3M inst), -23.8% branch misses**
  - `w_interp`: **-0.7% cycles, -6.5% branch misses, -0.5% L1I refills**
  - `w_arith`: **-0.2% cycles, -0.1% inst**

### 4B. Shared Cold-Opcode Outlining (`dj-oyu/quickjs` PR #8, `js_call_cold_op`)
- Outlines 52 cold opcodes (688 source lines) out of `JS_CallInternal` into a single `static no_inline qjs_cold` helper in `.text.unlikely` behind one shared 20 B call stub (preserving `sf->cur_sp == NULL` while running).
- `JS_CallInternal`: `47,764 B` -> **`37,572 B` (-10,192 B / -21.3%)**, total binary **-1,172 B**, hot 66-opcode span `23.9 KiB` -> **`18.3 KiB`**.
- Cortex-A53 Hardware PMU A/B (no PGO, vs. PR #7): `w_interp` **`L1I_REFILL` -25.8%, `CPU_CYCLES` -4.8%**; `w_lexer` **`CPU_CYCLES` -1.15%, `BR_MIS_PRED` -3.8%**.

### 4C. Inline TypedArray & `charCodeAt` Fast Paths (`dj-oyu/quickjs` PR #9)
- Inline in-bounds `Int32Array`/`Uint8Array` read/write in `OP_get_array_el`/`OP_put_array_el` (checking `!typed_array_is_immutable(p)`) and inline `String.prototype.charCodeAt` in `OP_call_method` (+50 lines, `JS_CallInternal` +556 B, `quickjs.o` `.text` +832 B / +0.089%).
- Cortex-A53 Hardware PMU A/B (no PGO, vs. PR #8): `w_lexer` **-22.6% cycles (1.29x), -26.0% inst (-37.5M inst)**; `w_interp` **-2.1% cycles, -10.2% branch misses, -46% L1D refills**; `w_arith` 0.00% instruction overhead.

### 4D. `cz-js` AArch64 GCC PGO (`vendor/rquickjs-sys/build.rs` + `scripts/pgo-train.mjs`)
- Optional `CZ_QJS_PGO_GEN=<dir>` / `CZ_QJS_PGO_USE=<dir>` flags in `vendor/rquickjs-sys/build.rs` using `-fno-reorder-blocks-and-partition` (required on AArch64 GCC for computed-goto / switch jump tables), trained with VM loops, calls, JSON/RegExp, and CJK UTF-8 (`w_utf8_cjk`).
- Binary size: `5.8 MB` -> **`5.6 MB`** (`CZ_JS_LSE=40`, `CZ_JS_RCPC=0`), Peak RSS `6.08 MB` -> **`5.82 MB`**.
- Real Cortex-A53 hardware A/B (on top of PR #6 + #7 + #8 + #9):

| Benchmark | `cz-js` NO-PGO | `cz-js` **PGO (CJK-inclusive)** | Speedup |
|---|---:|---:|---:|
| `w_lexer` (vs. pre-PR #9 `174.8 ms`) | 130.17 ms | **115.59 ms** | **-11.2% (1.51x vs. pre-PR #9)** |
| `w_interp` (bytecode VM + TypedArray) | 33.30 ms | **27.00 ms** | **-18.9% (1.23x)** |
| `w_arith` (float arithmetic loop) | 76.73 ms | **67.07 ms** | **-12.6% (1.14x)** |
| `w_fib28` (recursive JS calls) | 530.28 ms | **496.01 ms** | **-6.5% (1.07x)** |
| `w_json` (JSON.stringify + parse) | 76.17 ms | **70.76 ms** | **-7.1% (1.08x)** |
| `JS: 2M integer loop` | 510.9 ms | **444.5 ms** | **-13.0% (1.57x vs. LLRT)** |
| `LazyJSON field lookup (50k)` | 2680.7 ms | **2211.7 ms** | **-17.5%** |
| `JSON.stringify (7KB eager / lazy)` | 430.6 / 69.1 ms | **376.3 / 61.1 ms** | **-12.6% / -11.6%** |
| `TextDecoder CJK (17KB)` | 532.0 ms | **504.4 ms** | **-5.2%** |
| `cardputer:kv set / get` | 32.9 / 61.5 ms | **27.7 / 51.1 ms** | **-15.8% / -16.9%** |
| `new URL() parse (5k)` | 310.1 ms | **282.8 ms** | **-8.8%** |

---

## Round 5: NEON JSON/Split/ReplaceAll & Prototype-Walk / `putc8` Cleanup (`dj-oyu/quickjs` PR #10 & PR #11)

### 5A. ARMv8.0 NEON 16B Scan & Zero-Alloc `replaceAll` (PR #10)
- 16 B/iter ARMv8.0 NEON scans in `string_buffer_append_quoted`, `json_parse_string` (signed `vcltq_s8(v, 0x20)`), and 1-char `js_string_split` (NEON `vceqq_u8` + `vaddv_u8` bitmask), plus zero-alloc plain `js_string_replace` / `replaceAll` when replacement contains no `$` (+1,232 B / +0.13% in `quickjs.o` `.text`, 0 B in `JS_CallInternal`).
- Real Cortex-A53 A/B (2x back-to-back):
  - `split(",").length` (3000 fields): `80.42 ms` -> **`19.20 ms` (4.19x)**
  - `replaceAll(",", ";")` (3000x): `396.06 ms` -> **`94.45 ms` (4.19x)**
  - `replaceAll("ok", "PASS")` (600x): `184.51 ms` -> **`63.55 ms` (2.90x)**
  - `split("\n").length` (600 lines): `39.37 ms` -> **`28.09 ms` (1.40x)**
  - `JSON.stringify` (200 objs, ~45KB): `55.00 ms` -> **`51.10 ms` (1.08x)**

### 5B. Default `POCKET_VM_LAZY_BUILTINS=0` + Lock-Bit Fix + Inline `string_buffer_putc8` (PR #11)
- Hardware PMU PC sampling revealed `js_lazy_touch` (`POCKET_VM_LAZY_BUILTINS=1`) was the #1 hotspot (13.5% cycles) on prototype-chain misses (`Object.prototype` stayed lazy forever on missing `toJSON` lookups), and `string_buffer_putc8` was not inlined (8.2% cycles in `JSON.stringify`).
- Defaulted `POCKET_VM_LAZY_BUILTINS` to `0` (matching upstream `quickjs-ng`, with `#if` guards + `lazy-builtins` CI job testing `=1`), moved `js_obj_set_locked(p, false)` out of `#if POCKET_VM_LAZY_BUILTINS`, and inlined `string_buffer_putc8` fast path (`quickjs.o` `.text`: `-9,960 B`).
- Real Cortex-A53 A/B (2x back-to-back average):
  - `plain.toJSON` missing prop (200k): `226.99 ms` -> **`115.35 ms` (1.97x, -49.2%)**
  - `proto_walk` (200k `toJSON` + `a`): `321.19 ms` -> **`204.55 ms` (1.57x, -36.3%)**
  - `JSON.stringify` (200 objs x 80): `81.73 ms` -> **`65.24 ms` (1.25x, -20.2%)**





