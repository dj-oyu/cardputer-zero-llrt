//! Phase 13: minimal `Buffer` (`extends Uint8Array`) + `"node:buffer"` / `"buffer"`.
//!
//! Zero-alloc design (`docs/ZERO_ALLOC_GUIDELINES.md`):
//! * UTF-8 / Base64 / Base64URL / Hex / Latin-1 conversions run as ONE native Rust pass over
//!   BORROWED JS bytes (`with_borrowed_bytes`) or move owned bytes out through
//!   `vec_to_uint8array` — NEVER a char-by-char JavaScript loop.
//! * `toString("utf8")` on valid data builds the QuickJS string straight from the borrowed
//!   slice (no intermediate Rust `String`).
//! * The `"node:buffer"` export and the global `Buffer` are the SAME object (identity), so
//!   `import { Buffer } from "node:buffer"` and `globalThis.Buffer` interop by reference.
//! * `crypto_module`'s standard Base64 codec is reused for `"base64"`; only the URL-safe unpadded
//!   variant lives here (its alphabet/padding semantics differ, so it does not duplicate logic).

use std::sync::atomic::{AtomicBool, Ordering};

use rquickjs::function::Opt;
use rquickjs::{Ctx, Exception, Function, Result, String as JsString, Value};

use crate::crypto_module::{base64_decode, base64_encode};
use crate::{vec_to_uint8array, with_borrowed_bytes};

static INSTALLED: AtomicBool = AtomicBool::new(false);

const B64URL_TABLE: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// URL-safe Base64 WITHOUT padding (`-`/`_` alphabet) — the shape Node's `base64url` emits.
fn base64url_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = if chunk.len() > 1 { chunk[1] } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] } else { 0 };
        let v = ((b0 as u32) << 16) | ((b1 as u32) << 8) | b2 as u32;
        out.push(B64URL_TABLE[((v >> 18) & 63) as usize] as char);
        out.push(B64URL_TABLE[((v >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64URL_TABLE[((v >> 6) & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(B64URL_TABLE[(v & 63) as usize] as char);
        }
    }
    out
}

/// Lowercase hex via a stack nibble table (faster than `format!("{b:02x}")` per byte).
fn hex_encode_fast(data: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(data.len() * 2);
    for b in data {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 15) as usize] as char);
    }
    out
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Node-style hex decode: ASCII whitespace is skipped and the first non-hex character terminates
/// the stream (a dangling nibble is dropped exactly like `Buffer.from(hex,'hex')` does).
fn hex_decode(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() / 2 + 1);
    let mut hi: Option<u8> = None;
    for &b in text {
        if matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0c) {
            continue;
        }
        match hex_nibble(b) {
            Some(n) => {
                if let Some(h) = hi.take() {
                    out.push((h << 4) | n);
                } else {
                    hi = Some(n);
                }
            }
            None => break,
        }
    }
    out
}

/// Canonical encoding names: `"utf-8"`/empty/undefined mean UTF-8, `"binary"` is an alias of
/// `"latin1"`. Unknown names are a `TypeError` (Node's message shape).
fn norm_encoding(ctx: &Ctx<'_>, enc: Option<&str>) -> Result<&'static str> {
    let lower = match enc {
        None | Some("") => "utf8",
        Some(e) => match e.to_ascii_lowercase().as_str() {
            "utf8" | "utf-8" | "ucs2" | "ucs-2" | "utf16le" | "utf-16le" => "utf8",
            "base64" => "base64",
            "base64url" => "base64url",
            "hex" => "hex",
            "latin1" | "binary" => "latin1",
            _ => "",
        },
    };
    if lower.is_empty() {
        return Err(Exception::throw_message(
            ctx,
            &format!("Unknown encoding: {}", enc.unwrap_or("")),
        ));
    }
    Ok(match lower {
        "utf8" => "utf8",
        "base64" => "base64",
        "base64url" => "base64url",
        "hex" => "hex",
        _ => "latin1",
    })
}

/// `Buffer.from(string, encoding)` — decode into ONE owned buffer MOVED to JavaScript (no copy).
fn buf_decode_impl<'js>(ctx: &Ctx<'js>, text: JsString, enc: Opt<String>) -> Result<Value<'js>> {
    let cs = text.to_cstring()?;
    let enc = norm_encoding(ctx, enc.0.as_deref())?;
    let src = cs.as_str();
    let bytes: Vec<u8> = match enc {
        "utf8" => src.as_bytes().to_vec(),
        "base64" => base64_decode(src, false)
            .ok_or_else(|| Exception::throw_type(ctx, "Invalid base64 input"))?,
        "base64url" => base64_decode(src, true)
            .ok_or_else(|| Exception::throw_type(ctx, "Invalid base64url input"))?,
        "hex" => hex_decode(src.as_bytes()),
        _ => src.chars().map(|c| (c as u32 & 0xff) as u8).collect(), // latin1/binary: low bytes
    };
    vec_to_uint8array(ctx, bytes)
}

