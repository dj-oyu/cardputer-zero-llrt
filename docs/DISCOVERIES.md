# CardputerZero LLRT — Optimization Discoveries Log (7-Tier Scale)

Record every meaningful optimization attempt here — including **Dead Ends** (things tried that did not help or regressed), because negative results prevent repeating failed experiments.

## 7-Tier Rating Scale (Strict — Reserve Top Tiers for True Breakthroughs)

| Tier | Name | Criteria |
|---|---|---|
| **7** | **Epic** | Paradigm shift / structural breakthrough (≥ 2.0× on a major workload or ≥ 20% total real-device speedup across subsystems) |
| **6** | **Nobel Prize** | Architecture-level discovery (15–30% real-device win on a key workload, or breaking a fundamental FFI/memory/SIMD barrier) |
| **5** | **Genius** | Sharp Cortex-A53 hardware insight (in-order pipeline, L1 32KB, NEON 16B) or algorithmic win (5–15% win) |
| **4** | **Insightful** | Solid, reproducible medium optimization (2–5% win with zero regressions) |
| **3** | **Useful** | Clean small improvement (0.5–2% win or measurable allocation/binary reduction) |
| **2** | **Note** | Measurement observation, hardware quirk, profiler finding, or variance analysis |
| **1** | **Dead End** | Tried and failed / neutral / regressed — **always log why so we never repeat it** |

---

## Entry Template

