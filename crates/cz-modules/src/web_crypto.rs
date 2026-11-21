//! Phase 12: `globalThis.crypto` + `crypto.subtle` (WinterCG / Web Crypto sub-set), backed
//! **exclusively** by `ring 0.17` — zero new crates, zero OpenSSL/C crypto.
//!
//! Zero-alloc design (`docs/ZERO_ALLOC_GUIDELINES.md`):
//! * `getRandomValues` fills the caller's JS buffer IN PLACE through `ArrayBuffer::as_raw()`
//!   (no intermediate Rust copy, no new allocation).
//! * `subtle.digest` hashes a BORROWED `&[u8]` view of the input typed array; only the fixed
//!   ring digest output becomes an owned `Vec` which is then MOVED into the JS view via
//!   `vec_to_uint8array` (never `new_copy`).
//! * HMAC `sign`/`verify` run over borrowed slices too; verify uses `ring::hmac::verify`
//!   (constant time) with nested borrows and executes NO JavaScript while borrowed.
//! * AES-GCM encrypt must not mutate the caller's bytes, so one exact plaintext+tag working
//!   buffer is built (`with_capacity(len+16)` + in-place `seal_in_place_append_tag`) and moved
//!   out; decrypt reuses that single allocation (`truncate` + move — no second memcpy).
//! * Key material lives in a process-wide registry keyed by integer ids so every native stays
//!   `'static` (the rquickjs closure rule) and nothing borrowed crosses contexts.

use std::sync::{Mutex, OnceLock};

use rquickjs::function::Opt;
use rquickjs::{ArrayBuffer, Ctx, Exception, Function, Result, Value};

use crate::crypto_module::{alg_of, base64_decode, random_uuid_impl, system_rng, Alg};
use crate::vec_to_uint8array;
use crate::with_borrowed_bytes;

/// AEAD tag length for AES-GCM (Web Crypto default `tagLength` is 128 bits).
const TAG_LEN: usize = 16;
/// GCM nonce length mandated by Web Crypto AES-GCM.
const IV_LEN: usize = 12;

/// Registry entry for an imported secret key. `&'static ring::aead::Algorithm` is a `Copy`
/// pointer to ring's const tables, so entries never clone key material between calls.
enum RegKey {
    Hmac { alg: Alg, key: Vec<u8> },
    Aes { algo: &'static ring::aead::Algorithm, key: Vec<u8> },
}

fn key_registry() -> &'static Mutex<Vec<Option<RegKey>>> {
    static REG: OnceLock<Mutex<Vec<Option<RegKey>>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(Vec::new()))
}

/// Copy-out of a registry slot under the lock: tiny (≤ 32 B) key bytes only, and the guard is
/// ALWAYS dropped before touching QuickJS values (under-lock visitor discipline §3).
fn key_entry(id: f64) -> Option<RegKey> {
    if !id.is_finite() || id < 0.0 || id.fract() != 0.0 || id > usize::MAX as f64 {
        return None;
    }
    let idx = id as usize;
    let reg = key_registry().lock().expect("web crypto registry lock");
    match reg.get(idx).and_then(|slot| slot.as_ref()) {
        Some(RegKey::Hmac { alg, key }) => Some(RegKey::Hmac {
            alg: *alg,
            key: key.clone(),
        }),
        Some(RegKey::Aes { algo, key }) => Some(RegKey::Aes { algo, key: key.clone() }),
        None => None,
    }
}

fn bad_key<'js>(ctx: &Ctx<'js>) -> rquickjs::Error {
    Exception::throw_type(ctx, "The CryptoKey handle is invalid")
}

// ---------------------------------------------------------------------------
// getRandomValues / randomUUID
// ---------------------------------------------------------------------------