/// `buf.toString(encoding)` — ONE native encode pass over the BORROWED view bytes. Valid UTF-8
/// builds the QuickJS string DIRECTLY from the borrowed slice (the same shape as
/// `__czUtf8Decode`); no JavaScript runs while the borrow is held.
fn buf_encode_impl<'js>(ctx: &Ctx<'js>, input: Value<'js>, enc: Opt<String>) -> Result<Value<'js>> {
    let enc = norm_encoding(ctx, enc.0.as_deref())?;
    with_borrowed_bytes(&input, |b| -> Result<Value<'js>> {
        match enc {
            "utf8" => match std::str::from_utf8(b) {
                Ok(s) => Ok(JsString::from_str(ctx.clone(), s)?.into_value()),
                // Lossy mode mirrors Node's toString(): U+FFFD replacements.
                Err(_) => Ok(JsString::from_str(ctx.clone(), String::from_utf8_lossy(b).as_ref())?.into_value()),
            },
            "base64" => Ok(JsString::from_str(ctx.clone(), &base64_encode(b))?.into_value()),
            "base64url" => Ok(JsString::from_str(ctx.clone(), &base64url_encode(b))?.into_value()),
            "hex" => Ok(JsString::from_str(ctx.clone(), &hex_encode_fast(b))?.into_value()),
            _ => {
                // latin1: every byte becomes exactly one code unit <= U+00FF.
                let mut s = String::with_capacity(b.len());
                for byte in b {
                    s.push(char::from_u32(*byte as u32).expect("Latin-1 code unit"));
                }
                Ok(JsString::from_str(ctx.clone(), &s)?.into_value())
            }
        }
    })?
}

/// `Buffer.byteLength(string, encoding)` — native measure without a JS loop.
fn buf_byte_length_impl<'js>(ctx: &Ctx<'js>, text: JsString, enc: Opt<String>) -> Result<usize> {
    let cs = text.to_cstring()?;
    let enc = norm_encoding(ctx, enc.0.as_deref())?;
    Ok(match enc {
        "utf8" => cs.as_str().len(),
        "latin1" => cs.chars().count(),
        "hex" => hex_decode(cs.as_str().as_bytes()).len(),
        "base64" => base64_decode(cs.as_str(), false).map(|v| v.len()).unwrap_or(0),
        _ => base64_decode(cs.as_str(), true).map(|v| v.len()).unwrap_or(0),
    })
}

/// `buf.equals(other)` — compares two BORROWED views (no copies); the nested borrows are
/// disjoint slices of distinct (or identical) buffers and no JavaScript runs while held.
fn buf_equals_impl<'js>(ctx: &Ctx<'js>, a: Value<'js>, b: Value<'js>) -> Result<bool> {
    if !a.is_object() || !b.is_object() {
        return Err(Exception::throw_type(
            ctx,
            "The \"other\" argument must be an instance of Uint8Array or Buffer",
        ));
    }
    let eq = with_borrowed_bytes(&a, |ab| with_borrowed_bytes(&b, |bb| ab.len() == bb.len() && ab == bb))??;
    Ok(eq)
}

