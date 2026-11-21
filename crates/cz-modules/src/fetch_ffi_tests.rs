//! Phase 10C unit tests for the native FFI surface installed by `install_fetch`: single-pass UTF-8
//! encode/decode (the char-by-char JavaScript loop is gone), buffer-kind tolerance (`Uint8Array`,
//! `ArrayBuffer`, `DataView`, `subarray` views), `fatal` vs lossy decoding, direct `Response.json()`
//! byte entry into cz-json, and the clone-aware body consumers.

use crate::fetch_module::{install_fetch, FETCH_SHIM};
use rquickjs::{CatchResultExt, CaughtError, Context, Runtime};

/// Install the fetch primitives + shim in a fresh runtime and evaluate one script. No network
/// worker is started, so this stays hermetic.
fn run_with_fetch(source: &str) -> String {
    let rt = Runtime::new().unwrap();
    let ctx = Context::full(&rt).unwrap();
    let out = ctx.with(|ctx| -> String {
        install_fetch(&ctx).unwrap();
        match ctx.eval::<String, _>(source).catch(&ctx) {
            Ok(text) => text,
            Err(err) => {
                let msg = match &err {
                    CaughtError::Exception(ex) => {
                        ex.message().unwrap_or_else(|| "unknown exception".to_string())
                    }
                    other => format!("{other}"),
                };
                panic!("script failed: {msg}");
            }
        }
    });
    while let Ok(true) = rt.execute_pending_job() {}
    out
}

#[test]
fn native_utf8_codec_is_byte_exact_for_multibyte_bom_and_fatal_input() {
    assert_eq!(
        run_with_fetch(
            r#"
            const enc = new TextEncoder();
            const dec = new TextDecoder();
            const text = "CardputerZero 日本語 🚀";
            const bytes = enc.encode(text);
            if (!(bytes instanceof Uint8Array)) throw new Error("encode must yield Uint8Array");
            if (bytes.length !== 28) throw new Error("utf8 length " + bytes.length); // 14 ascii + 9 + 4 + surrogate-pair counted exactly
            if (dec.decode(bytes) !== text) throw new Error("roundtrip mismatch");
            if (enc.encode("é").length !== 2) throw new Error("two-byte sequence");

            // BOM handling: stripped by default (view only, no copy), preserved with ignoreBOM.
            const bom = new Uint8Array([0xef, 0xbb, 0xbf, 0x61]);
            if (dec.decode(bom) !== "a") throw new Error("bom not stripped");
            if (new TextDecoder("utf-8", { ignoreBOM: true }).decode(bom) !== "\uFEFFa") throw new Error("ignoreBOM");

            // fatal -> a real TypeError; the default mode mirrors Node's U+FFFD replacement.
            let threw = false;
            try { new TextDecoder("utf-8", { fatal: true }).decode(new Uint8Array([0xc3, 0x28])); }
            catch (e) { threw = e instanceof TypeError; }
            if (!threw) throw new Error("fatal must throw TypeError");
            if (dec.decode(new Uint8Array([0xc3, 0x28])) !== "\uFFFD(") throw new Error("lossy shape");

            // Buffer kinds all decode natively (Uint8Array views + ArrayBuffer + DataView).
            const ab = new ArrayBuffer(6);
            new Uint8Array(ab).set([0x68, 0x69, 0x21, 0xe6, 0x97, 0xa5]);
            if (dec.decode(ab) !== "hi!日") throw new Error("ArrayBuffer decode");
            if (new TextDecoder().decode(new Uint8Array(ab, 0, 2)) !== "hi") throw new Error("view decode");
            if (new TextDecoder().decode(new DataView(ab, 0, 2)) !== "hi") throw new Error("DataView decode");

            // encodeInto bulk-copies through a destination view instead of writing byte-by-byte.
            const dst = new Uint8Array(5);
            const res = enc.encodeInto("abcé", dst);
            if (res.written !== 5 || dst[0] !== 97 || dst[4] !== 0xa9) throw new Error("encodeInto " + JSON.stringify(res));
            if (__czUtf8Encode("héllo").length !== 6) throw new Error("native encode length");
            "UTF8_NATIVE_OK"
        "#
        ),
        "UTF8_NATIVE_OK"
    );
}

#[test]
fn shim_wires_native_codecs_direct_json_byte_entry_and_clone_aware_body_moves() {
    // Structural contract (mirrors the shared Phase 10C harness): char-by-char decoding is gone.
    assert!(!FETCH_SHIM.contains("parts.push(String.fromCharCode("));
    assert!(FETCH_SHIM.contains("__czUtf8Encode("), "native encoder wiring");
    assert!(FETCH_SHIM.contains("__czUtf8Decode("), "native decoder wiring");
    assert!(FETCH_SHIM.contains("__czJsonParseBytes(bytes)"), "Response.json byte entry");
    assert!(FETCH_SHIM.contains("this._shared"), "clone tracking drives defensive copies");

    // The natives really exist after install, and their error behavior matches Web/Node.
    assert_eq!(
        run_with_fetch(
            r#"
            const kinds = [typeof __czUtf8Encode, typeof __czUtf8Decode, typeof __czJsonParseBytes];
            if (kinds.join(",") !== "function,function,function") throw new Error("natives " + kinds.join(","));

            const small = __czUtf8Encode('{"a":7,"arr":[1,2]}');
            const obj = __czJsonParseBytes(small);
            if (obj.a !== 7 || obj.arr[1] !== 2 || Array.isArray(obj.arr) !== true) throw new Error("dom parse");
            if (__czJsonParseBytes('{"b":3}').b !== 3) throw new Error("string fallback path");

            let bad = false;
            try { __czJsonParseBytes(__czUtf8Encode("{oops")); } catch (e) { bad = e instanceof SyntaxError; }
            if (!bad) throw new Error("invalid bytes must throw SyntaxError");
            let wrong = false;
            try { __czJsonParseBytes(42); } catch (e) { wrong = e instanceof TypeError; }
            if (!wrong) throw new Error("non-buffer input must throw TypeError");

            // Clone-aware consumers: a fresh Response is unshared, cloning marks both sides.
            const r = new Response(new Uint8Array([1, 2, 3]));
            if (r._shared !== false) throw new Error("fresh Response must not be marked shared");
            const c = r.clone();
            if (c._shared !== true || r._shared !== true) throw new Error("clone must mark both sides");
            "NATIVES_OK"
        "#
        ),
        "NATIVES_OK"
    );
}

