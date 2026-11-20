# Final Boss Challenge — Make CardputerZero LLRT >= 30% Faster on Real Cortex-A53 Hardware

## Objective (Strict Win Conditions)
Optimize this Rust + QuickJS runtime (`crates/cz-hal`, `crates/cz-gfx`, `crates/cz-json`, `crates/cz-modules`, `crates/cz-runner`, and `[profile.release]` in `Cargo.toml`) to achieve **genuine, cheat-free speedups**:

1. **Real CardputerZero Cortex-A53 Hardware Target (Mandatory Final Gate)**:
   - Total time across all 7 workloads: **>= 30% faster** (`2,985 ms -> <= 2,089 ms`, `ratio <= 0.70`).
   - **Every single workload individually >= 5% faster** (no 0%-gain or regressed workloads!).
   - Must cross-build cleanly for `aarch64-unknown-linux-gnu` (`-C target-cpu=cortex-a53`) with `LSE <= 200` and `RCpc == 0`.
2. **Real-Device Baseline (Recorded on CardputerZero-1 Cortex-A53, 3-seed median)**:
   | Workload | Real A53 Baseline | Min Target (-5%) | Bottleneck Area |
   |---|---:|---:|---|
   | `w1_small_rest` | 267 ms | <= 253 ms | Small JSON parse+stringify FFI boundary & string copy |
   | `w2_escaped_utf8` | 231 ms | <= 219 ms | NEON 16B escape/unescape & UTF-8 string path |
   | `w3_medium_array` | **1,236 ms** | <= 1,174 ms | **BIGGEST WIN**: 250-object array parse + Atom cache + 1-pass DOM + int fast path |
   | `w4_large_lazy` | 148 ms | <= 140 ms | 16B Tape build, array index O(1) lookup, CoW |
   | `w5_kv_json` | 196 ms | <= 186 ms | `cardputer:kv` row access + direct Rust JSON serialization |
   | `w6_http_1k` | **862 ms** | <= 818 ms | **2nd BIGGEST WIN**: 1,000 Keep-Alive HTTP reqs (event loop sleep, `TCP_NODELAY`, buffer reuse, single `write`) |
   | `w7_js_core` | 45 ms | <= 42 ms | Release profile (`lto`, `codegen-units = 1`), QuickJS C compile flags / allocator |
   | **TOTAL** | **2,985 ms** | **<= 2,089 ms (-30%)** | |
3. **Zero Regressions & Anti-Cheat**:
   - All 225+ Rust unit tests and all 38 E2E tests MUST keep passing.
   - Benchmark verifier runs dynamic random seeds and checks exact FNV-1a checksums against Node.js ground truth (memoization/hardcoding is impossible).
4. **7-Tier Discovery Log (`docs/DISCOVERIES.md`)**:
   - Log EVERY experiment — including **`[Dead End]`** attempts that did not help or regressed!
   - Strict 7-tier scale: `Epic` > `Nobel Prize` > `Genius` > `Insightful` > `Useful` > `Note` > `Dead End`.
   - Every 15 minutes, the harness will inject a non-intrusive reflection prompt; update `docs/DISCOVERIES.md` honestly.
   - After clearing the -30% target, you will perform a codebase-wide sweep for all `Epic` and `Nobel Prize` discoveries!