/// Install the global `Buffer` (`extends Uint8Array`) idempotently. Called by `cz-runner` at
/// startup AND by `BufferModule.evaluate` so `"node:buffer"` always exports THE SAME object.
pub fn install_buffer<'js>(ctx: &Ctx<'js>) -> Result<()> {
    if INSTALLED.load(Ordering::SeqCst) && ctx.globals().get::<_, bool>("__czBufferInstalled").unwrap_or(false) {
        return Ok(());
    }

    ctx.globals().set(
        "__czBufDecode",
        Function::new(ctx.clone(), |ctx: Ctx<'js>, text: JsString, enc: Opt<String>| -> Result<Value<'js>> {
            buf_decode_impl(&ctx, text, enc)
        })?,
    )?;
    ctx.globals().set(
        "__czBufEncode",
        Function::new(ctx.clone(), |ctx: Ctx<'js>, input: Value<'js>, enc: Opt<String>| -> Result<Value<'js>> {
            buf_encode_impl(&ctx, input, enc)
        })?,
    )?;
    ctx.globals().set(
        "__czBufByteLength",
        Function::new(ctx.clone(), |ctx: Ctx<'js>, text: JsString, enc: Opt<String>| -> Result<usize> {
            buf_byte_length_impl(&ctx, text, enc)
        })?,
    )?;
    ctx.globals().set(
        "__czBufEquals",
        Function::new(ctx.clone(), |ctx: Ctx<'js>, a: Value<'js>, b: Value<'js>| -> Result<bool> {
            buf_equals_impl(&ctx, a, b)
        })?,
    )?;
    ctx.eval::<(), _>(BUFFER_SHIM)?;
    INSTALLED.store(true, Ordering::SeqCst);
    Ok(())
}