/// Phase 14 guard: manually constructed `Response`/`Request` bodies must flow through the same
/// native consumers as engine responses. Setup kicks off async consumption, QuickJS jobs drain
/// OUTSIDE the context scope (RefCell rule), then a second script asserts every result.
fn run_manual(setup: &str, check: &str) -> String {
    let rt = Runtime::new().unwrap();
    let ctx = Context::full(&rt).unwrap();
    ctx.with(|ctx| {
        install_fetch(&ctx).unwrap();
        if let Err(err) = ctx.eval::<(), _>(setup).catch(&ctx) {
            panic!("setup failed: {err}");
        }
    });
    while let Ok(true) = rt.execute_pending_job() {}
    ctx.with(|ctx| -> String {
        match ctx.eval::<String, _>(check).catch(&ctx) {
            Ok(text) => text,
            Err(err) => {
                let msg = match &err {
                    CaughtError::Exception(ex) => ex.message().unwrap_or_else(|| "unknown exception".to_string()),
                    other => format!("{other}"),
                };
                panic!("manual consumer check failed: {msg}");
            }
        }
    })
}

#[test]
fn manual_response_and_request_consumers_match_web_semantics() {
    let setup = r#"
        (async () => {
            // Hono-style manual Response: string body + init status/statusText/headers.
            const r = new Response('{"ok":true,"runtime":"cardputer-zero-llrt"}',
                { status: 201, statusText: 'Created', headers: { 'content-type': 'application/json' } });
            globalThis.__shape = [r.status, String(r.ok), r.statusText, r.headers.get('content-type')].join('|');
            const j = await r.json();
            globalThis.__json = j.ok === true && j.runtime === 'cardputer-zero-llrt';
            // WinterTC defaults: a no-arg manual Response is status 200 with an empty body.
            const e = new Response();
            globalThis.__empty = [e.status, await e.text(), e.headers.get('content-type') === null].join('|');
            // Bytes bodies keep flowing untouched, and clone() shares the buffered view.
            const b = new Response(new Uint8Array([104, 105, 33]));
            globalThis.__bytesText = await b.clone().text();
            globalThis.__bytesShared = b._shared === true;
            // Manual Request consumers (`c.req.json()`/`text()`/`arrayBuffer()` reach raw).
            const reqS = new Request('http://x/y', { method: 'POST', body: '{"b":8}' });
            globalThis.__reqJson = (await reqS.json()).b;
            globalThis.__reqText = await reqS.text();
            const reqB = new Request('http://x/z', { method: 'POST', body: new Uint8Array([123, 34, 97, 34, 58, 55, 125]) });
            globalThis.__reqBytesJson = (await reqB.json()).a;
            globalThis.__reqAb = (await reqB.arrayBuffer()).byteLength;
            globalThis.__noBodyText = await new Request('http://x/n').text();
            globalThis.__done = true;
        })().catch((err) => { globalThis.__err = String((err && err.message) || err); globalThis.__done = true; });
    "#;
    let check = r#"
        if (globalThis.__err !== undefined) throw new Error('async: ' + globalThis.__err + ' steps='+[globalThis.__s1,globalThis.__s2,globalThis.__s3,globalThis.__s4,globalThis.__s5,globalThis.__s6].join(';'));
        if (globalThis.__done !== true) throw new Error('consumers hung');
        if (globalThis.__shape !== '201|true|Created|application/json') throw new Error('shape ' + globalThis.__shape);
        if (globalThis.__json !== true) throw new Error('manual json');
        if (globalThis.__empty !== '200||true') throw new Error('empty shape ' + globalThis.__empty);
        if (globalThis.__bytesText !== 'hi!') throw new Error('clone text ' + globalThis.__bytesText);
        if (globalThis.__bytesShared !== true) throw new Error('clone must mark both sides');
        if (globalThis.__reqJson !== 8) throw new Error('request json');
        if (globalThis.__reqText !== '{"b":8}') throw new Error('request text');
        if (globalThis.__reqBytesJson !== 7) throw new Error('request bytes json');
        if (globalThis.__reqAb !== 7) throw new Error('request arrayBuffer ' + globalThis.__reqAb);
        if (globalThis.__noBodyText !== '') throw new Error('no-body text');
        'MANUAL_CONSUMERS_OK'
    "#;
    assert_eq!(run_manual(setup, check), "MANUAL_CONSUMERS_OK");
}