/// Fill `[byteOffset, byteOffset + byteLength)` of the caller's ArrayBuffer with CSPRNG bytes
/// IN PLACE. No JavaScript may run while the raw alias is live (`as_raw` contract).
fn fill_random_impl<'js>(ctx: &Ctx<'js>, buffer: ArrayBuffer<'js>, offset: f64, len: f64) -> Result<()> {
    if !offset.is_finite() || !len.is_finite() || offset < 0.0 || len < 0.0 || offset.fract() != 0.0 || len.fract() != 0.0 {
        return Err(Exception::throw_range(
            ctx,
            "getRandomValues offset/length must be non-negative integers",
        ));
    }
    let off = offset as usize;
    let n = len as usize;
    if n == 0 && off == 0 {
        return Ok(());
    }
    // SAFETY: `ring::rand` never re-enters the QuickJS engine while this slice is live, and we
    // derive `*mut u8` directly from `NonNull<[u8]>` without creating an intermediate `&[u8]`.
    let slice = unsafe {
        let raw = buffer
            .as_raw()
            .ok_or_else(|| Exception::throw_type(ctx, "ArrayBuffer is detached"))?;
        let raw_ptr: *mut [u8] = raw.as_ptr();
        let total_len = raw_ptr.len();
        if off.checked_add(n).is_none_or(|end| end > total_len) {
            return Err(Exception::throw_range(
                ctx,
                "getRandomValues range exceeds the ArrayBuffer",
            ));
        }
        std::slice::from_raw_parts_mut((raw_ptr as *mut u8).add(off), n)
    };
    use ring::rand::SecureRandom;
    system_rng()
        .fill(slice)
        .map_err(|e| Exception::throw_message(ctx, &format!("system randomness failed: {e:?}")))
}

// ---------------------------------------------------------------------------
// subtle.digest (SHA-256 / SHA-512)
// ---------------------------------------------------------------------------

fn digest_impl<'js>(ctx: &Ctx<'js>, algorithm: String, data: Value<'js>) -> Result<Value<'js>> {
    let alg = alg_of(&algorithm)
        .ok_or_else(|| Exception::throw_type(&ctx, &format!("UnsupportedAlgorithm: {algorithm}")))?;
    // Hash straight through the borrowed input view; only the fixed-size ring Digest becomes
    // owned bytes afterwards (then moved out — zero second copy).
    let digest = with_borrowed_bytes(&data, |b| ring::digest::digest(alg.digest_alg(), b))?;
    vec_to_uint8array(ctx, digest.as_ref().to_vec())
}

// ---------------------------------------------------------------------------
// subtle.importKey / exportKey
// ---------------------------------------------------------------------------

/// Validate raw key bytes for an algorithm and register the secret key. Returns the registry id.
fn store_key<'js>(
    ctx: &Ctx<'js>,
    name: &str,
    hash: &str,
    bytes: &[u8],
) -> Result<RegKey> {
    if name == "HMAC" {
        let alg = alg_of(hash)
            .ok_or_else(|| Exception::throw_type(ctx, "HMAC import requires hash SHA-256 or SHA-512"))?;
        Ok(RegKey::Hmac { alg, key: bytes.to_vec() })
    } else if name == "AES-GCM" {
        let algo = match bytes.len() {
            16 => &ring::aead::AES_128_GCM,
            32 => &ring::aead::AES_256_GCM,
            n => {
                return Err(Exception::throw_type(
                    ctx,
                    &format!("AES-GCM keys must be 16 or 32 bytes (got {n})"),
                ))
            }
        };
        Ok(RegKey::Aes { algo, key: bytes.to_vec() })
    } else {
        Err(Exception::throw_type(ctx, &format!("UnsupportedAlgorithm: {name}")))
    }
}

fn import_key_impl<'js>(
    ctx: &Ctx<'js>,
    format: String,
    key_data: Value<'js>,
    algorithm: String,
    hash: String,
) -> Result<f64> {
    let bytes = match format.as_str() {
        "raw" => {
            // Strings are accepted as UTF-8 key material (Node webcrypto parity); buffer kinds
            // are borrowed directly — never `.to_vec()`d just to reach `&[u8]`... except here the
            // registry must OWN the key, so exactly one copy into owned storage happens.
            if let Some(s) = key_data.as_string() {
                s.to_string()?.into_bytes()
            } else {
                with_borrowed_bytes(&key_data, |b| b.to_vec())?
            }
        }
        "jwk" => {
            let obj = key_data
                .as_object()
                .ok_or_else(|| Exception::throw_type(ctx, "JWK import expects an object"))?;
            let kty: Option<String> = obj.get("kty")?;
            if kty.as_deref() != Some("oct") {
                return Err(Exception::throw_type(ctx, "Only oct (symmetric) JWKs are supported"));
            }
            let k: String = obj.get("k")?;
            base64_decode(&k, true)
                .ok_or_else(|| Exception::throw_type(ctx, "Invalid base64url JWK key parameter k"))?
        }
        other => {
            return Err(Exception::throw_type(ctx, &format!("Unsupported key format: {other}")));
        }
    };
    let entry = store_key(ctx, algorithm.as_str(), hash.as_str(), &bytes)?;
    let mut reg = key_registry().lock().expect("web crypto registry lock");
    let id = reg.len() as f64;
    reg.push(Some(entry));
    Ok(id)
}