const BUFFER_SHIM: &str = r#"
(function () {
    if (globalThis.__czBufferInstalled) return;

    function normEnc(e) {
        if (e === undefined || e === null || e === "") e = "utf8";
        else e = String(e).toLowerCase();
        if (e === "utf-8" || e === "ucs2" || e === "ucs-2" || e === "utf16le" || e === "utf-16le") e = "utf8";
        else if (e === "binary") e = "latin1";
        if (e !== "utf8" && e !== "base64" && e !== "base64url" && e !== "hex" && e !== "latin1") {
            throw new TypeError("Unknown encoding: " + e);
        }
        return e;
    }

    // Minimal Node-compatible Buffer: a real Uint8Array subclass, so views keep their internal
    // slots (zero-copy reads) and `instanceof Uint8Array` stays true everywhere downstream.
    class Buffer extends Uint8Array {}

    function wrapView(u8) {
        if (!(u8 instanceof Buffer)) Object.setPrototypeOf(u8, Buffer.prototype);
        return u8;
    }

    Buffer.from = function (input, encoding) {
        const enc = normEnc(encoding);
        if (typeof input === "string") return wrapView(__czBufDecode(input, enc));
        if (input instanceof ArrayBuffer) return wrapView(new Uint8Array(input).slice()); // own copy of the bytes
        if (ArrayBuffer.isView(input)) return wrapView(input.slice(0));                   // native element copy
        if (Array.isArray(input)) {
            const u = new Uint8Array(input.length);
            for (let i = 0; i < input.length; i++) u[i] = input[i] & 0xff; // number[] shaping (cold path)
            return wrapView(u);
        }
        throw new TypeError("Buffer.from requires a string, Buffer, ArrayBuffer, Uint8Array or number[]");
    };

    Buffer.alloc = function (size, fill) {
        if (!Number.isInteger(size) || size < 0) throw new RangeError("Buffer.alloc size must be a non-negative integer");
        const u = new Uint8Array(size); // QuickJS zero-fills fresh backing
        if (fill !== undefined && fill !== null) {
            if (typeof fill === "number") u.fill(fill & 0xff);
            else {
                const pat = typeof fill === "string" ? __czBufDecode(fill, "utf8")
                    : ArrayBuffer.isView(fill) ? fill : null;
                if (pat && pat.length > 0) {
                    for (let i = 0; i < u.length; i++) u[i] = pat[i % pat.length]; // pattern fill (cold path)
                }
            }
        }
        return wrapView(u);
    };

    Buffer.allocUnsafe = function (size) {
        if (!Number.isInteger(size) || size < 0) throw new RangeError("Buffer.allocUnsafe size must be a non-negative integer");
        return wrapView(new Uint8Array(size));
    };
    Buffer.allocUnsafeSlow = Buffer.allocUnsafe;

    Buffer.concat = function (list, totalLength) {
        if (!Array.isArray(list)) throw new TypeError("Buffer.concat expects an array of Uint8Array/Buffer");
        let sum = 0;
        for (let i = 0; i < list.length; i++) {
            if (!(list[i] instanceof Uint8Array)) throw new TypeError("concat list entry must be a Uint8Array/Buffer");
            sum += list[i].length;
        }
        const total = totalLength === undefined ? sum : Number(totalLength);
        if (!Number.isInteger(total) || total < 0 || total > sum) throw new RangeError("Buffer.concat totalLength out of range");
        const out = new Uint8Array(total);
        let off = 0;
        for (let i = 0; i < list.length && off < total; i++) {
            const src = list[i];
            const n = Math.min(src.length, total - off);
            out.set(n === src.length ? src : Uint8Array.prototype.subarray.call(src, 0, n), off); // native bulk copy
            off += n;
        }
        return wrapView(out);
    };

    Buffer.isBuffer = function (val) { return val instanceof Buffer; };

    Buffer.byteLength = function (input, encoding) {
        return __czBufByteLength(typeof input === "string" ? input : String(input), normEnc(encoding));
    };

    Buffer.prototype.toString = function (encoding, start, end) {
        let enc;
        if (encoding === undefined || encoding === null) enc = "utf8";
        else if (typeof encoding !== "string") enc = "utf8"; // Buffer/number forms fall back to utf8 text
        else enc = normEnc(encoding);
        if (start === undefined && end === undefined) return __czBufEncode(this, enc);
        const view = Uint8Array.prototype.subarray.call(this, start || 0, end === undefined ? this.length : end);
        return __czBufEncode(view, enc);
    };

    Buffer.prototype.equals = function (other) { return __czBufEquals(this, other); };

    Buffer.prototype.subarray = function (start, end) {
        const s = Uint8Array.prototype.subarray.call(this, start, end); // zero-copy VIEW
        Object.setPrototypeOf(s, Buffer.prototype);
        return s;
    };
    Buffer.prototype.slice = Buffer.prototype.subarray;

    Buffer.prototype.copy = function (target, targetStart, sourceStart, sourceEnd) {
        if (!(target instanceof Uint8Array)) throw new TypeError("The \"target\" argument must be a Uint8Array or Buffer");
        let ss = sourceStart === undefined ? 0 : Math.floor(sourceStart);
        if (ss < 0) ss = 0;
        const len = this.length;
        let se = sourceEnd === undefined ? len : Math.min(len, Math.floor(sourceEnd));
        let ts = targetStart === undefined ? 0 : Math.floor(targetStart);
        if (ts < 0 || !Number.isFinite(ts)) ts = 0;
        if (se <= ss || ts >= target.length) return 0;
        const n = Math.min(se - ss, target.length - ts);
        // ONE engine-level bulk copy: typed-array `set` runs in C over a zero-copy view (no JS
        // byte loop), which also handles aliased same-buffer targets like Node's memmove does.
        target.set(Uint8Array.prototype.subarray.call(this, ss, ss + n), ts);
        return n;
    };

    globalThis.Buffer = Buffer;
    globalThis.__czBufferInstalled = true;
})();
"#;

/// Builtin module shape for `"node:buffer"` / `"buffer"`: exports `{ Buffer, default }` where BOTH
/// names point at the SAME class object as `globalThis.Buffer` (identity across realms of one rt).
pub struct BufferModule;

