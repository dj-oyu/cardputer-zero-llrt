//! Layer 0 — Cortex-A53 ARMv8.0 NEON & portable SWAR JSON primitives.
//!
//! Everything here is lifetime- and allocation-free: `scan_unescaped_run` finds the first byte
//! that must be escaped while serializing/parsing a JSON string body, `write_escaped_str` bulk
//! copies clean runs, `write_i64` formats integers through a 200-byte two-digit lookup table,
//! and `parse_int_fast` bypasses the `f64` parser for pure integer tokens. On `aarch64` the scan
//! uses baseline ARMv8.0 NEON only (`vld1q_u8`, `vdupq_u8`, `vceqq_u8`, `vcltq_u8`, `vorrq_u8`,
//! `vmaxvq_u8`) — NO LSE/RCpc/SVE instructions, so it runs on Cortex-A53. The non-aarch64 path is
//! an identical-semantics 8-bytes-per-iteration SWAR loop so x86_64 tests prove the same behavior.

/// A JSON string body must stop at `"`, `\` or any control byte `< 0x20`.
const QUOTE: u8 = b'"';
const BACKSLASH: u8 = b'\\';

/// Returns the index of the first byte at or after `start` that is `"` , `\` or `< 0x20`,
/// or `bytes.len()` when the whole tail is clean.
#[inline]
pub fn scan_unescaped_run(bytes: &[u8], start: usize) -> usize {
    let n = bytes.len();
    if start >= n {
        return n;
    }
    #[cfg(target_arch = "aarch64")]
    {
        neon_scan(bytes, start)
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        swar_scan(bytes, start)
    }
}

// ---------------------------------------------------------------------------
// ARMv8.0 baseline NEON (Cortex-A53): 16 bytes per iteration
// ---------------------------------------------------------------------------

#[cfg(target_arch = "aarch64")]
use core::arch::aarch64::{
    vbslq_u8, vceqq_u8, vcltq_u8, vdupq_n_u8, vld1q_u8, vmaxvq_u8, vminvq_u8, vorrq_u8,
};

#[cfg(target_arch = "aarch64")]
fn neon_scan(bytes: &[u8], start: usize) -> usize {
    let n = bytes.len();
    // Lane index vector 0..15, used to recover the first hit lane without a movemask.
    let lanes: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
    let mut i = start;
    unsafe {
        let lane_v = vld1q_u8(lanes.as_ptr());
        let quote_v = vdupq_n_u8(QUOTE);
        let slash_v = vdupq_n_u8(BACKSLASH);
        let ctrl_v = vdupq_n_u8(0x20);
        let all_ones = vdupq_n_u8(u8::MAX);

        while i + 16 <= n {
            let v = vld1q_u8(bytes.as_ptr().add(i));
            let eq_quote = vceqq_u8(v, quote_v);
            let eq_slash = vceqq_u8(v, slash_v);
            let lt_ctrl = vcltq_u8(v, ctrl_v);
            let hit = vorrq_u8(vorrq_u8(eq_quote, eq_slash), lt_ctrl);
            // vmaxvq reduces the 16 compare lanes: zero => the whole chunk is clean.
            if vmaxvq_u8(hit) != 0 {
                // Matched lanes keep their index, clean lanes become 255; vminvq yields the
                // first (lowest) matching lane index inside this chunk.
                let sel = vbslq_u8(hit, lane_v, all_ones);
                return i + vminvq_u8(sel) as usize;
            }
            i += 16;
        }
    }
    // Scalar tail (< 16 bytes).
    while i < n {
        let b = bytes[i];
        if b == QUOTE || b == BACKSLASH || b < 0x20 {
            return i;
        }
        i += 1;
    }
    n
}

// ---------------------------------------------------------------------------
// Portable SWAR fallback (x86_64 / anything else): 8 bytes per iteration
// ---------------------------------------------------------------------------

#[cfg(not(target_arch = "aarch64"))]
const ONES: u64 = 0x0101010101010101;
#[cfg(not(target_arch = "aarch64"))]
const HIGH: u64 = 0x8080808080808080;

/// Per-byte equality against an ASCII byte `v`, producing the high bit of every matching lane.
/// Lanes whose original byte had the top bit set (UTF-8 continuation bytes) never match.
#[cfg(not(target_arch = "aarch64"))]
#[inline]
fn swar_eq(w: u64, v: u8) -> u64 {
    let x = w ^ (ONES * u64::from(v));
    let zero = x.wrapping_sub(ONES) & !x & HIGH;
    zero & !(w & HIGH)
}

