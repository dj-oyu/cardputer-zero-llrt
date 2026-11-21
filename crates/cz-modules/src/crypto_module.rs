//! Phase 9A: Node-compatible `node:crypto` / `crypto` backed **exclusively** by `ring 0.17`
//! (already in the workspace `Cargo.lock` via rustls — zero new crates, zero OpenSSL/C TLS
//! libraries anywhere).
//!
//! Architecture mirrors the other `cz-modules` layers: `'static` Rust natives registered as
//! `globalThis.__czCrypto*` plus a JavaScript shim (`ctx.eval`) that wires them into chainable
//! `CzHash`/`CzHmac` objects with Node semantics:
//! * `createHash("sha256" | "sha512")` (case-insensitive; `sha-256`/`sha-512` accepted too)
//!   with `.update(string | Uint8Array | ArrayBuffer)` (chainable) and
//!   `.digest(encoding?)` → hex/base64 string or `Uint8Array` when omitted.
//! * `createHmac(algorithm, key)` with the same chained update/digest surface.
//! * `randomBytes(size)` from `ring::rand::SystemRandom` (`RangeError` for negative / > 65536).
//! * `randomUUID()` RFC 4122 v4 lowercase.
//! * `timingSafeEqual(a, b)` constant-time compare via `ring::constant_time`
//!   (`RangeError` on length mismatch, `TypeError` on non-Uint8Array inputs).
//!
//! Hash/HMAC state lives in a process-wide registry keyed by integer ids, so the natives stay
//! `'static` (the rquickjs closure rule) and no borrowed QuickJS data crosses contexts.

use std::sync::{Mutex, OnceLock};

use rquickjs::function::Opt;
use rquickjs::{Ctx, Exception, Function, Object, Result, Value};

/// Digest algorithm selector (SHA-256 / SHA-512 only, per spec). Shared with the Phase 12
/// `globalThis.crypto` (`crypto.subtle`) layer so both surfaces normalize identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Alg {
    Sha256,
    Sha512,
}

impl Alg {
    pub(crate) fn digest_alg(self) -> &'static ring::digest::Algorithm {
        match self {
            Alg::Sha256 => &ring::digest::SHA256,
            Alg::Sha512 => &ring::digest::SHA512,
        }
    }

    pub(crate) fn hmac_alg(self) -> ring::hmac::Algorithm {
        match self {
            // ring 0.17 `hmac::Key::new` takes Algorithm by value.
            Alg::Sha256 => ring::hmac::HMAC_SHA256,
            Alg::Sha512 => ring::hmac::HMAC_SHA512,
        }
    }
}

/// Normalize a JS algorithm name: case-insensitive, tolerating `sha-256`/`sha_256` spacing.
pub(crate) fn alg_of(name: &str) -> Option<Alg> {
    let norm: String = name
        .to_ascii_lowercase()
        .chars()
        .filter(|c| *c != '-' && *c != '_' && *c != ' ')
        .collect();
    match norm.as_str() {
        "sha256" => Some(Alg::Sha256),
        "sha512" => Some(Alg::Sha512),
        _ => None,
    }
}

enum DigestCtx {
    Hash(ring::digest::Context),
    Hmac(Box<ring::hmac::Context>),
}

/// One incremental hash/HMAC sink across chained `update()` calls (never buffers payload in RAM).
struct Sink {
    ctx: DigestCtx,
    done: bool,
}

fn registry() -> &'static Mutex<Vec<Option<Sink>>> {
    static REG: OnceLock<Mutex<Vec<Option<Sink>>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(Vec::new()))
}

fn sink_index<'js>(ctx: &Ctx<'js>, id: f64, msg: &str) -> Result<usize> {
    if !id.is_finite() || id < 0.0 || id.fract() != 0.0 || id > usize::MAX as f64 {
        return Err(Exception::throw_message(ctx, msg));
    }
    Ok(id as usize)
}

/// `string | Uint8Array | ArrayBuffer` → owned bytes (`bytes_from_value` handles buffer kinds).
fn js_bytes<'js>(v: &Value<'js>) -> Result<Vec<u8>> {
    if let Some(s) = v.as_string() {
        return Ok(s.to_string()?.into_bytes());
    }
    crate::bytes_from_value(v)
}

const HEX_LUT: &[u8; 16] = b"0123456789abcdef";
const B64_STD_TABLE: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const B64_URL_TABLE: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub(crate) fn hex_encode(data: &[u8]) -> String {
    let n = data.len();
    let mut out = Vec::<u8>::with_capacity(n * 2);
    #[allow(unused_mut)]
    let mut i = 0usize;

    #[cfg(target_arch = "aarch64")]
    {
        use core::arch::aarch64::{
            uint8x16x2_t, vandq_u8, vdupq_n_u8, vld1q_u8, vqtbl1q_u8, vshrq_n_u8, vst2q_u8,
        };
        if n >= 16 {
            unsafe {
                let lut = vld1q_u8(HEX_LUT.as_ptr());
                let mask_lo = vdupq_n_u8(0x0F);
                let src = data.as_ptr();
                let dst = out.as_mut_ptr();
                while i + 16 <= n {
                    let v = vld1q_u8(src.add(i));
                    let hi = vqtbl1q_u8(lut, vshrq_n_u8(v, 4));
                    let lo = vqtbl1q_u8(lut, vandq_u8(v, mask_lo));
                    vst2q_u8(dst.add(i * 2), uint8x16x2_t(hi, lo));
                    i += 16;
                }
                out.set_len(i * 2);
            }
        }
    }

    for &b in &data[i..] {
        out.push(HEX_LUT[(b >> 4) as usize]);
        out.push(HEX_LUT[(b & 0x0F) as usize]);
    }
    // SAFETY: every byte comes from HEX_LUT (pure ASCII).
    unsafe { String::from_utf8_unchecked(out) }
}