fn export_raw_impl<'js>(ctx: &Ctx<'js>, id: f64) -> Result<Value<'js>> {
    let entry = key_entry(id).ok_or_else(|| bad_key(ctx))?;
    // Raw export hands out the key bytes as a fresh owned buffer (moved out, no second copy):
    // cloning under the lock is the ONE allocation the export path pays.
    let key = match entry {
        RegKey::Hmac { key, .. } | RegKey::Aes { key, .. } => key,
    };
    vec_to_uint8array(ctx, key)
}

// ---------------------------------------------------------------------------
// subtle.sign / verify (HMAC, constant-time verify)
// ---------------------------------------------------------------------------

fn hmac_of<'js>(ctx: &Ctx<'js>, id: f64) -> Result<(ring::hmac::Algorithm, Vec<u8>)> {
    match key_entry(id) {
        Some(RegKey::Hmac { alg, key }) => Ok((alg.hmac_alg(), key)),
        _ => Err(bad_key(ctx)),
    }
}

fn sign_impl<'js>(ctx: &Ctx<'js>, id: f64, data: Value<'js>) -> Result<Value<'js>> {
    let (halg, key) = hmac_of(ctx, id)?;
    let ring_key = ring::hmac::Key::new(halg, &key);
    // Sign over the BORROWED input view; the fixed tag is copied into an owned Vec that is
    // then moved straight into the JS ArrayBuffer.
    let tag = with_borrowed_bytes(&data, |b| ring::hmac::sign(&ring_key, b))?;
    vec_to_uint8array(ctx, tag.as_ref().to_vec())
}

fn verify_impl<'js>(ctx: &Ctx<'js>, id: f64, signature: Value<'js>, data: Value<'js>) -> Result<bool> {
    let (halg, key) = hmac_of(ctx, id)?;
    let ring_key = ring::hmac::Key::new(halg, &key);
    // Nested immutable borrows of two JS buffers + ring's constant-time compare. NO JavaScript
    // executes inside either borrow, so the aliases stay valid (§2/§3).
    let ok = with_borrowed_bytes(&signature, |sig| {
        with_borrowed_bytes(&data, |msg| ring::hmac::verify(&ring_key, msg, sig).is_ok())
    })??;
    Ok(ok)
}

// ---------------------------------------------------------------------------
// subtle.encrypt / decrypt (AES-128/256-GCM via ring LessSafeKey)
// ---------------------------------------------------------------------------

fn aes_of<'js>(ctx: &Ctx<'js>, id: f64) -> Result<ring::aead::LessSafeKey> {
    match key_entry(id) {
        Some(RegKey::Aes { algo, key }) => {
            let unbound = ring::aead::UnboundKey::new(algo, &key)
                .map_err(|_| Exception::throw_type(ctx, "Invalid AES key length for this algorithm"))?;
            Ok(ring::aead::LessSafeKey::new(unbound))
        }
        _ => Err(bad_key(ctx)),
    }
}

/// Borrow a 12-byte GCM nonce (copied out to an owned fixed array before any JS construction).
fn nonce_from<'js>(ctx: &Ctx<'js>, value: &Value<'js>) -> Result<ring::aead::Nonce> {
    let iv = with_borrowed_bytes(value, |b| b.to_vec())?;
    if iv.len() != IV_LEN {
        return Err(Exception::throw_type(
            ctx,
            &format!("AES-GCM iv must be {IV_LEN} bytes (got {})", iv.len()),
        ));
    }
    ring::aead::Nonce::try_assume_unique_for_key(&iv)
        .map_err(|_| Exception::throw_type(ctx, "AES-GCM iv must be 12 bytes"))
}