/// High bit set in every lane holding a control byte (`< 0x20`): bits 6 and 5 are both zero and
/// the top bit is clear (bytes >= 0x80 are valid UTF-8 text, never JSON controls).
#[cfg(not(target_arch = "aarch64"))]
#[inline]
fn swar_lt_0x20(w: u64) -> u64 {
    let ascii_lanes = !w & HIGH; // top bit marker for bytes < 0x80
    let bits56 = w & 0x6060606060606060;
    swar_eq(bits56, 0) & ascii_lanes
}

#[cfg(not(target_arch = "aarch64"))]
#[inline]
fn swar_hit_mask(w: u64) -> u64 {
    swar_eq(w, QUOTE) | swar_eq(w, BACKSLASH) | swar_lt_0x20(w)
}

#[cfg(not(target_arch = "aarch64"))]
fn swar_scan(bytes: &[u8], start: usize) -> usize {
    let n = bytes.len();
    let mut i = start;
    while i + 8 <= n {
        let chunk: [u8; 8] = [
            bytes[i],
            bytes[i + 1],
            bytes[i + 2],
            bytes[i + 3],
            bytes[i + 4],
            bytes[i + 5],
            bytes[i + 6],
            bytes[i + 7],
        ];
        let w = u64::from_le_bytes(chunk);
        let m = swar_hit_mask(w);
        if m != 0 {
            // Each hit sets that lane's top bit; the lowest set bit is the first hit in LE order.
            return i + (m.trailing_zeros() / 8) as usize;
        }
        i += 8;
    }
    while i < n {
        let b = bytes[i];
        if b == QUOTE || b == BACKSLASH || b < 0x20 {
            return i;
        }
        i += 1;
    }
    n
}

/// Portable reference scan used by unit tests to prove the SIMD/SWAR semantics byte-for-byte.
pub fn scan_reference(bytes: &[u8], start: usize) -> usize {
    for i in start..bytes.len() {
        let b = bytes[i];
        if b == QUOTE || b == BACKSLASH || b < 0x20 {
            return i;
        }
    }
    bytes.len()
}

// ---------------------------------------------------------------------------
// Fast integer formatting (200-byte two-digit lookup table)
// ---------------------------------------------------------------------------

/// `const`-built 200-byte table of decimal pairs: `DIGITS[2 * d]`/`DIGITS[2 * d + 1]` = "00".."99".
const DIGITS: [u8; 200] = build_digits();

const fn build_digits() -> [u8; 200] {
    let mut t = [0u8; 200];
    let mut d = 0usize;
    while d < 100 {
        t[2 * d] = b'0' + (d / 10) as u8;
        t[2 * d + 1] = b'0' + (d % 10) as u8;
        d += 1;
    }
    t
}

/// Append the decimal form of an unsigned integer without per-digit division overhead.
#[inline]
pub fn write_u64(out: &mut Vec<u8>, v: u64) {
    if v < 100 {
        if v >= 10 {
            out.push(DIGITS[(v as usize) * 2]);
            out.push(DIGITS[(v as usize) * 2 + 1]);
        } else {
            out.push(b'0' + v as u8);
        }
        return;
    }
    // Work out how many digits we need, then fill from the back using pair lookups.
    let digits = digit_count(v);
    let len = out.len();
    out.resize(len + digits, 0u8);
    let mut rest = v;
    let mut pos = len + digits;
    while rest >= 100 {
        let q = rest / 100;
        let r = (rest - q * 100) as usize;
        pos -= 2;
        out[pos] = DIGITS[r * 2];
        out[pos + 1] = DIGITS[r * 2 + 1];
        rest = q;
    }
    if rest >= 10 {
        pos -= 2;
        out[pos] = DIGITS[rest as usize * 2];
        out[pos + 1] = DIGITS[rest as usize * 2 + 1];
    } else {
        pos -= 1;
        out[pos] = b'0' + rest as u8;
    }
}