/// ARMv8.0 NEON + portable Base64 / Base64URL encoder (48B -> 64B via `vld3q_u8` + `vqtbl4q_u8` + `vst4q_u8`).
pub(crate) fn base64_encode_with_options(data: &[u8], url_safe: bool, pad: bool) -> String {
    let table = if url_safe { B64_URL_TABLE } else { B64_STD_TABLE };
    let n = data.len();
    let cap = if pad {
        ((n + 2) / 3) * 4
    } else {
        (n / 3) * 4 + match n % 3 { 0 => 0, 1 => 2, _ => 3 }
    };
    let mut out = Vec::<u8>::with_capacity(cap);
    #[allow(unused_mut)]
    let mut i = 0usize;

    #[cfg(target_arch = "aarch64")]
    {
        use core::arch::aarch64::{
            uint8x16x4_t, vandq_u8, vdupq_n_u8, vld1q_u8, vld3q_u8, vorrq_u8, vqtbl4q_u8,
            vshlq_n_u8, vshrq_n_u8, vst4q_u8,
        };
        if n >= 48 {
            unsafe {
                let mut o = 0usize;
                let lut = uint8x16x4_t(
                    vld1q_u8(table.as_ptr()),
                    vld1q_u8(table.as_ptr().add(16)),
                    vld1q_u8(table.as_ptr().add(32)),
                    vld1q_u8(table.as_ptr().add(48)),
                );
                let mask6 = vdupq_n_u8(0x3F);
                let src = data.as_ptr();
                let dst = out.as_mut_ptr();
                while i + 48 <= n {
                    let raw = vld3q_u8(src.add(i));
                    let idx0 = vshrq_n_u8(raw.0, 2);
                    let idx1 = vandq_u8(vorrq_u8(vshlq_n_u8(raw.0, 4), vshrq_n_u8(raw.1, 4)), mask6);
                    let idx2 = vandq_u8(vorrq_u8(vshlq_n_u8(raw.1, 2), vshrq_n_u8(raw.2, 6)), mask6);
                    let idx3 = vandq_u8(raw.2, mask6);
                    let c0 = vqtbl4q_u8(lut, idx0);
                    let c1 = vqtbl4q_u8(lut, idx1);
                    let c2 = vqtbl4q_u8(lut, idx2);
                    let c3 = vqtbl4q_u8(lut, idx3);
                    vst4q_u8(dst.add(o), uint8x16x4_t(c0, c1, c2, c3));
                    i += 48;
                    o += 64;
                }
                out.set_len(o);
            }
        }
    }

    for chunk in data[i..].chunks(3) {
        let b0 = chunk[0];
        let b1 = if chunk.len() > 1 { chunk[1] } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] } else { 0 };
        let v = ((b0 as u32) << 16) | ((b1 as u32) << 8) | b2 as u32;
        out.push(table[((v >> 18) & 63) as usize]);
        out.push(table[((v >> 12) & 63) as usize]);
        if chunk.len() > 1 {
            out.push(table[((v >> 6) & 63) as usize]);
        } else if pad {
            out.push(b'=');
        }
        if chunk.len() > 2 {
            out.push(table[(v & 63) as usize]);
        } else if pad {
            out.push(b'=');
        }
    }
    // SAFETY: every byte comes from table or '=' (pure ASCII).
    unsafe { String::from_utf8_unchecked(out) }
}

/// Minimal standard-alphabet base64 (no new crates; ring keeps its Base64 Ring-internal only).
pub(crate) fn base64_encode(data: &[u8]) -> String {
    base64_encode_with_options(data, false, true)
}

/// Pure-Rust SHA-256 hex (used by unit tests + available for embedders).
pub fn sha256_hex(data: &[u8]) -> String {
    hex_encode(ring::digest::digest(&ring::digest::SHA256, data).as_ref())
}

/// Pure-Rust HMAC-SHA256 bytes over `key`/`data` (SigV4 signing-key chains land here from JS).
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key), data).as_ref().to_vec()
}

// ---------------------------------------------------------------------------
// Rust natives (registered as globalThis.__czCrypto*)
// ---------------------------------------------------------------------------