fn encrypt_impl<'js>(
    ctx: &Ctx<'js>,
    id: f64,
    iv: Value<'js>,
    additional_data: Opt<Value<'js>>,
    data: Value<'js>,
) -> Result<Value<'js>> {
    let key = aes_of(ctx, id)?;
    let nonce = nonce_from(ctx, &iv)?;
    let aad = match additional_data.0 {
        Some(v) if !v.is_undefined() && !v.is_null() => with_borrowed_bytes(&v, |b| b.to_vec())?,
        _ => Vec::new(),
    };
    // Plaintext is NEVER mutated in place (Web semantics), so exactly one working buffer is
    // built: capacity len+16 lets ring APPEND the tag into the spare without reallocating, then
    // the sealed buffer is MOVED into the JS view (`ct || tag`).
    let sealed = with_borrowed_bytes(&data, |pt| -> Result<Vec<u8>> {
        let mut buf = Vec::with_capacity(pt.len() + TAG_LEN);
        buf.extend_from_slice(pt);
        key.seal_in_place_append_tag(nonce, ring::aead::Aad::from(aad), &mut buf)
            .map_err(|_| Exception::throw_message(ctx, "AES-GCM encryption failed"))?;
        Ok(buf)
    })??;
    vec_to_uint8array(ctx, sealed)
}

fn decrypt_impl<'js>(
    ctx: &Ctx<'js>,
    id: f64,
    iv: Value<'js>,
    additional_data: Opt<Value<'js>>,
    data: Value<'js>,
) -> Result<Value<'js>> {
    let key = aes_of(ctx, id)?;
    let nonce = nonce_from(ctx, &iv)?;
    let aad = match additional_data.0 {
        Some(v) if !v.is_undefined() && !v.is_null() => with_borrowed_bytes(&v, |b| b.to_vec())?,
        _ => Vec::new(),
    };
    // One owned working copy of `ct || tag`; ring decrypts IN PLACE and the plaintext length is
    // applied with `truncate`, so the SAME allocation is moved out — no second memcpy.
    let mut buf = with_borrowed_bytes(&data, |b| b.to_vec())?;
    if buf.len() < TAG_LEN {
        return Err(Exception::throw_message(
            ctx,
            "OperationError: AES-GCM ciphertext is shorter than the 16-byte tag",
        ));
    }
    let plaintext = key
        .open_in_place(nonce, ring::aead::Aad::from(&aad[..]), &mut buf[..])
        .map_err(|_| Exception::throw_message(ctx, "OperationError: AES-GCM authentication failed"))?;
    let pt_len = plaintext.len();
    buf.truncate(pt_len);
    vec_to_uint8array(ctx, buf)
}

// ---------------------------------------------------------------------------
// JS shim (`globalThis.crypto`)
// ---------------------------------------------------------------------------