const fn digit_count(v: u64) -> usize {
    // Threshold-based decimal length (no log/loop).
    if v < 10 {
        1
    } else if v < 100 {
        2
    } else if v < 1000 {
        3
    } else if v < 10000 {
        4
    } else if v < 100000 {
        5
    } else if v < 1000000 {
        6
    } else if v < 10000000 {
        7
    } else if v < 100000000 {
        8
    } else if v < 1000000000 {
        9
    } else if v < 10000000000 {
        10
    } else if v < 100000000000 {
        11
    } else if v < 1000000000000 {
        12
    } else if v < 10000000000000 {
        13
    } else if v < 100000000000000 {
        14
    } else if v < 1000000000000000 {
        15
    } else if v < 10000000000000000 {
        16
    } else if v < 100000000000000000 {
        17
    } else if v < 1000000000000000000 {
        18
    } else if v < 10000000000000000000 {
        19
    } else {
        20
    }
}

/// Append the decimal form of a signed integer (`-` prefix, magnitude via the pair table).
#[inline]
pub fn write_i64(out: &mut Vec<u8>, v: i64) {
    if v < 0 {
        out.push(b'-');
        // Two's-complement magnitude stays inside u64 even for i64::MIN.
        write_u64(out, (v as u64).wrapping_neg());
    } else {
        write_u64(out, v as u64);
    }
}

/// Parse a pure-integer token (optional `-`, digits only — no `.`/`e`/`E`) directly into `i64`.
/// Returns `None` when the token has any float part, is malformed, or overflows `i64` (callers
/// then fall back to the `f64` parser exactly like QuickJS does).
#[inline]
pub fn parse_int_fast(tok: &[u8]) -> Option<i64> {
    let (neg, digits) = split_int_token(tok)?;
    if digits.is_empty() || !all_digits(digits) {
        return None;
    }
    // Leading zeros are invalid JSON (`01`) — reject so callers surface a SyntaxError.
    if digits.len() > 1 && digits[0] == b'0' {
        return None;
    }
    let mut acc: u64 = 0;
    for &d in digits {
        let x = (d - b'0') as u64;
        acc = acc.checked_mul(10)?.checked_add(x)?;
    }
    if neg {
        // i64::MIN magnitude = 9223372036854775808 is representable.
        if acc > 9223372036854775808 {
            return None;
        }
        Some(if acc == 9223372036854775808 {
            i64::MIN
        } else {
            -(acc as i64)
        })
    } else {
        if acc > i64::MAX as u64 {
            return None;
        }
        Some(acc as i64)
    }
}

/// Split an integer token into `(negative?, digit slice)`, rejecting any `.`/`e`/`E` presence.
fn split_int_token(tok: &[u8]) -> Option<(bool, &[u8])> {
    if tok.is_empty() {
        return None;
    }
    for &b in tok {
        if b == b'.' || b == b'e' || b == b'E' {
            return None;
        }
    }
    if tok[0] == b'-' {
        Some((true, &tok[1..]))
    } else {
        Some((false, tok))
    }
}

fn all_digits(s: &[u8]) -> bool {
    s.iter().all(|b| b.is_ascii_digit())
}

// ---------------------------------------------------------------------------
// String escaping (NEON/SWAR bulk copy + minimal escapes)
// ---------------------------------------------------------------------------

/// Escape one byte into its shortest JSON form (lowercase `\u00xx` per ECMA-262).
fn escape_byte(out: &mut Vec<u8>, b: u8) {
    match b {
        QUOTE => out.extend_from_slice(b"\\\""),
        BACKSLASH => out.extend_from_slice(b"\\\\"),
        0x08 => out.extend_from_slice(b"\\b"),
        0x0c => out.extend_from_slice(b"\\f"),
        0x0a => out.extend_from_slice(b"\\n"),
        0x0d => out.extend_from_slice(b"\\r"),
        0x09 => out.extend_from_slice(b"\\t"),
        _ => {
            out.extend_from_slice(b"\\u00");
            let hi = (b >> 4) & 0xf;
            let lo = b & 0xf;
            out.push(if hi < 10 { b'0' + hi } else { b'a' + (hi - 10) });
            out.push(if lo < 10 { b'0' + lo } else { b'a' + (lo - 10) });
        }
    }
}

/// Serialize a string body as a JSON string literal using NEON/SWAR clean-run bulk copies.
pub fn write_escaped_str(out: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    out.push(QUOTE);
    let mut p = 0usize;
    while p < bytes.len() {
        let q = scan_unescaped_run(bytes, p);
        if q > p {
            out.extend_from_slice(&bytes[p..q]);
        }
        if q == bytes.len() {
            break;
        }
        escape_byte(out, bytes[q]);
        p = q + 1;
    }
    out.push(QUOTE);
}

fn sign_bit(v: f64) -> bool {
    (v.to_bits() >> 63) & 1 == 1
}