impl rquickjs::module::ModuleDef for BufferModule {
    fn declare<'js>(decl: &rquickjs::module::Declarations<'js>) -> Result<()> {
        for name in ["Buffer", "default"] {
            decl.declare(name)?;
        }
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &rquickjs::module::Exports<'js>) -> Result<()> {
        // Guarantee the class exists even when a context never ran the runner's install sequence.
        install_buffer(ctx)?;
        let buffer = ctx.globals().get::<_, Value>("Buffer")?;
        exports.export("Buffer", buffer.clone())?;
        exports.export("default", buffer)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_codecs_round_trip_all_supported_encodings() {
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            install_buffer(&ctx).unwrap();
            let src = "CardputerZero 日本語";
            let utf8: String = ctx
                .eval(r#"__czBufEncode(__czBufDecode("CardputerZero 日本語", "utf8"), "utf8")"#)
                .unwrap();
            assert_eq!(utf8, src);
            let b64: String = ctx.eval(r#"__czBufEncode(__czBufDecode("QQ==", "base64"), "base64")"#).unwrap();
            assert_eq!(b64, "QQ==");
            let hex: String = ctx.eval(r#"__czBufEncode(__czBufDecode("deadbeef", "hex"), "hex")"#).unwrap();
            assert_eq!(hex, "deadbeef");
            let url: String = ctx
                .eval(r#"__czBufEncode(new Uint8Array([0xfb, 0xff, 0xfe]), "base64url")"#)
                .unwrap();
            assert_eq!(url, "-__-"); // URL-safe alphabet, no padding, never '+' or '/'
            let back: String = ctx
                .eval(r#"__czBufEncode(__czBufDecode("-__-", "base64url"), "hex")"#)
                .unwrap();
            assert_eq!(back, "fbfffe");
            let n: usize = ctx.eval(r#"__czBufByteLength("日本語", "utf8")"#).unwrap();
            assert_eq!(n, 9);
        });
        while let Ok(true) = rt.execute_pending_job() {}
    }

    #[test]
    fn buffer_extends_uint8array_and_module_exports_share_identity() {
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            install_buffer(&ctx).unwrap();
            let ok: bool = ctx
                .eval(r#"(() => {
                    const b = Buffer.from("ab", "hex"); // 0xab
                    if (!Buffer.isBuffer(b) || !(b instanceof Uint8Array)) return false;
                    if (b.toString("hex") !== "ab") return false;
                    if (b.subarray(0, 1) instanceof Buffer === false) return false;
                    const c = Buffer.alloc(2, 0x42);
                    if (c.toString("latin1") !== "BB") return false;
                    const cat = Buffer.concat([Buffer.from("x"), Buffer.from("y")]);
                    if (cat.toString() !== "xy" || !Buffer.isBuffer(cat)) return false;
                    if (!c.equals(Buffer.alloc(2, 0x42))) return false;
                    const t = new Uint8Array(4);
                    if (cat.copy(t, 1) !== 2) return false;
                    if (t[1] !== 0x78 || t[2] !== 0x79) return false;
                    return true;
                })()"#)
                .unwrap();
            assert!(ok, "Buffer semantics");
        });
        while let Ok(true) = rt.execute_pending_job() {}
    }

    #[test]
    fn hex_decode_is_node_lenient_and_base64url_unpadded() {
        // "DE adb" decodes to DE,AD; the dangling 'b' stays pending until the invalid 0xCE breaks
        // the stream, so exactly two bytes land (Node truncates the same way).
        assert_eq!(hex_decode(b"DE adb\xceef!"), vec![0xde, 0xad]);
        assert_eq!(hex_decode(b"1"), Vec::new()); // odd dangling nibble yields nothing
        assert_eq!(base64url_encode(&[0xfb, 0xff, 0xfe]), "-__-"); // 3 bytes -> 4 chars, never '=' + /
        assert_eq!(base64url_encode(&[0xfb]), "-w"); // single byte: standard "+w==" -> urlsafe "-w"
        assert_eq!(base64url_encode(&[]), "");
    }
}