const WEB_CRYPTO_SHIM: &str = r#"
(function () {
    if (typeof globalThis.crypto !== "undefined") return;

    function normName(alg, fallback) {
        if (typeof alg === "string") return alg.replace(/[-_ ]/g, "").toUpperCase();
        if (alg && typeof alg === "object" && typeof alg.name === "string") {
            return alg.name.replace(/[-_ ]/g, "").toUpperCase();
        }
        return fallback || "";
    }
    function normHash(alg) {
        if (typeof alg === "string") return alg.toLowerCase().replace(/[-_ ]/g, "");
        if (alg && typeof alg === "object" && typeof alg.hash === "string") {
            return alg.hash.toLowerCase().replace(/[-_ ]/g, "");
        }
        if (alg && typeof alg.hash === "object" && alg.hash && typeof alg.hash.name === "string") {
            return alg.hash.name.toLowerCase().replace(/[-_ ]/g, "");
        }
        return "";
    }
    // BufferSource coercion: Uint8Array / ArrayBuffer pass through, other views get one view only.
    function asSource(v, what) {
        if (v instanceof Uint8Array) return v;
        if (v instanceof ArrayBuffer) return v;
        if (typeof ArrayBuffer !== "undefined" && ArrayBuffer.isView(v)) {
            return new Uint8Array(v.buffer, v.byteOffset, v.byteLength);
        }
        throw new TypeError(what + " must be an ArrayBuffer or a typed/DataView view");
    }

    globalThis.crypto = {
        getRandomValues: function (array) {
            if (!(typeof ArrayBuffer !== "undefined" && ArrayBuffer.isView(array))) {
                throw new TypeError("crypto.getRandomValues requires a typed array or DataView");
            }
            const n = array.byteLength;
            if (n > 65536) throw new RangeError("crypto.getRandomValues exceeds the 65536 byte limit");
            __czCryptoRandomFill(array.buffer, array.byteOffset, n);
            return array;
        },
        randomUUID: function () { return __czCryptoRandomUUIDWeb(); },
        subtle: {
            digest: async function (algorithm, data) {
                const name = normName(algorithm, "");
                if (name !== "SHA256" && name !== "SHA512") {
                    throw new TypeError("UnsupportedAlgorithm: only SHA-256 and SHA-512 are supported");
                }
                return __czSubtleDigest(name, asSource(data, "digest data"));
            },
            importKey: async function (format, keyData, algorithm, extractable, keyUsages) {
                const fmt = String(format);
                if (fmt !== "raw" && fmt !== "jwk") {
                    throw new TypeError("Unsupported key format: " + format);
                }
                const name = normName(algorithm, "");
                if (name !== "HMAC" && name !== "AESGCM") {
                    throw new TypeError("UnsupportedAlgorithm: " + (algorithm && algorithm.name) + " (use HMAC or AES-GCM)");
                }
                const canonical = name === "HMAC" ? "HMAC" : "AES-GCM";
                const hash = normHash(algorithm);
                if (fmt === "raw" && !(typeof keyData === "string") &&
                    !(keyData instanceof Uint8Array) && !(keyData instanceof ArrayBuffer) &&
                    !(typeof ArrayBuffer !== "undefined" && ArrayBuffer.isView(keyData))) {
                    throw new TypeError("raw importKey requires a BufferSource");
                }
                if (fmt === "jwk") {
                    if (!keyData || typeof keyData !== "object" || typeof keyData.k !== "string") {
                        throw new TypeError("JWK import requires an object with a string k parameter");
                    }
                }
                const extract = extractable === undefined ? true : !!extractable;
                const id = __czSubtleImportKey(fmt, keyData, canonical, hash);
                const usages = [];
                if (keyUsages && typeof keyUsages.length === "number") {
                    for (let i = 0; i < keyUsages.length; i++) usages.push(String(keyUsages[i]));
                } else if (typeof keyUsages === "string") {
                    usages.push(keyUsages);
                }
                return {
                    type: "secret",
                    algorithm: canonical === "HMAC" ? { name: "HMAC", hash: hash } : { name: "AES-GCM" },
                    extractable: extract,
                    usages: usages,
                    _id: id
                };
            },
            exportKey: async function (format, key) {
                if (String(format) !== "raw") {
                    throw new TypeError("exportKey supports only the raw format");
                }
                if (!key || typeof key._id !== "number") throw new TypeError("exportKey requires a CryptoKey");
                if (!key.extractable) throw new Error("NotSupportedError: key is not extractable");
                return __czSubtleExportKey(key._id);
            },
            sign: async function (algorithm, key, data) {
                if (normName(algorithm, "HMAC") !== "HMAC") {
                    throw new TypeError("sign supports only HMAC");
                }
                if (!key || typeof key._id !== "number") throw new TypeError("sign requires a CryptoKey");
                return __czSubtleSign(key._id, asSource(data, "sign data"));
            },
            verify: async function (algorithm, key, signature, data) {
                if (normName(algorithm, "HMAC") !== "HMAC") {
                    throw new TypeError("verify supports only HMAC");
                }
                if (!key || typeof key._id !== "number") throw new TypeError("verify requires a CryptoKey");
                return __czSubtleVerify(key._id, asSource(signature, "signature"), asSource(data, "verify data")) === true;
            },
            _gcmAlg: function (algorithm) {
                if (!algorithm || typeof algorithm !== "object") {
                    throw new TypeError("AES-GCM requires an algorithm object with an iv");
                }
                if (normName(algorithm, "") !== "AESGCM") {
                    throw new TypeError("UnsupportedAlgorithm: " + algorithm.name + " (use AES-GCM)");
                }
                if (algorithm.tagLength !== undefined && Number(algorithm.tagLength) !== 128) {
                    throw new TypeError("AES-GCM tagLength must be 128 bits");
                }
                if (algorithm.iv === undefined || algorithm.iv === null) {
                    throw new TypeError("AES-GCM requires a 12-byte iv");
                }
                const iv = asSource(algorithm.iv, "AES-GCM iv");
                let aad = undefined;
                if (algorithm.additionalData !== undefined && algorithm.additionalData !== null) {
                    aad = asSource(algorithm.additionalData, "additionalData");
                }
                return { iv: iv, aad: aad };
            },
            encrypt: async function (algorithm, key, data) {
                if (!key || typeof key._id !== "number") throw new TypeError("encrypt requires a CryptoKey");
                const g = this._gcmAlg(algorithm);
                return __czSubtleEncrypt(key._id, g.iv, g.aad, asSource(data, "encrypt data"));
            },
            decrypt: async function (algorithm, key, data) {
                if (!key || typeof key._id !== "number") throw new TypeError("decrypt requires a CryptoKey");
                const g = this._gcmAlg(algorithm);
                return __czSubtleDecrypt(key._id, g.iv, g.aad, asSource(data, "decrypt data"));
            }
        }
    };

    // WinterCG compliance marker: Node's webcrypto exposes getCrypto().
    globalThis.crypto.getCrypto = function () { return globalThis.crypto; };
})();
"#;

