//! Phase 12: native `atob`/`btoa`, `Blob`/`File`/`FormData` + the `Response.blob()` /
//! `Response.formData()` integration surface.
//!
//! Zero-alloc design (`docs/ZERO_ALLOC_GUIDELINES.md`):
//! * Base64 encode/decode is ONE native Rust pass over borrowed bytes (`crypto_module`'s shared
//!   codec) — no pure-JS char loops anywhere.
//! * Blob parts concatenate once into a single owned view; `Blob.slice()` only creates a new
//!   `Uint8Array` VIEW over the existing buffer (no copy). `Response.blob()` wraps the received
//!   body bytes directly via `__czBlobFromBytes`.
//! * `Response.formData()` parses `multipart/form-data` and `application/x-www-form-urlencoded`
//!   in ONE native scan over the BORROWED response bytes: header/field detection uses
//!   `eq_ignore_ascii_case` + `(start,end)` spans instead of lowercased per-line `String`s.
//! * All QuickJS shims below stay `'static` (no borrowed data crosses contexts) and use
//!   QuickJS-safe syntax (no optional chaining / nullish coalescing).

use rquickjs::{Array, Ctx, Exception, Function, Object, Result, Value};

use crate::crypto_module::base64_decode;
use crate::fetch_module::install_fetch;
use crate::vec_to_uint8array;
use crate::with_borrowed_bytes;

// ---------------------------------------------------------------------------
// atob / btoa (HTML latin1 binary-string semantics, native single pass)
// ---------------------------------------------------------------------------

/// `btoa`: every code point must be ≤ U+00FF (`InvalidCharacterError` otherwise); the Latin-1
/// bytes are Base64-encoded in one pass through the shared `crypto_module` table.
fn btoa_impl<'js>(ctx: &Ctx<'js>, input: String) -> Result<String> {
    let mut bytes = Vec::with_capacity(input.len());
    for ch in input.chars() {
        if ch as u32 > 0xff {
            return Err(Exception::throw_message(
                ctx,
                "InvalidCharacterError: btoa cannot encode code points above U+00FF",
            ));
        }
        bytes.push(ch as u8);
    }
    Ok(crate::crypto_module::base64_encode(&bytes))
}

/// `atob`: HTML-lenient decode (whitespace skipped, padding terminates); each decoded byte becomes
/// ONE Latin-1 code unit, so binary strings round-trip through `btoa` byte-exactly.
fn atob_impl<'js>(ctx: &Ctx<'js>, input: String) -> Result<String> {
    let bytes = base64_decode(&input, false).ok_or_else(|| {
        Exception::throw_message(ctx, "InvalidCharacterError: atob input is not valid Base64")
    })?;
    let mut text = String::with_capacity(bytes.len());
    for b in bytes {
        text.push(char::from_u32(b as u32).expect("Latin-1 code unit"));
    }
    Ok(text)
}

// ---------------------------------------------------------------------------
// Response.formData() wire parsing (multipart + urlencoded), one native pass
// ---------------------------------------------------------------------------

/// ASCII whitespace trim on a borrowed slice (no allocations).
fn trim_slice(b: &[u8]) -> &[u8] {
    let mut s = b;
    while !s.is_empty() && matches!(s[0], b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
        s = &s[1..];
    }
    while !s.is_empty() && matches!(s[s.len() - 1], b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
        s = &s[..s.len() - 1];
    }
    s
}