/// ECMA-262 `Number::toString(10)` text for finite numbers (`NaN`/infinity become `null`, which
/// is what `JSON.stringify` emits). Matches V8/QuickJS output exactly, including exponent form.
pub fn write_f64_json(out: &mut Vec<u8>, v: f64) {
    if !v.is_finite() {
        out.extend_from_slice(b"null");
        return;
    }
    // Fast integer path through the two-digit table.
    if v.fract() == 0.0 && v.abs() <= 9_000_000_000_000_000_000.0 {
        if v == 0.0 {
            // JS prints negative zero as "-0" via ToString.
            if sign_bit(v) {
                out.push(b'-');
            }
            out.push(b'0');
            return;
        }
        write_i64(out, v as i64);
        return;
    }
    es_digits_format(out, v);
}

/// Turn a finite non-integer into ECMA §10.2.1.2 decimal text using Ryu's shortest digits from
/// `{:e}` (`mantissae + exponent`) then applying the exact ECMAScript formatting rules.
fn es_digits_format(out: &mut Vec<u8>, v: f64) {
    let neg = v < 0.0 || (v == 0.0 && sign_bit(v));
    let abs = if neg { -v } else { v };
    let s = format!("{abs:e}");
    // Parse "D[.DDD]eEX" into digit string + exponent E.
    let (mant, exp_str) = match s.split_once('e') {
        Some(p) => p,
        None => (s.as_str(), "0"),
    };
    let e: i32 = exp_str.parse().unwrap_or(0);
    let mut digits: Vec<u8> = Vec::with_capacity(24);
    for c in mant.bytes() {
        if c != b'.' {
            digits.push(c);
        }
    }
    // Strip leading zeros (they only shift the exponent bookkeeping).
    while digits.first() == Some(&b'0') && digits.len() > 1 {
        digits.remove(0);
    }
    // Trailing zeros are never part of the mathematical value's shortest representation.
    while digits.len() > 1 && digits.last() == Some(&b'0') {
        digits.pop();
    }
    let k = digits.len() as i32;
    let n = e + 1; // value = digits * 10^(n - k)

    if neg {
        out.push(b'-');
    }
    if k <= n && n <= 21 {
        out.extend_from_slice(&digits);
        for _ in 0..(n - k) {
            out.push(b'0');
        }
    } else if n > 0 && n <= 21 {
        // n < k: insert the decimal point after n digits.
        out.extend_from_slice(&digits[..n as usize]);
        out.push(b'.');
        out.extend_from_slice(&digits[n as usize..]);
    } else if n > -6 && n <= 0 {
        out.extend_from_slice(b"0.");
        for _ in 0..(-n) {
            out.push(b'0');
        }
        out.extend_from_slice(&digits);
    } else if n == k + 1 && k <= 21 {
        out.extend_from_slice(&digits);
        out.extend_from_slice(b"e+");
        write_i32(out, n - 1);
    } else {
        // Exponential form with a single leading digit.
        out.push(digits[0]);
        if k > 1 {
            out.push(b'.');
            out.extend_from_slice(&digits[1..]);
        }
        out.push(b'e');
        let ex = n - 1;
        if ex < 0 {
            out.push(b'-');
            write_i32(out, -ex);
        } else {
            out.push(b'+');
            write_i32(out, ex);
        }
    }
}

fn write_i32(out: &mut Vec<u8>, v: i32) {
    write_i64(out, v as i64);
}

/// The exact `Number::toString` string (used by tests and for small helper paths).
pub fn f64_json_string(v: f64) -> String {
    let mut out = Vec::new();
    write_f64_json(&mut out, v);
    // JSON number text is always ASCII.
    String::from_utf8(out).expect("json numbers are ascii")
}

#[cfg(test)]
mod simd_tests {
    use super::*;

    #[test]
    fn scan_agrees_with_reference_on_tricky_buffers() {
        // Mixes UTF-8 continuation bytes, escapes, quotes, controls and chunk-boundary offsets.
        let mut buf: Vec<u8> = Vec::new();
        for chunk in 0..17usize {
            buf.extend_from_slice(&[
                b'a', b'b', 0xe6, 0x97, 0xa5, // '日' continuation bytes must not stop the scan
                b'c', 0xc3, 0x28,
            ]);
            if chunk % 3 == 0 {
                buf.push(b'"');
            }
            if chunk % 5 == 0 {
                buf.push(b'\\');
            }
            if chunk % 7 == 0 {
                buf.push(0x1f);
            }
            if chunk % 11 == 0 {
                buf.push(0x7f); // DEL stays clean like QuickJS/V8
            }
            buf.push(b'z');
        }
        for start in 0..buf.len() {
            assert_eq!(
                scan_unescaped_run(&buf, start),
                scan_reference(&buf, start),
                "scan mismatch at start={start}"
            );
        }
    }