fn new_impl<'js>(ctx: &Ctx<'js>, alg: String, key: Opt<Value<'js>>) -> Result<f64> {
    let alg = alg_of(&alg).ok_or_else(|| {
        Exception::throw_message(ctx, &format!("Unsupported digest algorithm: {alg}"))
    })?;
    let digest_ctx = match key.0 {
        Some(v) if !v.is_undefined() && !v.is_null() => {
            let kb = js_bytes(&v)?;
            let ring_key = ring::hmac::Key::new(alg.hmac_alg(), &kb);
            DigestCtx::Hmac(Box::new(ring::hmac::Context::with_key(&ring_key)))
        }
        _ => DigestCtx::Hash(ring::digest::Context::new(alg.digest_alg())),
    };
    let mut reg = registry().lock().expect("crypto registry lock");
    let id = reg.len() as f64;
    reg.push(Some(Sink { ctx: digest_ctx, done: false }));
    Ok(id)
}

fn update_impl<'js>(ctx: &Ctx<'js>, id: f64, data: Value<'js>) -> Result<()> {
    let idx = sink_index(ctx, id, "invalid crypto handle")?;
    let bytes = js_bytes(&data)?;
    let mut reg = registry().lock().expect("crypto registry lock");
    let slot = reg
        .get_mut(idx)
        .and_then(|s| s.as_mut())
        .ok_or_else(|| Exception::throw_message(ctx, "invalid crypto handle"))?;
    if slot.done {
        return Err(Exception::throw_message(ctx, "digest already computed for this hash"));
    }
    match &mut slot.ctx {
        DigestCtx::Hash(c) => c.update(&bytes),
        DigestCtx::Hmac(c) => c.update(&bytes),
    }
    Ok(())
}

fn final_impl<'js>(ctx: &Ctx<'js>, id: f64, encoding: Opt<String>) -> Result<Value<'js>> {
    let idx = sink_index(ctx, id, "invalid crypto handle")?;
    let enc = encoding.0.unwrap_or_default();
    if !matches!(enc.as_str(), "" | "hex" | "base64") {
        return Err(Exception::throw_message(
            ctx,
            &format!("Unsupported encoding: {enc} (expected hex or base64)"),
        ));
    }
    let mut reg = registry().lock().expect("crypto registry lock");
    let slot = reg
        .get_mut(idx)
        .and_then(|s| s.take())
        .ok_or_else(|| Exception::throw_message(ctx, "digest already computed for this hash"))?;
    let out = match slot.ctx {
        DigestCtx::Hash(c) => c.finish().as_ref().to_vec(),
        DigestCtx::Hmac(c) => c.sign().as_ref().to_vec(),
    };
    if enc == "hex" {
        Ok(rquickjs::String::from_str(ctx.clone(), &hex_encode(&out))?.into_value())
    } else if enc == "base64" {
        Ok(rquickjs::String::from_str(ctx.clone(), &base64_encode(&out))?.into_value())
    } else {
        crate::u8_array_from_vec(ctx, out)
    }
}

static RNG: OnceLock<ring::rand::SystemRandom> = OnceLock::new();

/// Process-wide CSPRNG shared by `node:crypto`, Web Crypto fill/UUID paths.
pub(crate) fn system_rng() -> &'static ring::rand::SystemRandom {
    RNG.get_or_init(|| ring::rand::SystemRandom::new())
}