/// Register the Web Crypto natives + `globalThis.crypto` shim (idempotent). Called by
/// `cz-runner` after `install_fetch`.
pub fn install_web_crypto<'js>(ctx: &Ctx<'js>) -> Result<()> {
    ctx.globals().set(
        "__czCryptoRandomFill",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, buffer: ArrayBuffer<'js>, offset: f64, len: f64| -> Result<()> {
                fill_random_impl(&ctx, buffer, offset, len)
            },
        )?,
    )?;
    ctx.globals().set(
        "__czCryptoRandomUUIDWeb",
        Function::new(ctx.clone(), |ctx: Ctx<'js>| -> Result<String> {
            random_uuid_impl().map_err(|_| Exception::throw_message(&ctx, "system randomness failed"))
        })?,
    )?;
    ctx.globals().set(
        "__czSubtleDigest",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, algorithm: String, data: Value<'js>| -> Result<Value<'js>> {
                digest_impl(&ctx, algorithm, data)
            },
        )?,
    )?;
    ctx.globals().set(
        "__czSubtleImportKey",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, format: String, key_data: Value<'js>, algorithm: String, hash: String| -> Result<f64> {
                import_key_impl(&ctx, format, key_data, algorithm, hash)
            },
        )?,
    )?;
    ctx.globals().set(
        "__czSubtleExportKey",
        Function::new(ctx.clone(), |ctx: Ctx<'js>, id: f64| -> Result<Value<'js>> {
            export_raw_impl(&ctx, id)
        })?,
    )?;
    ctx.globals().set(
        "__czSubtleSign",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, id: f64, data: Value<'js>| -> Result<Value<'js>> {
                sign_impl(&ctx, id, data)
            },
        )?,
    )?;
    ctx.globals().set(
        "__czSubtleVerify",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, id: f64, signature: Value<'js>, data: Value<'js>| -> Result<bool> {
                verify_impl(&ctx, id, signature, data)
            },
        )?,
    )?;
    ctx.globals().set(
        "__czSubtleEncrypt",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, id: f64, iv: Value<'js>, aad: Opt<Value<'js>>, data: Value<'js>| -> Result<Value<'js>> {
                encrypt_impl(&ctx, id, iv, aad, data)
            },
        )?,
    )?;
    ctx.globals().set(
        "__czSubtleDecrypt",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, id: f64, iv: Value<'js>, aad: Opt<Value<'js>>, data: Value<'js>| -> Result<Value<'js>> {
                decrypt_impl(&ctx, id, iv, aad, data)
            },
        )?,
    )?;
    ctx.eval::<(), _>(WEB_CRYPTO_SHIM)?;
    Ok(())
}

#[cfg(test)]
mod web_crypto_tests {
    use super::*;
    use rquickjs::{CatchResultExt, CaughtError, Context, Runtime};

    fn run(source: &str) -> String {
        let rt = Runtime::new().unwrap();
        let ctx = Context::full(&rt).unwrap();
        let out = ctx.with(|ctx| -> String {
            // TextEncoder/TextDecoder live in the fetch shim; install it first (hermetic).
            crate::fetch_module::install_fetch(&ctx).unwrap();
            install_web_crypto(&ctx).unwrap();
            match ctx.eval::<String, _>(source).catch(&ctx) {
                Ok(text) => text,
                Err(err) => panic!(
                    "script failed: {}",
                    match &err {
                        CaughtError::Exception(ex) => ex.message().unwrap_or_else(|| "unknown".into()),
                        other => format!("{other}"),
                    }
                ),
            }
        });
        while let Ok(true) = rt.execute_pending_job() {}
        out
    }

