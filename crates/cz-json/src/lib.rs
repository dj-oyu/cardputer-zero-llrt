//! `cz-json` — Cortex-A53 (ARMv8.0 baseline NEON) one-pass JSON engine with a 16-byte arena
//! `TapeNode` transparent lazy materialization layer.
//!
//! * [`simd`] — Layer 0: NEON/SWAR string scanning, escape writing, 2-digit-table integer
//!   formatting and integer-first parsing.
//! * [`tape`] — Layer 1: the flat 16-byte node arena (`Vec::with_capacity(len / 8)`), strict
//!   RFC 8259 validation and adaptive object key lookup (linear `key_fp` scan `<= 16` children,
//!   cached `HashMap<u64, u32>` beyond that).
//! * [`lazy`] — Copy-on-Write Lazy documents: untouched data never materializes, mutations edit a
//!   sparse map, and serialization slices original bytes for clean subtrees.

pub const LAZY_THRESHOLD_BYTES: usize = lazy::LAZY_THRESHOLD_BYTES;

pub mod lazy;
pub mod parse;
pub mod simd;
pub mod tape;
pub mod val;

pub use lazy::{Got, LazyDoc};
pub use parse::{decode_string_content, parse_dom, parse_owned, validate, ParseErr, Scanner, Sink};
pub use simd::{
    f64_json_string, parse_int_fast, scan_reference, scan_unescaped_run, write_escaped_str, write_f64_json,
    write_i64, write_u64,
};
pub use tape::{
    hash64, key_fingerprint, ChildIter, FLAG_BOOL_TRUE, FLAG_HAS_ESCAPE, FLAG_KEY_ESCAPE, LINEAR_SCAN_MAX, Scalar,
    TapeDoc, TapeNode, TapeSink,
};
pub use val::{Span, Val};

/// Engine identity string used by diagnostics and tests.
pub const ENGINE: &str = "cz-json a53-neon 1-pass + 16B tape lazy";