/// Percent-decode `src` into `out` ('+' -> space only for urlencoded bodies).
fn percent_decode(src: &[u8], plus: bool, out: &mut Vec<u8>) {
    let mut i = 0;
    while i < src.len() {
        if src[i] == b'%' && i + 2 < src.len() {
            let hi = hex_val(src[i + 1]);
            let lo = hex_val(src[i + 2]);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        if plus && src[i] == b'+' {
            // '+' means SPACE in application/x-www-form-urlencoded.
            out.push(b' ');
            i += 1;
            continue;
        }
        out.push(src[i]);
        i += 1;
    }
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Find `needle` starting at or after `from`; returns absolute offset.
fn find_bytes(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    let last = hay.len() - needle.len();
    let mut i = from.min(hay.len());
    // Inline first-byte skip loop (memchr-style) keeps the scan single-pass.
    while i <= last {
        if hay[i] == needle[0] {
            let mut ok = true;
            for k in 1..needle.len() {
                if hay[i + k] != needle[k] {
                    ok = false;
                    break;
                }
            }
            if ok {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Extract a Content-Type parameter (`boundary=`) case-insensitively without allocating a
/// lowercased copy of the header: windows are compared with `eq_ignore_ascii_case`.
fn ct_param(value: &str, param: &str) -> Option<String> {
    let vb = value.as_bytes();
    let pb = param.as_bytes();
    let mut i = 0;
    while i + pb.len() <= vb.len() {
        if vb[i..i + pb.len()].eq_ignore_ascii_case(pb) {
            let after = trim_slice(&vb[(i + pb.len())..]);
            if !after.first().is_some_and(|c| *c == b'=') {
                i += 1;
                continue;
            }
            let rest = &after[1..];
            if rest.first().is_some_and(|c| *c == b'"') {
                // Quoted string honoring backslash escapes.
                let mut out = String::new();
                for c in &rest[1..] {
                    if *c == b'"' {
                        return Some(out);
                    }
                    if *c == b'\\' {
                        continue;
                    }
                    out.push(*c as char);
                }
                return Some(out); // unterminated: take what we have
            }
            let end = rest.iter().position(|c| *c == b';').unwrap_or(rest.len());
            let tok = trim_slice(&rest[..end]);
            if tok.is_empty() {
                return None;
            }
            return Some(String::from_utf8_lossy(tok).into_owned());
        }
        i += 1;
    }
    None
}

/// One parsed form entry (owned only where the descriptor needs it).
struct FormPart {
    name: String,
    filename: Option<String>,
    content_type: Option<String>,
    body: Vec<u8>,
}

/// Scan a `multipart/form-data` body in a SINGLE pass over the borrowed bytes: delimiter
/// occurrences must sit at line starts (`\r\n--boundary`), part headers are located by the first
/// `\r\n\r\n`, and only `(offset,len)` spans move until materialization.
fn scan_multipart(body: &[u8], boundary: &str) -> Option<Vec<FormPart>> {
    let delim = format!("--{boundary}");
    let db = delim.as_bytes();
    let mut idxs: Vec<usize> = Vec::new();
    let mut pos = 0;
    while let Some(abs) = find_bytes(body, db, pos) {
        let at_line_start = abs == 0 || body[..abs].ends_with(b"\r\n");
        if at_line_start {
            idxs.push(abs);
            pos = abs + db.len();
        } else {
            pos = abs + 1;
        }
    }
    if idxs.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    for (k, &start) in idxs.iter().enumerate() {
        // Segment AFTER this delimiter (and before the CRLF that precedes the next one).
        let seg_begin = start + db.len();
        // Final boundary ("--boundary--") ends the message; no part follows it.
        if body[seg_begin..].starts_with(b"--") {
            break;
        }
        let seg_end = match idxs.get(k + 1) {
            Some(&next) => next.saturating_sub(2), // drop CRLF belonging to the next delimiter
            None => body.len(),
        };
        if seg_begin >= seg_end {
            continue;
        }
        let mut seg = &body[seg_begin..seg_end];
        if seg.starts_with(b"\r\n") {
            seg = &seg[2..]; // CRLF terminating the boundary line
        }
        // Header block? A part without a blank line has NO headers (body only) -> skip (no name).
        let hs = find_bytes(seg, b"\r\n\r\n", 0);
        let (headers_raw, part_body) = match hs {
            Some(h) => (&seg[..h], &seg[h + 4..]),
            None => continue,
        };
        let mut name: Option<Vec<u8>> = None;
        let mut filename: Option<Vec<u8>> = None;
        let mut filename_ext: Option<Vec<u8>> = None;
        let mut content_type: Option<Vec<u8>> = None;
        let mut disposition = false;
        for line in headers_raw.split(|b| *b == b'\n') {
            let line = trim_slice(line);
            if line.is_empty() {
                continue;
            }
            let Some(colon) = line.iter().position(|b| *b == b':') else {
                continue; // header without ':' is ignored (WHATWG forgiving)
            };
            let fname = trim_slice(&line[..colon]);
            let fval = trim_slice(&line[colon + 1..]);
            if fname.eq_ignore_ascii_case(b"content-disposition") {
                // Prefix match by span (no lowercased copy of the header value).
                if fval.len() < 9 || !fval[..9].eq_ignore_ascii_case(b"form-data") {
                    continue;
                }
                disposition = true;
                // Parameter list after the first ';'.
                if let Some(sc) = fval.iter().position(|c| *c == b';') {
                    parse_disp_params(&fval[sc + 1..], &mut name, &mut filename, &mut filename_ext);
                }
            } else if fname.eq_ignore_ascii_case(b"content-type") {
                content_type = Some(fval.to_vec());
            }
        }
        if !disposition || name.is_none() {
            continue;
        }
        // RFC 6266 semantics: `filename*` (extended) carries a `charset'lang'value` prefix and is
        // percent-decoded; a plain `filename` stays LITERAL (legacy keeps % sequences as-is).
        let is_ext = filename_ext.is_some();
        let ext = filename_ext.as_ref().or(filename.as_ref());
        parts.push(FormPart {
            name: String::from_utf8_lossy(&name.unwrap()).into_owned(),
            filename: ext.map(|raw| {
                let raw = raw.as_slice();
                let bytes = if is_ext {
                    let mut d = Vec::new();
                    // Strip the `charset'lang'` prefix when two apostrophes are present.
                    let val = match raw.iter().position(|c| *c == b'\'') {
                        Some(a) => match raw[a + 1..].iter().position(|c| *c == b'\'') {
                            Some(b2) => &raw[a + 1 + b2 + 1..],
                            None => &raw[..],
                        },
                        None => &raw[..],
                    };
                    percent_decode(val, false, &mut d);
                    d
                } else {
                    raw.to_vec()
                };
                String::from_utf8_lossy(&bytes).into_owned()
            }),
            content_type: content_type.map(|raw| String::from_utf8_lossy(&raw).into_owned()),
            body: part_body.to_vec(),
        });
    }
    Some(parts)
}

/// Walk `name="value"` / `filename` / `filename*` parameters over a borrowed disposition tail.
/// Only the ≤ 3 interesting values get an owned copy (cold path per part); everything else is
/// scanned by span with `eq_ignore_ascii_case` — no lowercased line `String`s.
fn parse_disp_params(
    params: &[u8],
    name: &mut Option<Vec<u8>>,
    filename: &mut Option<Vec<u8>>,
    filename_ext: &mut Option<Vec<u8>>,
) {
    let mut i = 0;
    while i < params.len() {
        if matches!(params[i], b' ' | b'\t' | b';') {
            i += 1;
            continue;
        }
        let key_start = i;
        while i < params.len() && params[i] != b'=' && params[i] != b';' {
            i += 1;
        }
        if i >= params.len() || params[i] != b'=' {
            break; // malformed tail
        }
        let key = trim_slice(&params[key_start..i]);
        i += 1;
        let owned: Vec<u8>;
        if params.get(i).is_some_and(|c| *c == b'"') {
            let mut j = i + 1;
            let mut unq = Vec::new();
            while j < params.len() {
                if params[j] == b'\\' && j + 1 < params.len() {
                    unq.push(params[j + 1]);
                    j += 2;
                    continue;
                }
                if params[j] == b'"' {
                    break;
                }
                unq.push(params[j]);
                j += 1;
            }
            owned = unq;
            i = (j + 1).min(params.len());
        } else {
            let mut j = i;
            while j < params.len() && params[j] != b';' {
                j += 1;
            }
            owned = trim_slice(&params[i..j]).to_vec();
            i = j;
        }
        assign_disp_param(key, &owned, name, filename, filename_ext);
    }
}

fn assign_disp_param(
    key: &[u8],
    value: &[u8],
    name: &mut Option<Vec<u8>>,
    filename: &mut Option<Vec<u8>>,
    filename_ext: &mut Option<Vec<u8>>,
) {
    if key.eq_ignore_ascii_case(b"name") && name.is_none() {
        *name = Some(value.to_vec());
    } else if key.eq_ignore_ascii_case(b"filename*") {
        *filename_ext = Some(value.to_vec());
    } else if key.eq_ignore_ascii_case(b"filename") && filename.is_none() {
        *filename = Some(value.to_vec());
    }
}

/// `application/x-www-form-urlencoded` body -> (name, value) pairs with '+'/percent decoding.
fn scan_urlencoded(body: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    for token in body.split(|b| *b == b'&') {
        if token.is_empty() {
            continue;
        }
        let eq = token.iter().position(|b| *b == b'=');
        let (k, v) = match eq {
            Some(e) => (&token[..e], &token[e + 1..]),
            None => (token, &token[token.len()..]),
        };
        let mut kb = Vec::new();
        let mut vb = Vec::new();
        percent_decode(k, true, &mut kb);
        percent_decode(v, true, &mut vb);
        if kb.is_empty() {
            continue;
        }
        out.push((kb, vb));
    }
    out
}

/// Native entry point: `{ ok, kind, message, parts: [{ name, isFile, filename, contentType,
/// value | bytes }] }`. `bytes` MOVES each file slice straight into its typed array view.
fn form_parse_impl<'js>(ctx: &Ctx<'js>, content_type: String, data: Value<'js>) -> Result<Object<'js>> {
    let obj = Object::new(ctx.clone())?;
    let parts_arr = Array::new(ctx.clone())?;
    let lowered = content_type.to_ascii_lowercase();
    let kind_res = if lowered.contains("multipart/form-data") {
        match ct_param(&content_type, "boundary") {
            Some(b) => {
                let scanned = with_borrowed_bytes(&data, |body| scan_multipart(body, &b))?;
                match scanned {
                    Some(parts) => Ok(("multipart", parts)),
                    None => Err(format!("multipart/form-data body contains no {content_type:?} boundary line")),
                }
            }
            None => Err("multipart/form-data response has no boundary parameter".to_string()),
        }
    } else if lowered.contains("application/x-www-form-urlencoded") {
        let pairs = with_borrowed_bytes(&data, scan_urlencoded)?;
        let parts = pairs
            .into_iter()
            .map(|(k, v)| FormPart {
                name: String::from_utf8_lossy(&k).into_owned(),
                filename: None,
                content_type: None,
                body: v,
            })
            .collect();
        Ok(("urlencoded", parts))
    } else {
        Err(format!(
            "Response is not form data (content-type: {content_type:?})"
        ))
    };
    match kind_res {
        Ok((kind, parts)) => {
            obj.set("ok", true)?;
            obj.set("kind", kind)?;
            obj.set("message", Value::new_null(ctx.clone()))?;
            for (i, p) in parts.into_iter().enumerate() {
                let e = Object::new(ctx.clone())?;
                let is_file = p.filename.is_some();
                e.set("name", p.name.as_str())?;
                e.set("isFile", is_file)?;
                match &p.filename {
                    Some(f) => {
                        e.set("filename", f.as_str())?;
                        e.set(
                            "contentType",
                            p.content_type.as_deref().unwrap_or("application/octet-stream"),
                        )?;
                        e.set("value", Value::new_null(ctx.clone()))?;
                        // One owned slice per file part, MOVED into the view (no second copy).
                        e.set("bytes", vec_to_uint8array(ctx, p.body)?)?;
                    }
                    None => {
                        e.set("filename", Value::new_null(ctx.clone()))?;
                        e.set("contentType", Value::new_null(ctx.clone()))?;
                        e.set("value", String::from_utf8_lossy(&p.body).as_ref())?;
                        e.set("bytes", Value::new_null(ctx.clone()))?;
                    }
                }
                parts_arr.set(i, e)?;
            }
            obj.set("parts", parts_arr)?;
        }
        Err(msg) => {
            obj.set("ok", false)?;
            obj.set("kind", "none")?;
            obj.set("message", msg.as_str())?;
            obj.set("parts", parts_arr)?;
        }
    }
    Ok(obj)
}

// ---------------------------------------------------------------------------
// JS shim: Blob / File / FormData + Response.prototype.blob()/formData()
// ---------------------------------------------------------------------------

const WEB_SHIM: &str = r#"
(function () {
    if (globalThis.__czWebApisInstalled) return;

    function partBytes(p) {
        if (typeof p === "string") return __czUtf8Encode(p);
        if (p instanceof Uint8Array) return p;
        if (p instanceof ArrayBuffer) return new Uint8Array(p);
        if (typeof ArrayBuffer !== "undefined" && ArrayBuffer.isView(p)) {
            return new Uint8Array(p.buffer, p.byteOffset, p.byteLength);
        }
        throw new TypeError("Blob parts must be strings, Blob/ArrayBufferView or ArrayBuffer");
    }

    // One concatenation pass for all parts (cold construction path).
    function concatParts(parts) {
        const views = [];
        let total = 0;
        for (const p of parts) {
            const v = p instanceof Blob ? p._bytes : partBytes(p);
            views.push(v);
            total += v.length;
        }
        const out = new Uint8Array(total);
        let off = 0;
        for (const v of views) { out.set(v, off); off += v.length; }
        return out;
    }

    function Blob(parts, options) {
        if (parts === undefined || parts === null) parts = [];
        if (typeof parts !== "object" || typeof parts.length !== "number") {
            throw new TypeError("Blob constructor requires a sequence of BlobParts");
        }
        this._bytes = concatParts(parts);
        this.type = options && options.type !== undefined && options.type !== null
            ? String(options.type).toLowerCase() : "";
    }
    Object.defineProperty(Blob.prototype, "size", { get: function () { return this._bytes.length; } });
    Blob.prototype.arrayBuffer = async function () {
        // Phase 15B: ALWAYS hand out a fresh copy — returning the Blob's own ArrayBuffer would
        // let a caller mutate the Blob's internal bytes through the returned buffer.
        const b = this._bytes;
        const copy = new ArrayBuffer(b.length);
        new Uint8Array(copy).set(b, 0);
        return copy;
    };
    Blob.prototype.bytes = async function () {
        const copy = new Uint8Array(this._bytes.length);
        copy.set(this._bytes, 0);
        return copy;
    };
    Blob.prototype.text = async function () { return __czUtf8Decode(this._bytes); };
    Blob.prototype.slice = function (start, end, contentType) {
        const size = this._bytes.length;
        let s = start === undefined ? 0 : Math.trunc(Number(start));
        let e = end === undefined ? size : Math.trunc(Number(end));
        if (!Number.isFinite(s)) s = s < 0 ? 0 : size;
        if (!Number.isFinite(e)) e = e < 0 ? 0 : size;
        if (s < 0) s = Math.max(0, size + s); else if (s > size) s = size;
        if (e < 0) e = Math.max(0, size + e); else if (e > size) e = size;
        if (e < s) e = s;
        const b = Object.create(Blob.prototype); // VIEW only — no byte copy
        b._bytes = this._bytes.subarray(s, e);
        b.type = contentType === undefined || contentType === null ? "" : String(contentType).toLowerCase();
        return b;
    };

    function File(bits, name, options) {
        Blob.call(this, bits === undefined || bits === null ? [] : bits, options);
        if (name === undefined || name === null) throw new TypeError("File requires a name");
        this.name = String(name);
        let lm = options && options.lastModified !== undefined ? Number(options.lastModified) : NaN;
        if (!Number.isFinite(lm)) lm = Math.floor(Date.now());
        this.lastModified = lm;
    }
    Object.setPrototypeOf(File.prototype, Blob.prototype);

    function FormData(init) {
        this._entries = [];
        if (init instanceof FormData) {
            for (const e of init._entries) this._entries.push([e[0], e[1], e[2]]);
        } else if (init !== undefined && init !== null && typeof init === "object") {
            const keys = Object.keys(init);
            for (const k of keys) this.append(k, init[k]);
        }
    }
    function fileish(v) { return v instanceof Blob; }
    function formValue(v) { return typeof v === "string" ? v : String(v); }
    FormData.prototype.append = function (name, value, filename) {
        if (filename !== undefined && fileish(value) && !(value instanceof File)) {
            value = new File([value], String(filename), { type: value.type });
        }
        this._entries.push([String(name), value, filename === undefined ? undefined : String(filename)]);
    };
    FormData.prototype.set = function (name, value, filename) {
        const n = String(name);
        if (filename !== undefined && fileish(value) && !(value instanceof File)) {
            value = new File([value], String(filename), { type: value.type });
        }
        let replaced = false;
        const keep = [];
        for (const e of this._entries) {
            if (e[0] === n) { if (!replaced) { keep.push([n, value, filename === undefined ? undefined : String(filename)]); replaced = true; } }
            else keep.push(e);
        }
        if (!replaced) keep.push([n, value, filename === undefined ? undefined : String(filename)]);
        this._entries = keep;
    };
    function entryValue(e) {
        if (!fileish(e[1])) return e[1];
        if (e[2] !== undefined && !(e[1] instanceof File)) {
            return new File([e[1]], e[2], { type: e[1].type });
        }
        return e[1];
    }
    FormData.prototype.get = function (name) {
        const n = String(name);
        for (const e of this._entries) if (e[0] === n) return entryValue(e);
        return null;
    };
    FormData.prototype.getAll = function (name) {
        const n = String(name);
        const out = [];
        for (const e of this._entries) if (e[0] === n) out.push(entryValue(e));
        return out;
    };
    FormData.prototype.has = function (name) {
        const n = String(name);
        for (const e of this._entries) if (e[0] === n) return true;
        return false;
    };
    FormData.prototype.delete = function (name) {
        const n = String(name);
        const keep = [];
        for (const e of this._entries) if (e[0] !== n) keep.push(e);
        this._entries = keep;
    };
    FormData.prototype.forEach = function (cb, thisArg) {
        const snap = [];
        for (const e of this._entries) snap.push([entryValue(e), String(e[0])]);
        for (const p of snap) cb.call(thisArg === undefined ? this : thisArg, p[0], p[1], this);
    };
    FormData.prototype.entries = function () {
        const snap = [];
        for (const e of this._entries) snap.push([String(e[0]), entryValue(e)]);
        return snap[Symbol.iterator]();
    };
    FormData.prototype.keys = function () {
        const snap = [];
        for (const e of this._entries) snap.push(String(e[0]));
        return snap[Symbol.iterator]();
    };
    FormData.prototype.values = function () {
        const snap = [];
        for (const e of this._entries) snap.push(entryValue(e));
        return snap[Symbol.iterator]();
    };
    FormData.prototype[Symbol.iterator] = function () { return this.entries(); };

    // WHATWG sanitizes quotes/backslashes/CR/LF out of field & file names.
    function sanitize(s) { return String(s).replace(/["\\\r\n]/g, "%22"); }

    // Serialize multipart/form-data: header strings encode natively; all chunks concat ONCE.
    FormData.prototype._serialize = function (boundary) {
        const chunks = [];
        let total = 0;
        const push = function (v) {
            let u;
            if (typeof v === "string") u = __czUtf8Encode(v);
            else if (v instanceof Uint8Array) u = v;
            else u = new Uint8Array(v.buffer !== undefined ? v.buffer : v);
            chunks.push(u);
            total += u.length;
        };
        for (const e of this._entries) {
            const v = entryValue(e);
            let head = "--" + boundary + "\r\nContent-Disposition: form-data; name=\"" + sanitize(e[0]) + "\"";
            if (fileish(v)) {
                const fname = e[2] !== undefined ? e[2] : (v.name !== undefined ? v.name : "blob");
                head += "; filename=\"" + sanitize(fname) + "\"";
                // Phase 15B: strip CR/LF out of the blob's Content-Type so a hostile `type`
                // can never inject extra multipart header lines (CRLF injection guard).
                const ctype = String(v.type || "").replace(/[\r\n]/g, "");
                if (ctype) head += "\r\nContent-Type: " + ctype;
            }
            head += "\r\n\r\n";
            push(head);
            push(fileish(v) ? v._bytes : String(v));
            push("\r\n");
        }
        push("--" + boundary + "--\r\n");
        const out = new Uint8Array(total);
        let off = 0;
        for (const c of chunks) { out.set(c, off); off += c.length; }
        return out;
    };

    globalThis.__czMultipartBoundary = function () {
        return "----CzForm" + Math.random().toString(36).slice(2, 10) + Date.now().toString(36);
    };
    // Zero-copy Blob wrapper over already-owned bytes (Response.blob() fast path).
    globalThis.__czBlobFromBytes = function (bytes, type) {
        const b = Object.create(Blob.prototype);
        b._bytes = bytes;
        b.type = String(type || "");
        return b;
    };
    globalThis.__czFormFile = function (bytes, name, type, lastModified) {
        const f = Object.create(File.prototype);
        f._bytes = bytes;
        f.type = String(type || "");
        f.name = String(name);
        f.lastModified = lastModified === undefined ? Math.floor(Date.now()) : lastModified;
        return f;
    };

    globalThis.Blob = Blob;
    globalThis.File = File;
    globalThis.FormData = FormData;
    globalThis.atob = function (b64) { return __czAtob(String(b64)); };
    globalThis.btoa = function (bin) { return __czBtoa(String(bin)); };

    // Response integration lives here (uses the public bytes()/headers surface).
    if (typeof globalThis.Response === "function") {
        globalThis.Response.prototype.blob = async function () {
            const bytes = await this.bytes();
            return globalThis.__czBlobFromBytes(bytes, this.headers.get("content-type") || "");
        };
        globalThis.Response.prototype.formData = async function () {
            const bytes = await this.bytes();
            const ct = this.headers.get("content-type") || "";
            const d = __czFormParse(ct, bytes);
            if (!d.ok) throw new TypeError(d.message);
            const fd = new FormData();
            for (const p of d.parts) {
                if (p.isFile) fd.append(p.name, globalThis.__czFormFile(p.bytes, p.filename, p.contentType));
                else fd.append(p.name, p.value);
            }
            return fd;
        };
    }

    globalThis.__czWebApisInstalled = true;
})();
"#;

/// Register the Web API natives + Blob/File/FormData/atob/btoa globals on a context
/// (idempotent). MUST run after [`install_fetch`] so `Response` exists to patch. Also installs
/// fetch itself when absent so standalone embedders get the full surface atomically.
pub fn install_web<'js>(ctx: &Ctx<'js>) -> Result<()> {
    if ctx.globals().get::<_, bool>("__czWebApisInstalled").unwrap_or(false) {
        return Ok(());
    }
    // UTF-8 native codecs come with the fetch shim; ensure they exist for Blob.text() etc.
    install_fetch(ctx)?;
    ctx.globals().set(
        "__czBtoa",
        Function::new(ctx.clone(), |ctx: Ctx<'js>, input: String| -> Result<String> {
            btoa_impl(&ctx, input)
        })?,
    )?;
    ctx.globals().set(
        "__czAtob",
        Function::new(ctx.clone(), |ctx: Ctx<'js>, input: String| -> Result<String> {
            atob_impl(&ctx, input)
        })?,
    )?;
    ctx.globals().set(
        "__czFormParse",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, content_type: String, data: Value<'js>| -> Result<Object<'js>> {
                form_parse_impl(&ctx, content_type, data)
            },
        )?,
    )?;
    ctx.eval::<(), _>(WEB_SHIM)?;
    Ok(())
}

#[cfg(test)]
mod web_tests {
    use super::*;
    use rquickjs::{CatchResultExt, CaughtError, Context, Runtime};

    /// Async-aware script runner: bodies end with `return "MARKER"`; the promise settles via
    /// pending jobs drained OUTSIDE any active `Context::with` scope (RefCell reentrancy rule).
    fn run(source: &str) -> String {
        let rt = Runtime::new().unwrap();
        let ctx = Context::full(&rt).unwrap();
        let wrapped = "(async function () { \n"
            .to_string()
            + source
            + "\n})().then((v) => { globalThis.__R = String(v); })"
            + ".catch((e) => { globalThis.__R = 'FAIL: ' + (e && e.message ? e.message : String(e)) + ' @ ' + (e && e.lineNumber ? e.lineNumber : '?'); });";
        ctx.with(|ctx| -> () {
            install_web(&ctx).unwrap();
            match ctx.eval::<(), _>(wrapped.as_str()).catch(&ctx) {
                Ok(()) => {}
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
        ctx.with(|ctx| -> String {
            ctx.globals().get::<_, String>("__R").ok().unwrap_or_else(|| "MISSING".into())
        })
    }

    #[test]
    fn atob_btoa_match_html_latin1_semantics() {
        assert_eq!(run(r#"
            if (btoa("CardputerZero!") !== "Q2FyZHB1dGVyWmVybyE=") throw new Error("btoa failed");
            if (atob("Q2FyZHB1dGVyWmVybyE=") !== "CardputerZero!") throw new Error("atob failed");
            if (btoa("") !== "" || atob("") !== "") throw new Error("empty edge");
            // binary-string round trip keeps bytes 0x80..0xFF as single code units
            const bin = String.fromCharCode(0, 255, 128);
            if (atob(btoa(bin)) !== bin) throw new Error("latin1 roundtrip");
            let threw = false;
            try { btoa("\u{FFFE}"); } catch (e) { threw = true; }
            if (!threw) throw new Error(">255 must throw");
            threw = false;
            try { atob("!!!!"); } catch (e) { threw = true; }
            if (!threw) throw new Error("invalid b64 must throw");
            return "B64_OK"
        "#), "B64_OK");
    }

    #[test]
    fn blob_file_slice_and_iteration_surface() {
        assert_eq!(run(r#"
            const b = new Blob(["hello ", new Uint8Array([119,111,114,108,100])], { type: "text/plain" });
            if (b.size !== 11 || b.type !== "text/plain") throw new Error("meta");
            if (await b.text() !== "hello world") throw new Error("text");
            if (await b.slice(0, 5).text() !== "hello") throw new Error("slice text");
            if (b.slice(-5).size !== 5) throw new Error("negative slice");
            const ab = await b.arrayBuffer();
            if (!(ab instanceof ArrayBuffer) || ab.byteLength !== 11) throw new Error("arrayBuffer");
            const empty = new Blob();
            if (empty.size !== 0) throw new Error("default parts");
            const f = new File(["file-body"], "note.txt", { type: "text/plain", lastModified: 1234 });
            if (!(f instanceof Blob) || !(f instanceof File)) throw new Error("File chain");
            if (f.name !== "note.txt" || f.lastModified !== 1234 || await f.text() !== "file-body") throw new Error("File meta");

            const fd = new FormData();
            fd.append("field", "v1");
            fd.append("field", "v2");
            fd.set("single", "ok");
            fd.append("upload", f);
            if (fd.get("field") !== "v1") throw new Error("get");
            if (fd.getAll("field").length !== 2) throw new Error("getAll");
            if (!fd.has("upload") || fd.has("nope")) throw new Error("has");
            let seen = 0;
            fd.forEach(function (v, n) { seen++; if (typeof n !== "string") throw new Error("forEach name"); });
            // Two field entries + single + upload => 4 live entries visited.
            if (seen !== 4) throw new Error("forEach count " + seen);
            const ks = [...fd.keys()];
            if (ks.join(",") !== "field,field,single,upload") throw new Error("keys " + ks.join(","));
            fd.delete("field");
            if (fd.has("field")) throw new Error("delete");
            return "BLOBFORM_OK"
        "#), "BLOBFORM_OK");
    }

    #[test]
    fn multipart_serializer_and_native_parser_roundtrip() {
        assert_eq!(run(r#"
            const fd = new FormData();
            fd.append("title", "demo");
            fd.append("doc", new File(["hello-file-content"], "hello.txt", { type: "text/plain" }));
            const boundary = "----CzTestBoundary42";
            const wire = fd._serialize(boundary);
            const d = __czFormParse("multipart/form-data; boundary=" + boundary, wire);
            if (!d.ok) throw new Error("parse failed: " + d.message);
            if (d.kind !== "multipart") throw new Error("kind");
            if (d.parts.length !== 2) throw new Error("parts " + d.parts.length);
            if (d.parts[0].name !== "title" || d.parts[0].value !== "demo" || d.parts[0].isFile) throw new Error("field");
            const doc = d.parts[1];
            if (!doc.isFile || doc.filename !== "hello.txt" || doc.contentType !== "text/plain") throw new Error("file meta");
            if (new TextDecoder().decode(doc.bytes) !== "hello-file-content") throw new Error("file body");

            const du = __czFormParse("application/x-www-form-urlencoded;charset=UTF-8",
                __czUtf8Encode("a=1+b%26c&d=%E6%97%A5"));
            if (!du.ok || du.kind !== "urlencoded" || du.parts.length !== 2) throw new Error("urlenc");
            // '+' decodes to SPACE in urlencoded bodies: "1+b%26c" -> "1 b&c".
            if (du.parts[0].value !== "1 b&c") throw new Error("plus/percent " + du.parts[0].value);
            if (du.parts[1].value !== "日") throw new Error("utf8 percent decode");

            const dbad = __czFormParse("text/plain", __czUtf8Encode("x"));
            if (dbad.ok !== false || typeof dbad.message !== "string") throw new Error("bad ct descriptor");
            return "FORMPARSE_OK"
        "#), "FORMPARSE_OK");
    }

    #[test]
    fn quoted_boundary_and_extended_filename_parse() {
        assert_eq!(run(r#"
            const wire = __czUtf8Encode("--Q\r\nContent-Disposition: form-data; name=\"a\"; filename=\"he%20llo.txt\"\r\nContent-Type: text/plain\r\n\r\nX\r\n--Q--\r\n");
            const d = __czFormParse("multipart/form-data; boundary=\"Q\"", wire);
            if (!d.ok) throw new Error("quoted boundary parse");
            if (d.parts.length !== 1 || d.parts[0].filename !== "he%20llo.txt") throw new Error("plain filename keeps % per WHATWG legacy");
            const wire2 = __czUtf8Encode("--R\r\nContent-Disposition: form-data; name=\"f\"; filename*=UTF-8''caf%C3%A9.txt\r\n\r\nZ\r\n--R--\r\n");
            const d2 = __czFormParse("multipart/form-data; boundary=R", wire2);
            if (!d2.ok || d2.parts[0].filename !== "café.txt") throw new Error("extended filename " + d2.parts[0].filename);
            return "EXT_OK"
        "#), "EXT_OK");
    }

    #[test]
    fn ct_param_and_scan_helpers_ground_truth() {
        assert_eq!(ct_param("multipart/form-data; Boundary=AbC", "boundary").as_deref(), Some("AbC"));
        assert_eq!(ct_param("text/plain; boundary=\"x;y\"", "boundary").as_deref(), Some("x;y"));
        assert_eq!(ct_param("text/plain", "boundary"), None);
        let body = b"--X\r\nContent-Disposition: form-data; name=\"k\"\r\n\r\nv\r\n--X--\r\n";
        let parts = scan_multipart(body, "X").expect("scan");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].name, "k");
        assert_eq!(parts[0].body, b"v".to_vec());
        assert!(parts[0].filename.is_none());
        let pairs = scan_urlencoded(b"a=1&b=two&nokey=%41");
        assert_eq!(pairs.len(), 3);
    }
}