/// Decode standard (`+`/`/`) or URL-safe (`-`/`_`) Base64 in ONE native pass. HTML-style
/// semantics: whitespace is skipped and everything from the first `=` onward terminates the
/// stream, so unpadded JWK (`k`) values decode cleanly; a lone dangling sextet yields no byte.
/// `None` marks an invalid character (HTML `InvalidCharacterError` / JWK parse failure).
///
/// On `aarch64`, clean 64-char blocks take an ARMv8.0 baseline NEON fast path (`vld4q_u8` ->
/// disjoint range decode + single `vminvq_u8` validation -> `vst3q_u8`, 64B -> 48B) and break
/// cleanly to the exact scalar state machine on any whitespace, `'='`, or invalid byte.
pub(crate) fn base64_decode(text: &str, url_safe: bool) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    let n = bytes.len();
    let mut out = Vec::<u8>::with_capacity((n / 4) * 3 + 3);
    #[allow(unused_mut)]
    let mut i = 0usize;

    #[cfg(target_arch = "aarch64")]
    {
        use core::arch::aarch64::{
            uint8x16_t, uint8x16x3_t, vaddq_u8, vandq_u8, vceqq_u8, vcgeq_u8, vcleq_u8,
            vdupq_n_u8, vld4q_u8, vminvq_u8, vorrq_u8, vshlq_n_u8, vshrq_n_u8, vst3q_u8,
        };
        if n >= 64 {
            unsafe {
                let c_cap_a = vdupq_n_u8(b'A');
                let c_cap_z = vdupq_n_u8(b'Z');
                let c_low_a = vdupq_n_u8(b'a');
                let c_low_z = vdupq_n_u8(b'z');
                let c_dig_0 = vdupq_n_u8(b'0');
                let c_dig_9 = vdupq_n_u8(b'9');
                let c_62 = vdupq_n_u8(if url_safe { b'-' } else { b'+' });
                let c_63 = vdupq_n_u8(if url_safe { b'_' } else { b'/' });

                // Wrapping add offsets (proven exact for each disjoint ASCII interval):
                // 'A'..'Z' (-65 = 191), 'a'..'z' (-71 = 185), '0'..'9' (+4),
                // '+' (+19) / '/' (+16), '-' (+17) / '_' (-32 = 224).
                let off_upper = vdupq_n_u8(191);
                let off_lower = vdupq_n_u8(185);
                let off_digit = vdupq_n_u8(4);
                let off_62 = vdupq_n_u8(if url_safe { 17 } else { 19 });
                let off_63 = vdupq_n_u8(if url_safe { 224 } else { 16 });

                #[inline(always)]
                unsafe fn decode_vec(
                    v: uint8x16_t,
                    c_cap_a: uint8x16_t,
                    c_cap_z: uint8x16_t,
                    c_low_a: uint8x16_t,
                    c_low_z: uint8x16_t,
                    c_dig_0: uint8x16_t,
                    c_dig_9: uint8x16_t,
                    c_62: uint8x16_t,
                    c_63: uint8x16_t,
                    off_upper: uint8x16_t,
                    off_lower: uint8x16_t,
                    off_digit: uint8x16_t,
                    off_62: uint8x16_t,
                    off_63: uint8x16_t,
                ) -> (uint8x16_t, uint8x16_t) {
                    let m_up = vandq_u8(vcgeq_u8(v, c_cap_a), vcleq_u8(v, c_cap_z));
                    let m_lo = vandq_u8(vcgeq_u8(v, c_low_a), vcleq_u8(v, c_low_z));
                    let m_dg = vandq_u8(vcgeq_u8(v, c_dig_0), vcleq_u8(v, c_dig_9));
                    let m_62 = vceqq_u8(v, c_62);
                    let m_63 = vceqq_u8(v, c_63);
                    let valid = vorrq_u8(
                        vorrq_u8(vorrq_u8(m_up, m_lo), vorrq_u8(m_dg, m_62)),
                        m_63,
                    );
                    let off = vorrq_u8(
                        vorrq_u8(vandq_u8(m_up, off_upper), vandq_u8(m_lo, off_lower)),
                        vorrq_u8(
                            vandq_u8(m_dg, off_digit),
                            vorrq_u8(vandq_u8(m_62, off_62), vandq_u8(m_63, off_63)),
                        ),
                    );
                    (vaddq_u8(v, off), valid)
                }

                let src = bytes.as_ptr();
                let dst = out.as_mut_ptr();
                let mut o = 0usize;
                while i + 64 <= n {
                    let raw = vld4q_u8(src.add(i));
                    let (s0, ok0) = decode_vec(
                        raw.0, c_cap_a, c_cap_z, c_low_a, c_low_z, c_dig_0, c_dig_9, c_62, c_63,
                        off_upper, off_lower, off_digit, off_62, off_63,
                    );
                    let (s1, ok1) = decode_vec(
                        raw.1, c_cap_a, c_cap_z, c_low_a, c_low_z, c_dig_0, c_dig_9, c_62, c_63,
                        off_upper, off_lower, off_digit, off_62, off_63,
                    );
                    let (s2, ok2) = decode_vec(
                        raw.2, c_cap_a, c_cap_z, c_low_a, c_low_z, c_dig_0, c_dig_9, c_62, c_63,
                        off_upper, off_lower, off_digit, off_62, off_63,
                    );
                    let (s3, ok3) = decode_vec(
                        raw.3, c_cap_a, c_cap_z, c_low_a, c_low_z, c_dig_0, c_dig_9, c_62, c_63,
                        off_upper, off_lower, off_digit, off_62, off_63,
                    );
                    let all_ok = vandq_u8(vandq_u8(ok0, ok1), vandq_u8(ok2, ok3));
                    if vminvq_u8(all_ok) == 0 {
                        break;
                    }
                    let b0 = vorrq_u8(vshlq_n_u8(s0, 2), vshrq_n_u8(s1, 4));
                    let b1 = vorrq_u8(vshlq_n_u8(s1, 4), vshrq_n_u8(s2, 2));
                    let b2 = vorrq_u8(vshlq_n_u8(s2, 6), s3);
                    vst3q_u8(dst.add(o), uint8x16x3_t(b0, b1, b2));
                    i += 64;
                    o += 48;
                }
                out.set_len(o);
            }
        }
    }

    let sextet = |b: u8| -> Option<u32> {
        match b {
            b'A'..=b'Z' => Some((b - b'A') as u32),
            b'a'..=b'z' => Some((b - b'a') as u32 + 26),
            b'0'..=b'9' => Some((b - b'0') as u32 + 52),
            b'+' if !url_safe => Some(62),
            b'/' if !url_safe => Some(63),
            b'-' if url_safe => Some(62),
            b'_' if url_safe => Some(63),
            _ => None,
        }
    };
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for &b in &bytes[i..] {
        if b == b'=' {
            break; // padding terminates the stream (trailing junk is ignored, per HTML)
        }
        if matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0c) {
            continue; // WHATWG/HTML whitespace filtering
        }
        let v = sextet(b)?;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    // A single leftover sextet (len % 4 == 1) cannot produce a byte: invalid input.
    if bits == 6 {
        return None;
    }
    Some(out)
}