    #[test]
    fn subtle_digest_sha256_and_sha512_match_known_vectors() {
        assert_eq!(
            run(
                r#"
                const abc = new TextEncoder().encode("abc");
                const d256 = __czSubtleDigest("SHA-256", abc);
                if (d256.length !== 32) throw new Error("sha256 len");
                const hex256 = Array.from(d256, (b) => b.toString(16).padStart(2, "0")).join("");
                if (hex256 !== "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad") {
                    throw new Error("sha256 mismatch");
                }
                const d512 = __czSubtleDigest("SHA-512", new ArrayBuffer(0));
                if (d512.length !== 64) throw new Error("sha512 len");
                if (typeof crypto.subtle.digest !== "function") throw new Error("digest missing");
                "DIGEST_OK"
            "#
            ),
            "DIGEST_OK"
        );
    }

    #[test]
    fn hmac_sign_verify_roundtrip_and_tamper_false() {
        assert_eq!(
            run(
                r#"
                const enc = new TextEncoder();
                const id = __czSubtleImportKey("raw", enc.encode("Jefe"), "HMAC", "sha256");
                const msg = enc.encode("what do ya want for nothing?");
                const sig = __czSubtleSign(id, msg);
                if (sig.length !== 32) throw new Error("hmac tag len");
                if (__czSubtleVerify(id, sig, msg) !== true) throw new Error("verify should pass");
                const bad = new Uint8Array(sig);
                bad[0] ^= 0xff;
                if (__czSubtleVerify(id, bad, msg) !== false) throw new Error("tampered must verify false");
                const raw = __czSubtleExportKey(id);
                if (new TextDecoder().decode(raw) !== "Jefe") throw new Error("export mismatch");
                "HMAC_OK"
            "#
            ),
            "HMAC_OK"
        );
    }

    #[test]
    fn aes_gcm_encrypt_decrypt_roundtrip_aad_and_tamper_reject() {
        assert_eq!(
            run(
                r#"
                const keyBytes = new Uint8Array(32);
                for (let i = 0; i < 32; i++) keyBytes[i] = i + 1;
                const id = __czSubtleImportKey("raw", keyBytes, "AES-GCM", "");
                const iv = new Uint8Array(12);
                for (let i = 0; i < 12; i++) iv[i] = 50 + i;
                const aad = new TextEncoder().encode("aad-bytes");
                const pt = new TextEncoder().encode("payload-日本");
                const ct = __czSubtleEncrypt(id, iv, aad, pt);
                if (ct.length !== pt.length + 16) throw new Error("tag layout");
                const back = __czSubtleDecrypt(id, iv, aad, ct);
                if (new TextDecoder().decode(back) !== "payload-日本") throw new Error("roundtrip");
                let threw = false;
                try { const badCt = new Uint8Array(ct); badCt[2] ^= 1; __czSubtleDecrypt(id, iv, aad, badCt); }
                catch (e) { threw = true; }
                if (!threw) throw new Error("tamper must reject");
                threw = false;
                try { __czSubtleDecrypt(id, iv, new TextEncoder().encode("wrong-aad"), ct); }
                catch (e) { threw = true; }
                if (!threw) throw new Error("aad mismatch must reject");
                // 128-bit AES through AES_128_GCM:
                const k16 = new Uint8Array(16);
                const id16 = __czSubtleImportKey("raw", k16, "AES-GCM", "");
                const ct16 = __czSubtleEncrypt(id16, iv, undefined, pt);
                const back16 = __czSubtleDecrypt(id16, iv, undefined, ct16);
                if (back16.length !== pt.length || new TextDecoder().decode(back16) !== "payload-日本") {
                    throw new Error("aes128 roundtrip");
                }
                "AES_OK"
            "#
            ),
            "AES_OK"
        );
    }

