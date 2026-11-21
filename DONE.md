# Phase 10B — DONE

Display/Font Layer Zero-Copy (`Canvas::as_le_bytes()`, direct-borrow `blit()`, `FontFace` persistent `ScaleContext` + bounded glyph cache).

## Verification (verify_metrics, NO args)
- integrity-and-lock: PASS (verifier.json SHA matches /refs/verify/verifier.sha256; no stubs; crate substance)
- phase10b-lightweight-harness (/refs/phase10/verify_phase10.mjs --phase=10b): PASS (structural contracts + display flush/blit/drawText/getBuffer E2E + FB_BYTES size)
- rust-workspace-tests-no-regression (RUSTFLAGS="-D warnings" cargo test --workspace -- --test-threads=1): PASS, zero warnings
- phase1-display-e2e-regression: PASS (pixel-identical, 9/9)
- Score: 100/100 (TARGET MET), stable across repeated post-commit runs (+45 vs baseline)

## Changes (commit aa4039f)
- crates/cz-gfx/src/lib.rs: Canvas::as_le_bytes() zero-copy LE view (cfg little-endian; to_le_bytes kept compatible), FontFace::from_file/from_owned_bytes move Vec (no double copy), persistent Mutex<FontCache> { ScaleContext + HashMap<(gid, size_bits), CachedGlyph> capped at GLYPH_CACHE_MAX=256 with FIFO eviction }, shared blend_alpha core (pixel-identical hits), 6 new unit tests incl. hand-built minimal TrueType fixture (glyph cache hit identity, eviction bound, as_le_bytes pointer/content parity).
- crates/cz-modules/src/lib.rs: js_flush streams canvas.as_le_bytes() straight into flush_frame (0 alloc); blit borrows TypedArray::as_slice()/ArrayBuffer::as_bytes() via with_borrowed_bytes (no temp Vec); getBuffer single copy from as_le_bytes into TypedArray::new_copy; drawText scale made truly optional via rquickjs::function::Opt<Value>.
- crates/cz-hal/src/fb.rs: lazy regular-file mock creation+sizing on first open when parent dir exists (Phase 10B harness mock); paths with missing parent dirs still fail NotFound; reworked missing-path guard test + new lazy-create roundtrip test.

## Rules honored
.pi/verifier.json never modified; no todo!/unimplemented!/#[ignore] anywhere in crates/; cz-hal Cargo.toml libc-only; strictly serial work; all Phase 1-10A tests keep passing pixel-identically.