fn random_bytes_impl<'js>(ctx: &Ctx<'js>, size: f64) -> Result<Value<'js>> {
    // Defensive double-check (the shim raises the spec RangeErrors first):
    if !size.is_finite() || size < 0.0 || size > 65536.0 || size != size.round() {
        return Err(Exception::throw_message(
            ctx,
            "randomBytes size must be an integer in 0..=65536",
        ));
    }
    let n = size as usize;
    let mut buf = vec![0u8; n];
    let rng = RNG.get_or_init(|| ring::rand::SystemRandom::new());
    use ring::rand::SecureRandom;
    rng.fill(&mut buf[..])
        .map_err(|e| Exception::throw_message(ctx, &format!("system randomness failed: {e:?}")))?;
    crate::u8_array_from_vec(ctx, buf)
}

/// RFC 4122 v4 lowercase UUID from `ring::rand` (reused by `globalThis.crypto.randomUUID`).
pub(crate) fn random_uuid_impl() -> rquickjs::Result<String> {
    let mut b = [0u8; 16];
    let rng = RNG.get_or_init(|| ring::rand::SystemRandom::new());
    use ring::rand::SecureRandom;
    rng.fill(&mut b[..]).map_err(|e| {
        rquickjs::Error::Io(std::io::Error::other(format!("system randomness failed: {e:?}")))
    })?;
    // RFC 4122 v4 bits: version 4 in the high nibble of byte 6, variant 10xx in byte 8.
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    Ok(format!(
        "{}-{}-{}-{}-{}",
        hex_encode(&b[0..4]),
        hex_encode(&b[4..6]),
        hex_encode(&b[6..8]),
        hex_encode(&b[8..10]),
        hex_encode(&b[10..16])
    ))
}

fn timing_safe_equal_impl<'js>(ctx: &Ctx<'js>, a: Value<'js>, b: Value<'js>) -> Result<bool> {
    let av = js_bytes(&a)?;
    let bv = js_bytes(&b)?;
    if av.len() != bv.len() {
        return Err(Exception::throw_message(
            ctx,
            "timingSafeEqual requires equal-length buffers",
        ));
    }
    // The spec mandates ring's constant-time comparator; ring keeps this entry point
    // public-but-deprecated, hence the narrow allow gate below.
    #[allow(deprecated)]
    Ok(ring::constant_time::verify_slices_are_equal(&av, &bv).is_ok())
}

// ---------------------------------------------------------------------------
// JS shim (classes + argument validation with proper Node error kinds)
// ---------------------------------------------------------------------------

const CRYPTO_SHIM: &str = r#"
(function () {
    if (globalThis.__czCryptoInstalledV9A) return;

    function normAlg(alg) {
        if (typeof alg !== "string") throw new TypeError("algorithm must be a string");
        const a = alg.toLowerCase().replace(/[-_ ]/g, "");
        if (a === "sha256" || a === "sha512") return a;
        throw new TypeError("Unsupported digest algorithm: " + alg);
    }

    function checkData(d, what) {
        if (typeof d === "string" || d instanceof Uint8Array || d instanceof ArrayBuffer) return d;
        throw new TypeError(what + " must be a string, Uint8Array or ArrayBuffer");
    }

    class CzHash {
        constructor(id) { this._id = id; this._done = false; }
        update(data) {
            if (this._done) throw new Error("digest already computed for this hash");
            __czCryptoUpdate(this._id, checkData(data, "update data"));
            return this;
        }
        digest(encoding) {
            if (this._done) throw new Error("digest already computed for this hash");
            if (encoding === undefined || encoding === null) return __czCryptoFinal(this._id, "");
            if (typeof encoding !== "string") throw new TypeError("encoding must be a string");
            const enc = encoding.toLowerCase();
            if (enc !== "hex" && enc !== "base64") {
                throw new RangeError("Unsupported encoding: " + encoding);
            }
            return __czCryptoFinal(this._id, enc);
        }
    }

    globalThis.__czCreateHash = function (algorithm) {
        return new CzHash(__czCryptoNew(normAlg(algorithm), undefined));
    };
    globalThis.__czCreateHmac = function (algorithm, key) {
        return new CzHash(__czCryptoNew(normAlg(algorithm), checkData(key, "key")));
    };
    globalThis.__czRandomBytes = function (size) {
        const n = Number(size);
        if (!Number.isInteger(n)) throw new RangeError("randomBytes size must be an integer");
        if (n < 0) throw new RangeError("randomBytes size must not be negative");
        if (n > 65536) throw new RangeError("randomBytes size exceeds the 65536 byte limit");
        return __czCryptoRandomBytes(n);
    };
    globalThis.__czTimingSafeEqual = function (a, b) {
        if (!(a instanceof Uint8Array) || !(b instanceof Uint8Array)) {
            throw new TypeError("timingSafeEqual requires Uint8Array arguments");
        }
        if (a.byteLength !== b.byteLength) {
            throw new RangeError("timingSafeEqual: buffers differ in length");
        }
        return __czCryptoTimingSafeEqual(a, b);
    };

    globalThis.__czCryptoInstalledV9A = true;
})();
"#;