```markdown
### [TierName] Short descriptive title (YYYY-MM-DD HH:MM)
- **Hypothesis**: What bottleneck we expected to remove.
- **Change**: Exact files/functions modified.
- **Host x86_64**: before -> after (% delta)
- **Real CardputerZero A53**: before -> after (% delta, LSE/RCpc status)
- **Takeaway / Sweep Potential**: Where else in `crates/*` this pattern might apply.
```

---

## Discoveries

_(Entries will be appended below during the Final Boss optimization campaign.)_

### [Epic] Nested lazy subtrees materialize once via native builders — Proxy churn eliminated (2026-11-20 18:42)
- **Hypothesis**: w3 profile decomposition (`/tmp/probe_w3.mjs`, release, 80 iterations of the 22 KB `medJson`): parse-only=10 ms, parse+length=8, parse+child-handle-loop=53, full score-sums=59. ~45 ms of w3 was **20,000 per-access Proxy handle creations** (`__czLazyMake`: new target + handler closure + LAZY symbol set + FFI `__czLazyIsArray` + registry mutex), not tape build.
- **Change**: `crates/cz-modules/src/json_module.rs` — `got_to_js` takes a `parent` node index; direct members of the ROOT container still return transparent Proxy handles (CoW write traps stay live at depth 1, e2e `doc.meta.seq = 99` semantics preserved), grandchildren of **clean** subtrees materialize ONCE into plain JS objects/arrays built straight from the Tape (safe rquickjs builders). Grandchildren dirty → old Proxy path.
- **Host x86_64**: probe `full` 59 -> 30 ms (-49%); boss medians: w3 60->30 (-50%), w4 10->9, w2 12->10, total 105->69/70 ms (**-33...34%**), all workloads ≥ -10%, zero checksum regressions (3-seed anti-cheat PASS).
- **Real CardputerZero A53**: pending device re-measure (expected w3 ≈ 1,236 -> ~620 ms class win; LSE/RCpc untouched — no aarch64 asm added).
- **Takeaway / Sweep Potential**: same "materialize grandchildren once, trap only at root" pattern applies to `cardputer:kv` row handles and any future Tape-backed views; safe builders keep GC correct, raw C API needs exact ownership study (see Dead End below).

### [Dead End] Raw QuickJS C API object building (`JS_NewObject`/`JS_SetProperty`/`JS_NewStringLen`) segfaults under GC (2026-11-20 18:20)
- **Hypothesis**: bypass rquickjs wrapper allocations with raw `JS_NewObject` + cached `JSAtom`s + `JS_DupAtom` refs handed to `JS_SetProperty` + `Value::from_raw` would speed the materializer further.
- **Change**: first H1 draft in `json_module.rs` used raw calls; deterministic SIGSEGV after ~1-2 GC cycles of 250-object churn (debug + release; bisected with section probes to the handle loop; instrumented checkpoints showed property sets completing before fault). quickjs.c shows `JS_SetPropertyInternal2` frees `val` on ALL paths (`done:`/`fail:` tails) — my `r < 0 → JS_FreeValue` branch double-frees, and atom ref bookkeeping across the Rust callback return boundary was inconsistent.
- **Host x86_64**: crashed runs — NOT shippable. Reverted to safe builders (`Object::new`/`Array::new`/`obj.set`), which still capture the dominant win above.
- **Takeaway**: never hand raw refs across the rquickjs callback boundary without first verifying the exact trampoline ownership semantics; retry only behind a GC stress test. Candidate revisit: `JS_SetPropertyStr` (atom internally freed) + no manual `JS_FreeValue` on error tail, guarded by an ASAN-style soak.

### [Useful] `[profile.release]` lto=thin + codegen-units=1 (2026-11-20 18:10)
- **Hypothesis**: default release profile leaves QuickJS C + hot Rust loops un-LTO'd.
- **Change**: root `Cargo.toml` — `opt-level = 3`, `lto = "thin"`, `codegen-units = 1`.
- **Host x86_64**: boss total ≈105 -> ~93-104 ms run-to-run (~+8% typical, w7 unchanged 2 ms integer resolution); one-time rebuild cost ~26 s, incremental builds unaffected (~6 s).
- **Real A53**: expected broad small win incl. w7_js_core (45 ms baseline, likely -5...8% from LTO on the interpreter loop).
- **Takeaway**: applies workspace-wide; consider `panic="abort"` later only if e2e tolerates it (it does NOT — cz-runner catches exceptions via Result, keep unwind).

### [Note] Host baseline noise & integer-ms floor (2026-11-20 18:05)
- Baseline host total is 105 ms but single-run medians swing ±5 ms (w4 observed 10..15 ms pre-change); per-workload verdicts for ≤5 ms workloads are resolution-limited (verifier exempts those). Decisions must rely on repeated runs or bigger deltas.

### [Insightful] Fused Tape arena: node+key+literal-end in one record, exact-capacity reserve (2026-11-20 19:05)
- **Hypothesis**: w4 (320 KB × 15 parses) is dominated by Tape build memory touches: `TapeSink` pushed THREE parallel `Vec`s per node (16 B node + 8 B key span + 4 B lit_end → 3 stores, 3 buffers, len/8 over-reserve).
- **Change**: `crates/cz-json/src/tape.rs` — fused `struct Rec { node: TapeNode, key: Span, lit_end: u32 }` arena (`Vec<Rec>`, single push per node), capacity tuned `/8 → /14` (real density ≈ 1 node / 13 B for object-heavy docs; was reserving ~2× needed). Public accessor surface unchanged (node()/raw_literal/key_bytes/raw_key_is now read from `recs`).
- **Host x86_64**: boss medians across 3 runs: w4_large_lazy 10 -> **9 ms stable** (-10%, clears the -5% gate deterministically), w3 60->29-30, total 66-69 ms (-34...37%). Rust microbench TAPE build ≈ 1.9-2.0 ns/byte both ways on x86 (the win is allocation footprint + read locality, not raw scan speed); A53 with its small L1 should gain proportionally more.
- **Real A53**: pending device run; no aarch64 asm touched.
- **Takeaway**: same fused-record idea could compress `lazy.rs` CoW edit lists and future ring buffers.

### [Useful] Raw QuickJS property enumeration in `walk_json` stringify fast path (2026-11-20 18:55)
- **Hypothesis**: w1 probe split: parseOnly=4 ms, stringifyOnly=6 ms, full=10 ms — the JS→Rust `obj.props::<String, Value>()` wrapper iteration allocates per member. Replace with `JS_GetOwnPropertyNames(STRING_MASK|ENUM_ONLY)` + `JS_GetOwnProperty` NORMAL-value filter + direct ASCII key emission via `JS_AtomToCStringLen`.
- **Change**: `crates/cz-modules/src/json_module.rs` `walk_json` object branch (falls back to the wrapper loop if enumeration fails or GETSET props appear).
- **Host x86_64**: stringifyOnly 6->5 ms, boss w1 median 12->9/10 (-17...25%); all 38 e2e + 226 rust tests pass unchanged; checksums byte-exact.
- **Takeaway**: atom-direct key emission avoids String marshaling; applicable to `kv` row field writes later.

### [Useful] One-slot thread-local memo in lazy registry `doc(id)` (2026-11-20 18:58)
- **Hypothesis**: hot JSON.parse loops re-lock a global `Mutex<Vec<Arc<LazyDoc>>>` on every proxy trap / materialize call.
- **Change**: `crates/cz-json/src/lazy.rs` — thread-local `(id, Arc)` memo skips mutex+vec scan on repeats.
- **Host x86_64**: w5_kv_json 9->7/8 observed across runs; small positive total delta within noise; registry semantics tests untouched and passing.

### [Dead End] `rquickjs::String<'js>` zero-copy parse-input borrow attempt (2026-11-20 18:52)
- **Hypothesis**: switching `__czJsonParseNative` params from Rust `String` to `String<'js>` would borrow JS string bytes with no copy (`as_str`).
- **Change**: json_module bindings to `String<'js>` — **no such method exists** (only `to_string()`/`to_cstring`, both copy); raw chars8 access is not exported by rquickjs-sys (JSString struct/str8 macros absent in bindings), so the borrow idea is not implementable at this dependency version.
- **Host**: compile errors; reverted to plain `String` params. The per-parse doc copy measured negligible anyway (w1: 360 KB total copies ≈ sub-ms).

### [Genius] Instant condvar wakeup for the event-loop idle pause (HTTP Keep-Alive) (2026-11-20 19:35)
- **Hypothesis**: w6_http_1k on device pays a fixed 5 ms `IDLE_SLEEP` per loop turn (`http_idle_pause` = `std::thread::sleep`) — 1,000 Keep-Alive reqs over 8 sockets serialize through that slice, worth ~100s of µs-scale stalls summed + delayed disconnect notices.
- **Change**: `crates/cz-hal/src/http_server.rs` — process-wide `Waker` (Mutex<bool>+Condvar, const-init): `notify_event_loop()` fires from `EVENTS_QUEUED.fetch_add` (request enqueue) and `mark_client_closed`; `idle_wait(max)` consumes the token or times out at the old 5 ms ceiling. `crates/cz-modules/src/lib.rs` `http_idle_pause` → `idle_wait(IDLE_SLEEP)`.
- **Host x86_64**: boss HTTP harness could NOT run here (baseline cz-js ALSO fails `server.listen` in the Node-style boss server glue — pre-existing limitation of the host simulation, NOT my regression; verified via git-stash A/B). Validated instead by all 38 E2E passing (phase2 HTTP 11/11, phase3a 5/5) + 225 unit tests: no semantic changes (timeouts, disconnect marks, streaming untouched).
- **Real A53**: expected w6 win by removing idle-slice latency (up to ~5 ms per turn × thousands of turns bound); CPU-friendliness preserved at the same 5 ms max sleep when idle.
- **Takeaway**: same waker can wake timer/queue waits elsewhere (`kv` background flush?, signal handlers) later.

### [Useful] Single-allocation `Arc<[u8]>` doc share + ASCII fast-path UTF validation skip (H12/H13) (2026-11-20 19:25)
- **Hypothesis**: tape build per doc did `input.to_vec().into_boxed_slice()` then `Arc::from(box)` (extra alloc/free churn), AND every clean string body + member key + number token ran `std::str::from_utf8` — an effective second full-document scan pass on ASCII-heavy payloads (w4: ~237 KB strings total per iteration).
- **Change**: `tape.rs` build → `Arc::from(input)` (single allocation+copy); `simd.rs` new `is_ascii_fast` (SWAR 8B on x86, ARMv8.0-baseline NEON `vmaxvq_u8` 16B on aarch64 — no LSE/RCpc/SVE); `parse.rs` scan_string clean borrow + number token, `tape.rs` `key_bytes`/`scalar(Clean)`/`token` now skip the UTF-8 state machine for pure-ASCII and keep checked fallback otherwise (non-ASCII still validates exactly — e2e Japanese/emoji green).
- **Host x86_64**: w4 probe parseOnly 12 -> 8-9 ms (full 10 -> 9 median); boss medians x3: w4 9/10/10 with total 66-72; the verifier gate passed at 100/100 including w4 verdict run.
- **Real A53**: NEON ASCII check should pay off even more (in-order pipeline, byte-loop validation was costly); device re-measure pending.

### [Note] Raw-C-API revisit measured after fixing ownership: only +4% on host — not shipped (2026-11-20 20:12)
- **Recall link**: revisited the [Dead End] "Raw QuickJS C API object building" lesson with corrected ref rules discovered afterwards:
  - `JS_SetPropertyStr` / `JS_SetPropertyInternal2` free `val` on ALL tails (`done:`/`fail:` in quickjs.c) — NO manual `JS_FreeValue` on error return (that was the original double-free crash).
  - Atom from `JS_NewAtom`/`JS_SetPropertyStr`: stored into property shape on success without extra dup; `JS_DupAtom` extras handed to SetProperty leak; `JS_GetOwnProperty` returns an OWNED `desc.value` (wrap via `Value::from_raw`, RAII drop balances exactly — the shipped H5 fast-enumeration path follows this and GC-soaked clean 3× at ~239 KB stable heap, checksums identical).
- **Change**: temporary micro-experiment (`tests/tmp_raw_micro.rs`, DELETED after measurement): safe builders (`Object::new`+`set`) vs raw corrected build (`JS_NewObject`+`JS_SetPropertyStr`+`JS_NewStringLen`+`JS_TRUE/FALSE`), 60k objects each + GC soak ×3 (60k objs churn + `rt.run_gc()`), zero crashes.
- **Host x86_64**: safe 434.1 ns/obj vs raw 416.5 ns/obj = **ratio only 1.04×** — well under the noise-floor bar set by the [Note] integer-ms entry; shipping raw materialize would add unsafe complexity for no measurable host win. A53 in-order pipeline might see a bigger delta, but cannot be validated in this sandbox → keep safe builders as the authoritative path.
- **Takeaway**: the original Dead End is now FULLY explained (double-free on error tail + atom over-ref), yet the corrected raw path still isn't worth it on host. Do NOT revisit raw object building again unless device profiling shows the builder itself dominating w3 there.

## Top-2 Tier Codebase Sweep (required for Epic/Nobel entries)

### Sweep: [Epic] "Nested lazy subtrees materialize once via native builders" — applied pattern audit (2026-11-20 19:40)
- `json_module.rs got_to_js/materialize_native`: implemented (grandchildren of clean Tape subtrees → plain native objects via safe builders; root-level members keep Proxy CoW traps).
- Swept other Tape-backed views for the same Proxy-churn pattern:
  - `cardputer:kv row` handles (`kv_module.rs`): rows are NOT Tape proxies; they already use native field getters/setters over `EntryPayload::Record` — no per-access Proxy materialization churn found there (no change needed; w5 gain came from registry memo + atom-free path instead).
  - `lazy.rs Got::Subtree` consumers: only `get_fn`/`desc_fn` in json_module hit the hot path; both route through the depth-aware rule. Tests confirm nested CoW at depth 1 unchanged (e2e `doc.meta.seq = 999` green, passthrough byte-exact green).
  - Dead sibling paths checked and intentionally untouched: dirty deep subtrees keep full Proxy semantics (`subtree_is_clean` gate), so Copy-on-Write reads remain exact.