    #[test]
    fn scan_returns_len_for_clean_tails_and_stops_at_specials() {
        let s = b"hello world, no specials here at all!! plus 32 bytes padding x";
        assert_eq!(scan_unescaped_run(s, 0), s.len());
        assert_eq!(scan_unescaped_run(s, s.len()), s.len());
        let t = b"0123456789ABCDEF\\tail"; // backslash lands exactly on a NEON/SWAR chunk edge
        assert_eq!(scan_unescaped_run(t, 0), 16);
        let u = b"\t";
        assert_eq!(scan_unescaped_run(u, 0), 0);
    }

    #[test]
    fn escaped_string_writer_matches_json_semantics() {
        let mut out = Vec::new();
        write_escaped_str(&mut out, "hi\n\t\"x\\y\u{0008}\u{000c}\r");
        assert_eq!(out, b"\"hi\\n\\t\\\"x\\\\y\\b\\f\\r\"");
        out.clear();
        write_escaped_str(&mut out, "ctl\u{0001}\u{001f}end");
        assert_eq!(out, b"\"ctl\\u0001\\u001fend\""); // lowercase hex per ECMA-262
        out.clear();
        write_escaped_str(&mut out, "日本語 🙂");
        assert_eq!(out, format!("\"{}\"", "日本語 🙂").as_bytes());
        out.clear();
        write_escaped_str(&mut out, "");
        assert_eq!(out, b"\"\"");
    }

    #[test]
    fn integer_writer_covers_all_digit_lengths_and_bounds() {
        let cases: Vec<(i64, &str)> = vec![
            (0, "0"),
            (-1, "-1"),
            (9, "9"),
            (10, "10"),
            (99, "99"),
            (100, "100"),
            (i64::MAX, "9223372036854775807"),
            (i64::MIN, "-9223372036854775808"),
            (-100, "-100"),
            (1234567890123, "1234567890123"),
        ];
        for (v, want) in cases {
            let mut out = Vec::new();
            write_i64(&mut out, v);
            assert_eq!(String::from_utf8(out).unwrap(), want, "write_i64({v})");
        }
    }

    #[test]
    fn parse_int_fast_bypasses_float_and_rejects_non_integers() {
        assert_eq!(parse_int_fast(b"-42"), Some(-42));
        assert_eq!(parse_int_fast(b"0"), Some(0));
        assert_eq!(parse_int_fast(b"00"), None); // leading zeros are invalid JSON
        assert_eq!(parse_int_fast(b"3.5"), None);
        assert_eq!(parse_int_fast(b"1e5"), None);
        assert_eq!(parse_int_fast(b"9999999999999999999999"), None); // i64 overflow
        assert_eq!(parse_int_fast(b""), None);
        assert_eq!(parse_int_fast(b"i64min".as_slice()), None);
        assert_eq!(parse_int_fast(b"9223372036854775807"), Some(i64::MAX));
        assert_eq!(parse_int_fast(b"-9223372036854775808"), Some(i64::MIN));
    }

    #[test]
    fn float_formatter_matches_ecma_number_tostring() {
        let cases: Vec<(f64, &str)> = vec![
            (33.5, "33.5"),
            (4166.0, "4166"),
            (-245.0, "-245"),
            (1e5, "100000"),
            (0.1, "0.1"),
            (2.5, "2.5"),
            (1e21, "1e+21"),
            (1.5e20, "150000000000000000000"),
            (1e-7, "1e-7"),
            (1.234e-6, "0.000001234"),
            (0.30000000000000004, "0.30000000000000004"),
            (123456789012345678.0, "123456789012345680"),
            (1.5e-8, "1.5e-8"),
        ];
        for (v, want) in cases {
            assert_eq!(f64_json_string(v), want, "format {v}");
        }
        assert_eq!(f64_json_string(f64::NAN), "null");
        assert_eq!(f64_json_string(f64::INFINITY), "null");
        // Negative zero keeps its sign like ToString.
        let nz = -0.0;
        assert_eq!(f64_json_string(nz), "-0");
    }
}