/// Register natives + shim on `globalThis` (idempotent). Called by [`CryptoModule`] evaluate.
pub fn ensure_crypto_glue<'js>(ctx: &Ctx<'js>) -> Result<()> {
    ctx.globals().set(
        "__czCryptoNew",
        Function::new(ctx.clone(), |ctx: Ctx<'js>, alg: String, key: Opt<Value<'js>>| -> Result<f64> {
            new_impl(&ctx, alg, key)
        })?,
    )?;
    ctx.globals().set(
        "__czCryptoUpdate",
        Function::new(ctx.clone(), |ctx: Ctx<'js>, id: f64, data: Value<'js>| -> Result<()> {
            update_impl(&ctx, id, data)
        })?,
    )?;
    ctx.globals().set(
        "__czCryptoFinal",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, id: f64, encoding: Opt<String>| -> Result<Value<'js>> {
                final_impl(&ctx, id, encoding)
            },
        )?,
    )?;
    ctx.globals().set(
        "__czCryptoRandomBytes",
        Function::new(ctx.clone(), |ctx: Ctx<'js>, size: f64| -> Result<Value<'js>> {
            random_bytes_impl(&ctx, size)
        })?,
    )?;
    ctx.globals().set(
        "__czCryptoRandomUUID",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>| -> Result<Value<'js>> {
                let text = uuid_or_throw(&ctx)?;
                Ok(rquickjs::String::from_str(ctx, &text)?.into_value())
            },
        )?,
    )?;
    ctx.globals().set(
        "__czCryptoTimingSafeEqual",
        Function::new(ctx.clone(), |ctx: Ctx<'js>, a: Value<'js>, b: Value<'js>| -> Result<bool> {
            timing_safe_equal_impl(&ctx, a, b)
        })?,
    )?;
    ctx.eval::<(), _>(CRYPTO_SHIM)?;
    Ok(())
}

/// `node:crypto` / `crypto` module definition (named exports AND a `default` export object).
pub struct CryptoModule;

const EXPORT_NAMES: &[&str] = &[
    "createHash",
    "createHmac",
    "randomBytes",
    "randomUUID",
    "timingSafeEqual",
];

impl rquickjs::module::ModuleDef for CryptoModule {
    fn declare<'js>(decl: &rquickjs::module::Declarations<'js>) -> Result<()> {
        for name in EXPORT_NAMES {
            decl.declare(*name)?;
        }
        decl.declare("default")?;
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &rquickjs::module::Exports<'js>) -> Result<()> {
        ensure_crypto_glue(ctx)?;
        // randomUUID is the only pure Rust native exported directly (no shim wrapper needed).
        let uuid_fn = ctx.globals().get::<_, Function>("__czCryptoRandomUUID")?;

        let values: Vec<(&'static str, Value<'js>)> = vec![
            ("createHash", ctx.globals().get::<_, Function>("__czCreateHash")?.into_value()),
            ("createHmac", ctx.globals().get::<_, Function>("__czCreateHmac")?.into_value()),
            ("randomBytes", ctx.globals().get::<_, Function>("__czRandomBytes")?.into_value()),
            ("randomUUID", uuid_fn.into_value()),
            ("timingSafeEqual", ctx.globals().get::<_, Function>("__czTimingSafeEqual")?.into_value()),
        ];
        for (name, value) in &values {
            exports.export(*name, value.clone())?;
        }
        let default_obj = Object::new(ctx.clone())?;
        for (name, value) in &values {
            default_obj.set(*name, value.clone())?;
        }
        exports.export("default", default_obj)?;
        Ok(())
    }
}

// UUID native wrapper helper: converts the pure impl for JS export.
fn uuid_or_throw<'js>(ctx: &Ctx<'js>) -> Result<String> {
    random_uuid_impl().map_err(|_| Exception::throw_message(ctx, "system randomness failed"))
}

#[cfg(test)]
mod crypto_tests {
    use super::*;
    use rquickjs::{Context, Module, Runtime};

    const SHA256_ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    const SHA256_EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const SHA512_ABC: &str = concat!(
        "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a",
        "2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
    );
    // RFC 4231 test case 2: key = "Jefe", data = "what do ya want for nothing?"
    const HMAC_SHA256_JEFE: &str = "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843";
    const HMAC_SHA512_JEFE: &str = concat!(
        "164b7a7bfcf819e2e395fbe73b56e0a387bd64222e831fd610270cd7ea250554",
        "9758bf75c05a994a6d034f65f8f0e6fdcaeab1a34d4a6b4b636e070a38bce737"
    );

