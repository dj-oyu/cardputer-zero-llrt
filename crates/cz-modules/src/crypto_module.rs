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

/// Digest algorithm selector (SHA-256 / SHA-512 only, per spec).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Alg {
    Sha256,
    Sha512,
}

impl Alg {
    fn digest_alg(self) -> &'static ring::digest::Algorithm {
        match self {
            Alg::Sha256 => &ring::digest::SHA256,
            Alg::Sha512 => &ring::digest::SHA512,
        }
    }

    fn hmac_alg(self) -> ring::hmac::Algorithm {
        match self {
            // ring 0.17 `hmac::Key::new` takes Algorithm by value.
            Alg::Sha256 => ring::hmac::HMAC_SHA256,
            Alg::Sha512 => ring::hmac::HMAC_SHA512,
        }
    }
}

/// Normalize a JS algorithm name: case-insensitive, tolerating `sha-256`/`sha_256` spacing.
fn alg_of(name: &str) -> Option<Alg> {
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

/// One hash/HMAC sink accumulated across chained `update()` calls (single-shot finalize).
struct Sink {
    alg: Alg,
    /// `Some(key)` turns the sink into an HMAC; `None` is a plain hash.
    key: Option<Vec<u8>>,
    data: Vec<u8>,
    done: bool,
}

fn registry() -> &'static Mutex<Vec<Option<Sink>>> {
    static REG: OnceLock<Mutex<Vec<Option<Sink>>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(Vec::new()))
}

/// `string | Uint8Array | ArrayBuffer` → owned bytes (`bytes_from_value` handles buffer kinds).
fn js_bytes<'js>(v: &Value<'js>) -> Result<Vec<u8>> {
    if let Some(s) = v.as_string() {
        return Ok(s.to_string()?.into_bytes());
    }
    crate::bytes_from_value(v)
}

fn hex_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2);
    for b in data {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Minimal standard-alphabet base64 (no new crates; ring keeps its Base64 Ring-internal only).
fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = if chunk.len() > 1 { chunk[1] } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] } else { 0 };
        let v = ((b0 as u32) << 16) | ((b1 as u32) << 8) | b2 as u32;
        out.push(TABLE[((v >> 18) & 63) as usize] as char);
        out.push(TABLE[((v >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 { TABLE[((v >> 6) & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[(v & 63) as usize] as char } else { '=' });
    }
    out
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

fn new_impl<'js>(ctx: &Ctx<'js>, alg: String, key: Opt<Value<'js>>) -> Result<i64> {
    let alg = alg_of(&alg).ok_or_else(|| {
        Exception::throw_message(ctx, &format!("Unsupported digest algorithm: {alg}"))
    })?;
    let key = match key.0 {
        Some(v) if !v.is_undefined() && !v.is_null() => Some(js_bytes(&v)?),
        _ => None,
    };
    let mut reg = registry().lock().expect("crypto registry lock");
    let id = reg.len() as i64;
    reg.push(Some(Sink { alg, key, data: Vec::new(), done: false }));
    Ok(id)
}

fn update_impl<'js>(ctx: &Ctx<'js>, id: i64, data: Value<'js>) -> Result<()> {
    let bytes = js_bytes(&data)?;
    let mut reg = registry().lock().expect("crypto registry lock");
    let slot = reg
        .get_mut(id.max(0) as usize)
        .and_then(|s| s.as_mut())
        .ok_or_else(|| Exception::throw_message(ctx, "invalid crypto handle"))?;
    if slot.done {
        return Err(Exception::throw_message(ctx, "digest already computed for this hash"));
    }
    slot.data.extend_from_slice(&bytes);
    Ok(())
}

fn final_impl<'js>(ctx: &Ctx<'js>, id: i64, encoding: Opt<String>) -> Result<Value<'js>> {
    let enc = encoding.0.unwrap_or_default();
    if !matches!(enc.as_str(), "" | "hex" | "base64") {
        return Err(Exception::throw_message(
            ctx,
            &format!("Unsupported encoding: {enc} (expected hex or base64)"),
        ));
    }
    let mut reg = registry().lock().expect("crypto registry lock");
    let slot = reg
        .get_mut(id.max(0) as usize)
        .and_then(|s| s.take())
        .ok_or_else(|| Exception::throw_message(ctx, "digest already computed for this hash"))?;
    let out = match slot.key {
        None => ring::digest::digest(slot.alg.digest_alg(), &slot.data).as_ref().to_vec(),
        Some(key) => {
            let tag = ring::hmac::sign(&ring::hmac::Key::new(slot.alg.hmac_alg(), &key), &slot.data);
            tag.as_ref().to_vec()
        }
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

fn random_uuid_impl() -> rquickjs::Result<String> {
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
        Function::new(ctx.clone(), |ctx: Ctx<'js>, alg: String, key: Opt<Value<'js>>| -> Result<i64> {
            new_impl(&ctx, alg, key)
        })?,
    )?;
    ctx.globals().set(
        "__czCryptoUpdate",
        Function::new(ctx.clone(), |ctx: Ctx<'js>, id: i64, data: Value<'js>| -> Result<()> {
            update_impl(&ctx, id, data)
        })?,
    )?;
    ctx.globals().set(
        "__czCryptoFinal",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, id: i64, encoding: Opt<String>| -> Result<Value<'js>> {
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
}
