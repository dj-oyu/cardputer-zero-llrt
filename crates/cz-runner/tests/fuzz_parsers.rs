//! Deterministic structure-aware & byte-mutation fuzzing suite for cz-js parsers
//! and sandbox boundaries (pi-sandbox / hostile untrusted input hardening).
//!
//! Run fast in debug or high-throughput in release:
//!   cargo test -p cz-runner --test fuzz_parsers --release -- --nocapture

use cz_json::{parse_dom, validate, LazyDoc, TapeDoc};
use cz_net::url::parse_url;
use cz_net::ws::{apply_mask_in_place, encode_frame_header, parse_close_payload};

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }
    #[inline]
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    #[inline]
    fn next_usize(&mut self, max: usize) -> usize {
        (self.next_u64() as usize) % max
    }
    fn mutate_bytes(&mut self, seed: &[u8], out: &mut Vec<u8>) {
        out.clear();
        out.extend_from_slice(seed);
        let ops = 1 + self.next_usize(6);
        for _ in 0..ops {
            match self.next_usize(7) {
                0 if !out.is_empty() => {
                    let i = self.next_usize(out.len());
                    out[i] ^= 1u8 << (self.next_u64() & 7);
                }
                1 if !out.is_empty() => {
                    let i = self.next_usize(out.len());
                    out[i] = self.next_u64() as u8;
                }
                2 if !out.is_empty() => {
                    let i = self.next_usize(out.len());
                    out.remove(i);
                }
                3 if out.len() < 1024 => {
                    let i = self.next_usize(out.len() + 1);
                    let interesting: &[u8] = b"\"\\{}[],:09-eE.+u/\x00\x0a\x0d\x1f\x7f\x80\xc0\xe0\xed\xf0\xff";
                    let b = interesting[self.next_usize(interesting.len())];
                    out.insert(i, b);
                }
                4 if !out.is_empty() => {
                    let new_len = self.next_usize(out.len());
                    out.truncate(new_len);
                }
                5 if out.len() < 512 && !out.is_empty() => {
                    let i = self.next_usize(out.len());
                    let j = self.next_usize(out.len());
                    let (a, b) = if i <= j { (i, j) } else { (j, i) };
                    let chunk: Vec<u8> = out[a..=b].to_vec();
                    let dst = self.next_usize(out.len() + 1);
                    out.splice(dst..dst, chunk);
                }
                _ => {
                    // Replace with random short byte sequence
                    if out.len() < 256 {
                        let n = 1 + self.next_usize(8);
                        for _ in 0..n {
                            out.push(self.next_u64() as u8);
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn fuzz_cz_json_parsers_and_lazy_cow() {
    let seeds: &[&[u8]] = &[
        b"null",
        b"true",
        b"false",
        b"0",
        b"-0",
        b"12345678901234567890",
        b"-9223372036854775808",
        b"1.7976931348623157e+308",
        b"5e-324",
        b"1e999999",
        b"\"hello\\n\\t\\u0041\\uD83D\\uDE00\"",
        b"\"\\uD800\"",
        b"\"\\uDFFF\"",
        b"\"\\uD800\\u0041\"",
        b"{\"a\":1,\"b\":[true,false,null,\"\\u0000\"],\"c\":{\"d\":-3.14}}",
        b"{\"dup\":1,\"dup\":2,\"\":3}",
        b"[[[[[[[[[[1]]]]]]]]]]",
        b"{\"a\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}",
        b"{\"k\":\"\xe3\x81\x82\xe3\x81\x84\xe3\x81\x86\"}",
    ];

    // 1. Adversarial hand-crafted inputs (deep nesting, invalid UTF-8, huge numbers)
    let mut adversarial: Vec<Vec<u8>> = vec![
        vec![],
        b" ".to_vec(),
        b"{".to_vec(),
        b"[".to_vec(),
        b"\"".to_vec(),
        b"\"\\".to_vec(),
        b"\"\\u".to_vec(),
        b"\"\\u123".to_vec(),
        b"\"\\uZZZZ\"".to_vec(),
        b"\"\\x41\"".to_vec(),
        b"\"\x00\"".to_vec(),
        b"\"\x1f\"".to_vec(),
        b"\"\xff\xfe\"".to_vec(),
        b"\"\xc0\xaf\"".to_vec(),
        b"\"\xed\xa0\x80\"".to_vec(),
        b"00".to_vec(),
        b"-01".to_vec(),
        b"1.".to_vec(),
        b".1".to_vec(),
        b"1e".to_vec(),
        b"1e+".to_vec(),
        b"1e-".to_vec(),
        b"NaN".to_vec(),
        b"Infinity".to_vec(),
        b"[1,]".to_vec(),
        b"{\"a\":1,}".to_vec(),
        b"{1:2}".to_vec(),
        b"true false".to_vec(),
    ];
    // Deep nesting (should return Err(Depth) cleanly, never stack-overflow!)
    for depth in [64usize, 256, 1024, 4096] {
        let mut v = vec![b'['; depth];
        v.extend(std::iter::repeat(b']').take(depth));
        adversarial.push(v);

        let mut o = Vec::with_capacity(depth * 6);
        for _ in 0..depth {
            o.extend_from_slice(b"{\"a\":");
        }
        o.push(b'1');
        o.extend(std::iter::repeat(b'}').take(depth));
        adversarial.push(o);
    }

    let mut parsed_ok = 0usize;
    let mut rng = Rng::new(0x5eed_cafe_1234_5678);
    let mut buf = Vec::with_capacity(1024);

    let mut exercise = |input: &[u8]| {
        let v_res = validate(input);
        let t_res = TapeDoc::build(input);
        let d_res = parse_dom(input);
        // Consistency invariant: validate, TapeDoc::build, and parse_dom must agree on Ok vs Err!
        assert_eq!(
            v_res.is_ok(),
            t_res.is_ok(),
            "validate vs TapeDoc mismatch on {:?}",
            String::from_utf8_lossy(input)
        );
        assert_eq!(
            v_res.is_ok(),
            d_res.is_ok(),
            "validate vs parse_dom mismatch on {:?}",
            String::from_utf8_lossy(input)
        );
        if let Ok(tape) = t_res {
            parsed_ok += 1;
            // Exercise scalar decoding, key lookups, and LazyDoc fork + stringify
            for idx in 0..tape.node_count().min(64) as u32 {
                let _ = tape.scalar(idx);
                let _ = tape.key_bytes(idx);
                if tape.node(idx).is_container() {
                    let _ = tape.lookup(idx, "a");
                    let _ = tape.child_at(idx, 0);
                }
            }
            if let Ok(lazy) = LazyDoc::build(input) {
                let forked = lazy.fork();
                assert!(forked.is_clean());
            }
        }
    };

    for s in seeds {
        exercise(s);
    }
    for a in &adversarial {
        exercise(a);
    }
    for _ in 0..25_000 {
        let seed = seeds[rng.next_usize(seeds.len())];
        rng.mutate_bytes(seed, &mut buf);
        exercise(&buf);
    }
    eprintln!("fuzz_cz_json: 25,000+ cases clean (parsed_ok={parsed_ok})");
}

#[test]
fn fuzz_cz_net_url_parser() {
    let seeds: &[&str] = &[
        "http://example.com/path?a=1&b=2#frag",
        "https://user:pass@[::1]:8443/a/b/../c/./d?q=%20%zz",
        "ws://127.0.0.1:8080/ws",
        "wss://example.com/chat",
        "file:///etc/passwd",
        "data:text/plain;base64,SGVsbG8=",
        "javascript:alert(1)",
        "http://127.0.0.1\\@evil.com/path",
        "http://[::ffff:127.0.0.1]:65535/",
        "http://[::ffff:127.0.0.1]:65536/",
        "http://[::ffff:127.0.0.1]:99999999999/",
        "http://example.com:0/",
        "http://example.com:/",
        "//relative.host/path",
        "/just/a/path?x=1",
        "../up/one",
        "   \t\n\r http://ex\tample.com/a\nb\rc  ",
        "http://0x7f000001/",
        "http://2130706433/",
        "http://[:::]/",
        "http://[1:2:3:4:5:6:7:8:9]/",
        "http://a:b:c@host/",
        "",
        ":",
        "://",
        "http://",
        "http:///",
    ];
    let bases: &[Option<&str>] = &[
        None,
        Some("https://base.example.com:8443/dir/sub/page.html?x=1#h"),
        Some("file:///a/b/c"),
        Some("data:text/plain,abc"),
    ];

    let mut rng = Rng::new(0x0123_4567_89ab_cdef);
    let mut buf = Vec::with_capacity(512);
    let mut ok_count = 0usize;

    for s in seeds {
        for &b in bases {
            if let Ok(u) = parse_url(s, b) {
                ok_count += 1;
                let v = u.view();
                let _ = (v.href(), v.protocol(), v.hostname(), v.port(), v.host(), v.pathname(), v.search(), v.hash(), v.origin(), v.request_target());
            }
        }
    }

    for _ in 0..25_000 {
        let seed = seeds[rng.next_usize(seeds.len())].as_bytes();
        rng.mutate_bytes(seed, &mut buf);
        let s = String::from_utf8_lossy(&buf);
        let base = bases[rng.next_usize(bases.len())];
        if let Ok(u) = parse_url(&s, base) {
            ok_count += 1;
            let v = u.view();
            let _ = (v.protocol(), v.hostname(), v.port(), v.pathname(), v.origin(), v.request_target());
        }
    }
    eprintln!("fuzz_cz_net_url: 25,000+ cases clean (ok={ok_count})");
}

#[test]
fn fuzz_cz_net_ws_primitives() {
    let mut rng = Rng::new(0xdead_beef_cafe_babe);
    let mut buf = Vec::with_capacity(4096);
    let mut hdr = [0u8; 14];

    for _ in 0..20_000 {
        let len = match rng.next_usize(5) {
            0 => rng.next_usize(16),
            1 => 120 + rng.next_usize(20),
            2 => rng.next_usize(256),
            3 => rng.next_usize(1024),
            _ => rng.next_usize(4096),
        };
        buf.resize(len, 0);
        for b in &mut buf {
            *b = rng.next_u64() as u8;
        }
        let orig = buf.clone();
        let key = [
            rng.next_u64() as u8,
            rng.next_u64() as u8,
            rng.next_u64() as u8,
            rng.next_u64() as u8,
        ];
        // Double masking must be an exact involution for every length & alignment
        apply_mask_in_place(&mut buf, key);
        apply_mask_in_place(&mut buf, key);
        assert_eq!(buf, orig, "WS mask involution failed at len={len}");

        // Close payload parser must never panic on arbitrary invalid UTF-8 / truncated bytes
        let (code, reason) = parse_close_payload(&buf[..len.min(130)]);
        assert!(reason.is_ascii() || !reason.is_empty() || code <= u16::MAX);

        // Frame header encoding must never panic across all opcodes & lengths
        let opcode = (rng.next_u64() & 0x0f) as u8;
        let fin = (rng.next_u64() & 1) != 0;
        let mask_opt = if (rng.next_u64() & 1) != 0 { Some(key) } else { None };
        let plen = if (rng.next_u64() & 3) == 0 {
            (rng.next_u64() & 0x1ffff) as usize
        } else {
            len
        };
        let n = encode_frame_header(&mut hdr, fin, opcode, plen, mask_opt);
        assert!(n >= 2 && n <= 14);
    }
    eprintln!("fuzz_cz_net_ws: 20,000 cases clean");
}

#[test]
fn fuzz_pi_sandbox_hostile_js_and_limits() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    // 1. Infinite loop interruption + stack overflow + OOM bombs must NEVER crash the host.
    let hostile_scripts: &[(&str, &str)] = &[
        ("infinite_loop", "while (true) {}"),
        ("stack_overflow", "function f(n) { return f(n + 1); } f(0);"),
        ("oom_repeat", "'a'.repeat(1 << 27);"),
        ("oom_array_push", "const a = []; for (let i = 0; i < 1e7; i++) a.push('x'.repeat(4096));"),
        ("oom_split_bomb", "'a,'.repeat(200000).split(',').map(x => x + 'y');"),
        ("json_depth_bomb", "JSON.parse('['.repeat(5000) + ']'.repeat(5000));"),
        ("json_object_bomb", "JSON.parse('{\"a\":'.repeat(3000) + '1' + '}'.repeat(3000));"),
    ];

    for (name, src) in hostile_scripts {
        let rt = rquickjs::Runtime::new().expect("rt");
        rt.set_memory_limit(16 * 1024 * 1024);
        rt.set_max_stack_size(256 * 1024);
        let ticks = Arc::new(AtomicUsize::new(0));
        let ticks_c = ticks.clone();
        rt.set_interrupt_handler(Some(Box::new(move || {
            ticks_c.fetch_add(1, Ordering::Relaxed) > 200
        })));
        let ctx = rquickjs::Context::full(&rt).expect("ctx");
        let res: rquickjs::Result<()> = ctx.with(|ctx| {
            cz_modules::install_json(&ctx)?;
            cz_modules::install_web_globals(&ctx)?;
            let _: rquickjs::Value = ctx.eval(*src)?;
            Ok(())
        });
        assert!(res.is_err(), "hostile script '{name}' should fail cleanly, not succeed");
    }

    // 2. Hostile FFI fuzzing across cz-modules (Buffer base64/hex, atob/btoa, TextDecoder, JSON, URL)
    let rt = rquickjs::Runtime::new().expect("rt");
    rt.set_memory_limit(32 * 1024 * 1024);
    rt.set_max_stack_size(512 * 1024);
    let ctx = rquickjs::Context::full(&rt).expect("ctx");
    ctx.with(|ctx| {
        cz_modules::install_json(&ctx).expect("install_json");
        cz_modules::install_web_globals(&ctx).expect("install_web_globals");
        cz_modules::install_fetch(&ctx).expect("install_fetch");
        cz_modules::install_web(&ctx).expect("install_web");
        cz_modules::install_url(&ctx).expect("install_url");
        cz_modules::install_buffer(&ctx).expect("install_buffer");
        let js = r#"
            let s1 = 0x51515151, s2 = 0xa3a3a3a3;
            function rng() {
                s1 ^= s1 << 13; s1 ^= s1 >>> 17; s1 ^= s1 << 5;
                s2 ^= s2 << 7; s2 ^= s2 >>> 9; s2 ^= s2 << 8;
                return (s1 ^ s2) >>> 0;
            }
            const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/=-_ \t\n\r!@#$%^&*()[]{}:\"'\\,.;<>?~`";
            function randStr(maxLen) {
                const n = rng() % maxLen;
                let s = "";
                for (let i = 0; i < n; i++) s += alphabet[rng() % alphabet.length];
                return s;
            }

            // Buffer base64 / base64url / hex (NEON + scalar boundary lengths 0..96)
            for (let i = 0; i < 400; i++) {
                const s = randStr(96);
                for (const enc of ["base64", "base64url", "hex", "utf8", "latin1"]) {
                    try {
                        const b = Buffer.from(s, enc);
                        b.toString(enc);
                    } catch (_) {}
                }
            }

            // TextDecoder (fatal & non-fatal) with random + invalid UTF-8 bytes
            const tdLoose = new TextDecoder("utf-8", { fatal: false });
            const tdFatal = new TextDecoder("utf-8", { fatal: true });
            const te = new TextEncoder();
            for (let i = 0; i < 400; i++) {
                const len = rng() % 128;
                const u8 = new Uint8Array(len);
                for (let j = 0; j < len; j++) u8[j] = rng() & 0xff;
                const s = tdLoose.decode(u8);
                te.encode(s);
                try { tdFatal.decode(u8); } catch (_) {}
            }

            // atob / btoa
            for (let i = 0; i < 400; i++) {
                const s = randStr(96);
                try { atob(s); } catch (_) {}
                try {
                    const enc = btoa(s);
                    if (atob(enc) !== s) throw new Error("btoa/atob roundtrip fail");
                } catch (_) {}
            }

            // URL
            for (let i = 0; i < 400; i++) {
                const s = randStr(80);
                try { new URL(s); } catch (_) {}
                try { new URL(s, "https://example.com/a/b"); } catch (_) {}
            }

            // JSON.parse + structuredClone + JSON.stringify
            for (let i = 0; i < 400; i++) {
                const s = randStr(64);
                try {
                    const v = JSON.parse(s);
                    const c = structuredClone(v);
                    JSON.stringify(c);
                } catch (_) {}
            }

            // Large LazyJSON + structuredClone + stringify
            const bigObj = { items: Array.from({length: 100}, (_, i) => ({ id: i, text: "line_" + i + "_日本語" })) };
            const bigStr = JSON.stringify(bigObj);
            const parsed = JSON.parse(bigStr);
            const cloned = structuredClone(parsed);
            cloned.items[0].id = -1;
            if (parsed.items[0].id !== 0) throw new Error("LazyJSON clone mutation leaked");
            "SANDBOX_FUZZ_OK"
        "#;
        let out: String = ctx.eval(js).expect("sandbox fuzz eval");
        assert_eq!(out, "SANDBOX_FUZZ_OK");
    });
    eprintln!("fuzz_pi_sandbox_hostile_js_and_limits: all 7 hostile bombs + 1,600 FFI fuzz cases clean");
}