    fn run_crypto_script(source: &str) -> String {
        use rquickjs::CatchResultExt;
        let rt = Runtime::new().unwrap();
        let ctx = Context::full(&rt).unwrap();
        let out = ctx.with(|ctx| -> String {
            Module::declare_def::<CryptoModule, _>(ctx.clone(), "node:crypto").unwrap();
            Module::declare_def::<CryptoModule, _>(ctx.clone(), "crypto").unwrap();
            let p = Module::evaluate(ctx.clone(), "crypto-test.mjs", source)
                .catch(&ctx)
                .expect("module eval must succeed");
            p.finish::<()>().catch(&ctx).expect("script must not throw");
            ctx.globals().get::<_, String>("__R").unwrap_or_else(|_| "NO_RESULT".into())
        });
        while let Ok(true) = rt.execute_pending_job() {}
        out
    }

    #[test]
    fn sha256_known_vectors_and_hmac_rfc4231() {
        // Pure Rust layer against canonical vectors (Node-generated expectations, verified above).
        assert_eq!(sha256_hex(b""), SHA256_EMPTY);
        assert_eq!(sha256_hex(b"abc"), SHA256_ABC);
        assert_eq!(
            hex_encode(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            HMAC_SHA256_JEFE
        );
        // ring HMAC-SHA512 sanity against the same RFC 4231 case 2 vector.
        let tag = ring::hmac::sign(
            &ring::hmac::Key::new(ring::hmac::HMAC_SHA512, b"Jefe"),
            b"what do ya want for nothing?",
        );
        assert_eq!(hex_encode(tag.as_ref()), HMAC_SHA512_JEFE);
        // FIPS 180-4 SHA-512("abc") through the pure digest path.
        assert_eq!(
            hex_encode(ring::digest::digest(&ring::digest::SHA512, b"abc").as_ref()),
            SHA512_ABC
        );
    }

    #[test]
    fn hash_js_createhash_chain_hex_and_bytes_digests() {
        let r = run_crypto_script(
            r#"
            import crypto from "node:crypto";
            const h = crypto.createHash("sha256").update("a").update("bc").digest("hex");
            if (h !== "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad") throw new Error("chain " + h);
            const upper = crypto.createHash("SHA-256").update("abc").digest("hex");
            if (upper === h ? false : true) throw new Error("case-insensitive algorithm mismatch");
            const bytes = crypto.createHash("sha512").update("abc").digest();
            if (!(bytes instanceof Uint8Array) || bytes.length !== 64) throw new Error("bytes shape");
            const hexOfBytes = Array.from(bytes).map(b => b.toString(16).padStart(2, "0")).join("");
            if (hexOfBytes !== "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f") {
                throw new Error("sha512 vector " + hexOfBytes);
            }
            const ab = new ArrayBuffer(3);
            new Uint8Array(ab).set([97, 98, 99]);
            if (crypto.createHash("sha256").update(ab).digest("hex") !== h) throw new Error("ArrayBuffer input");
            globalThis.__R = "CRYPTO_HASH_OK";
        "#,
        );
        assert_eq!(r, "CRYPTO_HASH_OK");
    }

    #[test]
    fn hmac_js_base64_and_bytes_with_rfc_vectors() {
        let r = run_crypto_script(
            r#"
            import crypto from "node:crypto";
            const mac = crypto.createHmac("sha256", "Jefe").update("what do ya want for nothing?").digest("hex");
            if (mac !== "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843") throw new Error("hmac " + mac);
            const b64 = crypto.createHmac("sha256", "Jefe").update("what do ya want for nothing?").digest("base64");
            if (b64.length !== 44 || !b64.endsWith("=")) throw new Error("base64 shape " + b64);
            const m512 = crypto.createHmac("SHA-512", new Uint8Array([74, 101, 102, 101])).update("what do ya want for nothing?").digest("hex");
            if (m512 !== "164b7a7bfcf819e2e395fbe73b56e0a387bd64222e831fd610270cd7ea2505549758bf75c05a994a6d034f65f8f0e6fdcaeab1a34d4a6b4b636e070a38bce737") throw new Error("hmac512 " + m512);
            globalThis.__R = "CRYPTO_HMAC_OK";
        "#,
        );
        assert_eq!(r, "CRYPTO_HMAC_OK");
    }

    #[test]
    fn random_uuid_shape_and_uniqueness() {
        let r = run_crypto_script(
            r#"
            import * as c from "node:crypto";
            const u1 = c.randomUUID(), u2 = c.randomUUID();
            if (!/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(u1)) throw new Error("uuid shape " + u1);
            if (u1 === u2) throw new Error("uuid collision");
            globalThis.__R = "UUID_OK";
        "#,
        );
        assert_eq!(r, "UUID_OK");
        // Rust-level shape too.
        let a = random_uuid_impl().unwrap();
        let b = random_uuid_impl().unwrap();
        assert_ne!(a, b);
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
        assert!(parts[2].starts_with('4'), "version nibble");
        assert!("89ab".contains(parts[3].chars().next().unwrap()), "variant nibble");
    }

    #[test]
    fn random_bytes_sizes_and_range_errors() {
        let r = run_crypto_script(
            r#"
            import crypto from "node:crypto";
            const rb = crypto.randomBytes(16);
            if (!(rb instanceof Uint8Array) || rb.length !== 16) throw new Error("len");
            if (crypto.randomBytes(0).length !== 0) throw new Error("zero size must work");
            const big = crypto.randomBytes(65536);
            if (big.length !== 65536) throw new Error("max size");
            let neg = null, over = null;
            try { crypto.randomBytes(-1); } catch (e) { neg = e.constructor.name; }
            try { crypto.randomBytes(65537); } catch (e) { over = e.constructor.name; }
            if (neg !== "RangeError" || over !== "RangeError") throw new Error("range kinds " + neg + "/" + over);
            const uniq = crypto.randomBytes(8); const uniq2 = crypto.randomBytes(8);
            if (JSON.stringify(Array.from(uniq)) === JSON.stringify(Array.from(uniq2))) throw new Error("rng repeats");
            globalThis.__R = "RANDOM_OK";
        "#,
        );
        assert_eq!(r, "RANDOM_OK");
    }

    #[test]
    fn timing_safe_equal_semantics() {
        let r = run_crypto_script(
            r#"
            import { timingSafeEqual } from "crypto";
            if (!timingSafeEqual(new Uint8Array([1, 2, 3]), new Uint8Array([1, 2, 3]))) throw new Error("equal must be true");
            if (timingSafeEqual(new Uint8Array([1, 2, 3]), new Uint8Array([1, 2, 4]))) throw new Error("unequal must be false");
            let lenErr = null, typeErr = null;
            try { timingSafeEqual(new Uint8Array([1]), new Uint8Array([1, 2])); } catch (e) { lenErr = e.constructor.name; }
            try { timingSafeEqual("ab", "ab"); } catch (e) { typeErr = e.constructor.name; }
            if (lenErr !== "RangeError" || typeErr !== "TypeError") throw new Error("kinds " + lenErr + "/" + typeErr);
            globalThis.__R = "TSE_OK";
        "#,
        );
        assert_eq!(r, "TSE_OK");
    }

    #[test]
    fn algorithm_and_digest_guard_errors() {
        let r = run_crypto_script(
            r#"
            import crypto from "node:crypto";
            let algErr = null, encErr = null, dblErr = null;
            try { crypto.createHash("md5"); } catch (e) { algErr = e.constructor.name; }
            try { crypto.createHash("sha256").update("x").digest("base32"); } catch (e) { encErr = e.constructor.name; }
            const h = crypto.createHash("sha256"); h.update("x"); h.digest("hex");
            try { h.update("y"); } catch (e) { dblErr = "update:" + e.constructor.name; }
            try { h.digest("hex"); } catch (e) { dblErr += " digest:" + e.constructor.name; }
            if (algErr !== "TypeError" || encErr !== "RangeError") throw new Error("kinds " + algErr + "/" + encErr);
            if (dblErr !== "update:Error digest:Error") throw new Error("double use " + dblErr);
            globalThis.__R = "GUARDS_OK";
        "#,
        );
        assert_eq!(r, "GUARDS_OK");
    }

    #[test]
    fn base64_and_hex_helpers_known_values() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"Hello World"), "SGVsbG8gV29ybGQ=");
        assert_eq!(base64_encode(b"abc"), "YWJj");
        assert_eq!(base64_encode(&[0u8, 255, 128]), "AP+A"); // Node Buffer ground truth
        assert_eq!(hex_encode(&[0x00, 0x0f, 0xff]), "000fff");
    }

    #[test]
    fn crypto_default_export_shape() {
        let r = run_crypto_script(
            r#"
            import def from "node:crypto";
            import * as ns from "crypto";
            for (const n of ["createHash", "createHmac", "randomBytes", "randomUUID", "timingSafeEqual"]) {
                if (typeof def[n] !== "function") throw new Error("default missing " + n);
                if (typeof ns[n] !== "function") throw new Error("named missing " + n);
            }
            globalThis.__R = "CRYPTO_DEFAULTS_OK";
        "#,
        );
        assert_eq!(r, "CRYPTO_DEFAULTS_OK");
    }

    #[test]
    fn nan_and_negative_crypto_handles_do_not_alias_slot_zero() {
        let r = run_crypto_script(
            r#"
            import crypto from "node:crypto";
            const h0 = crypto.createHash("sha256").update("a");
            for (const bad of [-1, NaN, Infinity, 0.5]) {
                let updErr = false;
                try { __czCryptoUpdate(bad, "evil"); } catch (_e) { updErr = true; }
                if (!updErr) throw new Error("bad update id must fail: " + bad);
                let finErr = false;
                try { __czCryptoFinal(bad, "hex"); } catch (_e) { finErr = true; }
                if (!finErr) throw new Error("bad final id must fail: " + bad);
            }
            if (h0.update("bc").digest("hex") !== "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad") {
                throw new Error("live handle 0 was corrupted by bad ids");
            }
            globalThis.__R = "CRYPTO_HANDLE_GUARD_OK";
        "#,
        );
        assert_eq!(r, "CRYPTO_HANDLE_GUARD_OK");
    }
}