    #[test]
    fn getrandom_values_fills_in_place_and_uuid_shape() {
        assert_eq!(
            run(
                r#"
                const a = crypto.getRandomValues(new Uint32Array(4));
                if (!(a instanceof Uint32Array)) throw new Error("must return same view");
                let any = false;
                for (let i = 0; i < 4; i++) if (a[i] !== 0) any = true;
                if (!any) throw new Error("buffer stayed zero");
                const u = crypto.randomUUID();
                if (!/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(u)) {
                    throw new Error("uuid shape " + u);
                }
                let rangeThrew = false;
                try { crypto.getRandomValues(new ArrayBuffer(65537)); } catch (e) { rangeThrew = e instanceof RangeError || e instanceof TypeError; }
                if (!rangeThrew) throw new Error("oversize must throw");
                "RAND_OK"
            "#
            ),
            "RAND_OK"
        );
    }

    #[test]
    fn jwk_import_and_async_promise_surface() {
        // NOTE: async wrappers settle through pending jobs; finish them OUTSIDE the scope.
        let rt = Runtime::new().unwrap();
        let ctx = Context::full(&rt).unwrap();
        ctx.with(|ctx| -> Result<()> {
            crate::fetch_module::install_fetch(&ctx).unwrap();
            install_web_crypto(&ctx).unwrap();
            ctx.eval::<(), _>(r#"
                const jwk = { kty: "oct", k: "SmVmZQ" }; // "Jefe" base64url (unpadded)
                crypto.subtle.importKey("jwk", jwk, { name: "HMAC", hash: "SHA-256" }, true, ["sign", "verify"])
                    .then((key) => {
                        if (key.type !== "secret" || key.extractable !== true) throw new Error("key shape");
                        if (!Array.isArray(key.usages) || key.usages.length !== 2) throw new Error("usages");
                        const enc = new TextEncoder();
                        return crypto.subtle.sign("HMAC", key, enc.encode("what do ya want for nothing?"));
                    })
                    .then((sig) => {
                        const hex = Array.from(new Uint8Array(sig), (b) => b.toString(16).padStart(2, "0")).join("");
                        if (hex !== "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843") {
                            throw new Error("jwk hmac mismatch " + hex);
                        }
                        globalThis.__R = "JWK_OK";
                    })
                    .catch((e) => { globalThis.__R = "FAIL: " + e; });
            "#)
            .unwrap();
            Ok(())
        })
        .unwrap();
        while let Ok(true) = rt.execute_pending_job() {}
        let out = ctx.with(|ctx| -> String {
            ctx.globals().get::<_, String>("__R").ok().unwrap_or_else(|| "MISSING".into())
        });
        assert_eq!(out, "JWK_OK");
    }

    #[test]
    fn base64url_jwk_decodes_unpadded_values() {
        assert_eq!(base64_decode("SmVmZQ", true), Some(b"Jefe".to_vec()));
        assert_eq!(base64_decode("AP+A", false), Some(vec![0u8, 255, 128]));
        // Hand-checked url-safe stream: A P - A _ Q -> 0x00 0xFF 0x80 0xFD (+ dangling 4 bits).
        assert_eq!(base64_decode("AP-A_Q", true), Some(vec![0u8, 255, 128, 253]));
        assert_eq!(base64_decode("YQ==", false), Some(vec![b'a']));
        assert_eq!(base64_decode("a", false), None); // dangling lone sextet per HTML spec
        assert_eq!(base64_decode(" \t\n Y W J j \r\n", false), Some(b"abc".to_vec()));
    }

    #[test]
    fn nan_and_negative_key_handles_reject_instead_of_aliasing_slot_zero() {
        assert_eq!(
            run(
                r#"
                const enc = new TextEncoder();
                const id = __czSubtleImportKey("raw", enc.encode("secret-slot-0"), "HMAC", "sha256");
                if (id < 0) throw new Error("expected non-negative id");
                for (const badId of [-1, -100, NaN, Infinity, -Infinity, 0.5]) {
                    let threw = false;
                    try { __czSubtleExportKey(badId); } catch (_e) { threw = true; }
                    if (!threw) throw new Error("bad id must reject export: " + badId);
                    threw = false;
                    try { __czSubtleSign(badId, enc.encode("msg")); } catch (_e) { threw = true; }
                    if (!threw) throw new Error("bad id must reject sign: " + badId);
                }
                const zeroArr = crypto.getRandomValues(new Uint8Array(0));
                if (zeroArr.length !== 0) throw new Error("zero-len getRandomValues");
                "HANDLE_GUARDS_OK"
            "#
            ),
            "HANDLE_GUARDS_OK"
        );
    }
}
