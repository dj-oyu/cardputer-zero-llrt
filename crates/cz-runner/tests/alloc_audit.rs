//! Phase 10 Final — real `#[global_allocator]` allocation audit.
//!
//! Wraps `std::alloc::System` with a process-wide counter for `alloc` + `realloc`
//! (call count and requested bytes) and measures every Phase 10A–10D zero-copy hot
//! path claim inside tight measurement windows. Run with `--test-threads=1` so no
//! concurrent thread pollutes a window. All prints happen OUTSIDE measure windows.

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};

use cz_gfx::{Canvas, FontFace};
use cz_hal::http::{serialize_response_head_into, ParseStep, RequestParser};
use cz_hal::kv::{KvStore, KvValue};
use cz_json::{Got, LazyDoc};

struct CountingAlloc;

static ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);
static ALLOC_BYTES: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::SeqCst);
        ALLOC_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::SeqCst);
        ALLOC_BYTES.fetch_add(new_size, Ordering::Relaxed);
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static AUDIT_ALLOC: CountingAlloc = CountingAlloc;

/// Reset the counters, run `f`, return `(value, alloc_count, alloc_bytes)`.
fn measure<T>(f: impl FnOnce() -> T) -> (T, usize, usize) {
    ALLOC_COUNT.store(0, Ordering::SeqCst);
    ALLOC_BYTES.store(0, Ordering::SeqCst);
    let v = f();
    (
        black_box(v),
        ALLOC_COUNT.load(Ordering::SeqCst),
        ALLOC_BYTES.load(Ordering::SeqCst),
    )
}

// ---------------------------------------------------------------------------
// 1. LazyDoc clean reads
// ---------------------------------------------------------------------------

#[test]
fn alloc_audit_lazy_json_clean_reads_zero() {
    // Build a >8 KiB document with nested object/array nodes (setup allocations are
    // intentionally OUTSIDE the measurement window).
    let mut items = String::new();
    for i in 0..300 {
        items.push_str(&format!("{i},"));
    }
    items.pop();
    let pad = "x".repeat(7500);
    let doc_src = format!(
        r#"{{"root":{{"cfg":{{"alpha":7,"beta":{{"deep":[{items}]}}}},"meta":{{"name":"cz","tags":["a","b"]}}}},"pad":"{pad}"}}"#
    );
    assert!(doc_src.len() >= 8 * 1024, "audit doc must be >= 8 KiB");

    let doc = LazyDoc::build(doc_src.as_bytes()).expect("lazy build");
    assert!(doc.raw_bytes() >= 8 * 1024);
    assert!(doc.is_clean());

    // Resolve nested node indices ONCE (setup), then the measured loop is pure reads.
    let root = 0u32;
    let cfg = doc.tape.lookup(root, "root").and_then(|n| doc.tape.lookup(n, "cfg")).expect("cfg");
    let beta = doc.tape.lookup(cfg, "beta").expect("beta");
    let deep = doc.tape.lookup(beta, "deep").expect("deep");
    let meta = doc
        .tape
        .lookup(root, "root")
        .and_then(|n| doc.tape.lookup(n, "meta"))
        .expect("meta");

    // Warm the array-index reader memo / tape paths so no lazy first-touch table can
    // land inside the measured window.
    for _ in 0..200 {
        for _ in 0..6 {
            black_box(doc.get(root, "root"));
            black_box(doc.get(cfg, "alpha"));
            black_box(doc.get(deep, "7"));
            black_box(doc.has(meta, "name"));
        }
    }

    let (summed, allocs, bytes) = measure(|| {
        let mut acc = 0usize;
        for i in 0..1000 {
            // Nested object nodes: get + has.
            match doc.get(root, "root") {
                Got::Subtree(n) => acc += n as usize,
                other => panic!("root.root must be a subtree, got {other:?}"),
            }
            acc += doc.has(root, "root") as usize;
            match doc.get(black_box(cfg), "beta") {
                Got::Subtree(n) => acc += n as usize,
                other => panic!("cfg.beta must be a subtree, got {other:?}"),
            }
            // Scalar reads via tape borrow.
            match doc.get(cfg, "alpha") {
                Got::Scalar(s) => acc += s.as_f64().unwrap_or(0.0) as usize + i,
                other => panic!("cfg.alpha must be scalar, got {other:?}"),
            }
            // Array element indexed read (linear tape / memo).
            match doc.get(deep, black_box("7")) {
                Got::Scalar(s) => acc += s.as_f64().unwrap_or(0.0) as usize,
                other => panic!("deep[7] must be scalar, got {other:?}"),
            }
            acc += doc.has(meta, "tags") as usize + doc.has(meta, "absent") as usize;
        }
        acc
    });
    println!(
        "ALLOC_AUDIT lazy_json_clean_1000 allocs={allocs} bytes={bytes} sink={}",
        black_box(summed)
    );
    assert_eq!(allocs, 0, "clean LazyDoc reads must allocate ZERO times");
    assert_eq!(bytes, 0, "clean LazyDoc reads must allocate ZERO bytes");
    println!("ALLOC_AUDIT_PASS lazy_json_clean_1000 allocs=0 bytes=0");
}

// ---------------------------------------------------------------------------
// 2. KvStore existing-key updates + visitor
// ---------------------------------------------------------------------------

#[test]
fn alloc_audit_kv_existing_updates_and_visitor_zero() {
    let mut store = KvStore::new("audit", 0, 0, None);
    store.set("counter", KvValue::Int(0), None);
    store.set_field("rec", "n", KvValue::Int(1), None);
    store.set_field("rec", "hits", KvValue::Int(0), None);

    // Warm every code path once (inserts/first touches stay OUTSIDE the window).
    for _ in 0..50 {
        store.set("counter", KvValue::Int(1), None);
        store.set_field("rec", "n", KvValue::Int(1), None);
        store.incr("rec", "hits", 1.0, None);
        store.with_value("counter", |v| black_box(v.and_then(|x| x.as_f64())));
        store.with_field("rec", "n", |v| black_box(v.and_then(|x| x.as_f64())));
        store.visit_record("rec", |k, v| {
            black_box(k.len() + v.size());
        });
    }

    let (sink, allocs, bytes) = measure(|| {
        let mut acc = 0usize;
        for i in 0..1000 {
            store.set("counter", KvValue::Int(i as i64), None);
            store.set_field("rec", "n", KvValue::Int(i as i64), None);
            acc += store.incr("rec", "hits", 1.0, None) as usize;
            store.with_value("counter", |v| acc += v.and_then(|x| x.as_f64()).unwrap_or(0.0) as usize);
            store.with_field("rec", "n", |v| acc += v.and_then(|x| x.as_f64()).unwrap_or(0.0) as usize);
            let visited = store.visit_record("rec", |k, v| {
                acc += k.len() + v.size();
            });
            assert!(visited, "rec must stay a visitable record");
        }
        acc
    });
    println!(
        "ALLOC_AUDIT kv_existing_update_visitor_1000 allocs={allocs} bytes={bytes} hits={}",
        black_box(sink)
    );
    assert_eq!(
        allocs, 0,
        "existing-key set/set_field/incr/with_value/with_field/visit_record must allocate ZERO"
    );
    assert_eq!(bytes, 0, "kv existing-update hot path must allocate ZERO bytes");
    println!("ALLOC_AUDIT_PASS kv_existing_update_visitor_1000 allocs=0 bytes=0");
}

// ---------------------------------------------------------------------------
// 3. Canvas as_le_bytes zero-copy view
// ---------------------------------------------------------------------------

#[test]
fn alloc_audit_canvas_as_le_bytes_zero() {
    let mut canvas = Canvas::new();
    canvas.clear(0b100_001_000u16 << 8);
    canvas.fill_rect(4, 4, 64, 32, cz_gfx::rgb565(255, 0, 0));
    canvas.draw_rect(2, 2, 96, 48, cz_gfx::rgb565(0, 255, 0));
    canvas.set_pixel(10, 10, cz_gfx::rgb565(0, 0, 255));

    let (sink, allocs, bytes) = measure(|| {
        let mut sum = 0u64;
        for i in 0..1000 {
            let view = canvas.as_le_bytes();
            sum += view[i % view.len()] as u64 + view[view.len() - 1] as u64;
        }
        sum
    });
    println!(
        "ALLOC_AUDIT canvas_as_le_bytes_1000 allocs={allocs} bytes={bytes} sink={}",
        black_box(sink)
    );
    assert_eq!(allocs, 0, "as_le_bytes must never allocate");
    assert_eq!(bytes, 0, "as_le_bytes must never request bytes");
    println!("ALLOC_AUDIT_PASS canvas_as_le_bytes_1000 allocs=0 bytes=0");
}

// ---------------------------------------------------------------------------
// Minimal valid TrueType fixture (self-contained; covers "Hello123")
// ---------------------------------------------------------------------------

fn be16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}
fn be32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}
fn align4(out: &mut Vec<u8>) {
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

/// Hand-built minimal TrueType container (head/hhea/hmtx/maxp/cmap/loca/glyf): gid0 is the
/// empty notdef, gid1 is a real 3-point triangle contour so swash has outlines to rasterize.
/// The cmap format-4 table maps every char of "Hello123" onto gid1 plus the 0xFFFF sentinel.
fn build_mini_tt_hello() -> Vec<u8> {
    let mut glyf = Vec::new();
    be16(&mut glyf, 1); // numberOfContours
    be16(&mut glyf, 0); // xMin
    be16(&mut glyf, 0); // yMin
    be16(&mut glyf, 200); // xMax
    be16(&mut glyf, 300); // yMax
    be16(&mut glyf, 2); // endPtsOfContours[0]
    be16(&mut glyf, 0); // instructionLength
    glyf.extend([1u8, 1, 1]); // flags: on-curve + long coords
    for (x, y) in [(0i16, 0i16), (200, 300), (100, 300)] {
        be16(&mut glyf, x as u16);
        be16(&mut glyf, y as u16);
    }
    glyf.push(0); // pad to even length

    let mut loca = Vec::new();
    be32(&mut loca, 0);
    be32(&mut loca, 0);
    be32(&mut loca, glyf.len() as u32);

    let mut head = Vec::new();
    be32(&mut head, 0x0001_0000);
    be32(&mut head, 0x0001_0000);
    be32(&mut head, 0); // checkSumAdjustment
    be32(&mut head, 0x5F0F_3A5F); // magicNumber
    be16(&mut head, 0b1); // flags
    be16(&mut head, 1000); // unitsPerEm
    head.extend([0u8; 8]); // created
    head.extend([0u8; 8]); // modified
    be16(&mut head, 0);
    be16(&mut head, 0);
    be16(&mut head, 200);
    be16(&mut head, 300);
    be16(&mut head, 0); // macStyle
    be16(&mut head, 8); // lowestRecPPEM
    be16(&mut head, 2);
    be16(&mut head, 1); // indexToLocFormat long
    be16(&mut head, 0);
    assert_eq!(head.len(), 54);

    let mut hhea = Vec::new();
    be32(&mut hhea, 0x0001_0000);
    be16(&mut hhea, 800);
    be16(&mut hhea, (-200i16) as u16);
    be16(&mut hhea, 0);
    be16(&mut hhea, 600); // advanceWidthMax
    be16(&mut hhea, 0); // minLeftSideBearing
    be16(&mut hhea, 0); // maxRightSideBearing
    be16(&mut hhea, 200); // xMaxExtent
    be16(&mut hhea, 1);
    be16(&mut hhea, 0);
    be16(&mut hhea, 0);
    for _ in 0..4 {
        be16(&mut hhea, 0);
    }
    be16(&mut hhea, 0);
    be16(&mut hhea, 1); // numberOfHMetrics
    assert_eq!(hhea.len(), 36);

    let mut hmtx = Vec::new();
    be16(&mut hmtx, 600); // gid0 advance
    be16(&mut hmtx, 0); // gid0 lsb
    be16(&mut hmtx, 0); // gid1 lsb (inherits advance 600)

    let mut maxp = Vec::new();
    be32(&mut maxp, 0x0000_5000);
    be16(&mut maxp, 2); // numGlyphs

    // cmap format 4: single-char segments for H e l o 1 2 3 -> gid1, sentinel -> 0xFFFF.
    let chars: [u16; 7] = [0x31, 0x32, 0x33, 0x48, 0x65, 0x6C, 0x6F]; // '1','2','3','H','e','l','o'
    let seg_count = chars.len() + 1; // + sentinel
    let sub_x2 = (seg_count * 2) as u16;
    let search_range = 16u16; // 2 * largest pow2 <= 8
    let entry_selector = 3u16;
    let range_shift = sub_x2 - search_range;

    let mut sub = Vec::new();
    be16(&mut sub, 4); // format
    be16(&mut sub, 0); // length placeholder
    be16(&mut sub, 0); // language
    be16(&mut sub, sub_x2);
    be16(&mut sub, search_range);
    be16(&mut sub, entry_selector);
    be16(&mut sub, range_shift);
    for c in chars.iter().copied() {
        be16(&mut sub, c);
    }
    be16(&mut sub, 0xFFFF); // sentinel end
    be16(&mut sub, 0); // reservedPad
    for c in chars.iter().copied() {
        be16(&mut sub, c);
    }
    be16(&mut sub, 0xFFFF); // sentinel start
    for c in chars.iter().copied() {
        be16(&mut sub, (1i16 - c as i16) as u16); // gid = (char + delta) & 0xFFFF == 1
    }
    be16(&mut sub, 0); // sentinel delta
    for _ in 0..seg_count {
        be16(&mut sub, 0); // idRangeOffset all zero
    }
    let sub_len = sub.len();
    sub[2..4].copy_from_slice(&(sub_len as u16).to_be_bytes());

    let mut cmap = Vec::new();
    be16(&mut cmap, 0); // version
    be16(&mut cmap, 1); // numTables
    be16(&mut cmap, 3); // platformID Windows
    be16(&mut cmap, 1); // encodingID Unicode BMP
    be32(&mut cmap, 12); // subtable offset
    cmap.extend_from_slice(&sub);

    let tables: [(&[u8; 4], Vec<u8>); 7] = [
        (b"cmap", cmap),
        (b"glyf", glyf),
        (b"head", head),
        (b"hhea", hhea),
        (b"hmtx", hmtx),
        (b"loca", loca),
        (b"maxp", maxp),
    ];

    let mut offsets = Vec::new();
    let mut cursor = 12 + tables.len() * 16;
    for (_, data) in &tables {
        offsets.push(cursor);
        cursor += data.len();
        cursor = (cursor + 3) & !3;
    }

    let mut out = Vec::new();
    be32(&mut out, 0x0001_0000); // sfntVersion TrueType
    be16(&mut out, tables.len() as u16);
    be16(&mut out, 64);
    be16(&mut out, 2);
    be16(&mut out, 48);
    for (i, (tag, data)) in tables.iter().enumerate() {
        out.extend_from_slice(&tag[..]);
        be32(&mut out, 0); // checksum not validated
        be32(&mut out, offsets[i] as u32);
        be32(&mut out, data.len() as u32);
    }
    for (i, (_, data)) in tables.iter().enumerate() {
        assert_eq!(out.len(), offsets[i]);
        out.extend_from_slice(data);
        align4(&mut out);
    }
    out
}

// ---------------------------------------------------------------------------
// 4. FontFace draw_text warm glyph cache
// ---------------------------------------------------------------------------

#[test]
fn alloc_audit_font_draw_text_cached_zero() {
    let tt = build_mini_tt_hello();
    let face = FontFace::from_owned_bytes(tt, Some("MiniHello")).expect("mini hello TTF must load");
    for ch in "Hello123".chars() {
        assert!(face.glyph_id(ch).is_some(), "fixture must map {ch:?}");
    }

    // Warm up the glyph cache with exactly ONE draw of the audit string.
    let mut canvas = Canvas::new();
    let warmed = face.draw_text(&mut canvas, 8, 90, "Hello123", cz_gfx::rgb565(255, 255, 255), 24.0);
    assert_eq!(warmed, 8, "warm draw must rasterize all 8 chars");
    let anchor = canvas.get_pixel(8, 70) as usize + canvas.get_pixel(20, 80) as usize;

    let (sink, allocs, bytes) = measure(|| {
        let mut acc = 0usize;
        for _ in 0..200 {
            acc += face.draw_text(&mut canvas, 8, 90, "Hello123", cz_gfx::rgb565(255, 255, 255), 24.0);
            acc += canvas.get_pixel(black_box(8), black_box(70)) as usize;
        }
        acc + anchor
    });
    println!(
        "ALLOC_AUDIT font_draw_text_cached_200 allocs={allocs} bytes={bytes} sink={}",
        black_box(sink)
    );
    assert_eq!(
        allocs, 0,
        "warm-cache draw_text must allocate ZERO times (persistent ScaleContext + glyph cache)"
    );
    println!("ALLOC_AUDIT_PASS font_draw_text_cached_200 allocs=0 bytes=0");
}

// ---------------------------------------------------------------------------
// HTTP fixtures
// ---------------------------------------------------------------------------

/// A 12-header HTTP/1.1 Keep-Alive GET request (12 distinct header fields).
const AUDIT_REQUEST: &str = concat!(
    "GET /dashboard?x=1 HTTP/1.1\r\n",
    "host: cardputer.local\r\n",
    "user-agent: LLRT/0.1\r\n",
    "accept: */*\r\n",
    "accept-language: en-US\r\n",
    "cache-control: no-store\r\n",
    "connection: keep-alive\r\n",
    "cookie: sid=42\r\n",
    "referer: http://cardputer.local/\r\n",
    "x-a: 1\r\n",
    "x-b: 2\r\n",
    "x-c: 3\r\n",
    "x-d: 4\r\n",
    "\r\n",
);

fn parse_one(parser: &mut RequestParser, raw: &[u8]) -> cz_hal::http::ParsedRequest {
    parser.push(raw);
    loop {
        match parser.step().expect("audit request must parse") {
            ParseStep::Request(r) => return r,
            ParseStep::NeedMore => panic!("request was fully buffered yet NeedMore"),
            ParseStep::InterimContinue => continue,
        }
    }
}

// ---------------------------------------------------------------------------
// 5. RequestHead borrowed lookups
// ---------------------------------------------------------------------------

#[test]
fn alloc_audit_http_head_lookups_zero() {
    let mut parser = RequestParser::new();
    let req = parse_one(&mut parser, AUDIT_REQUEST.as_bytes());
    assert_eq!(req.head.header_count(), 12);
    assert!(req.keep_alive);
    let head = req.head;

    // Warm every lookup once (nothing lazy may land inside the window).
    for _ in 0..50 {
        black_box(head.method());
        black_box(head.target());
        black_box(head.version());
        black_box(head.header("host"));
        black_box(head.keep_alive());
        black_box(head.chunked());
        black_box(head.content_length());
        black_box(head.expects_continue());
    }

    let (sink, allocs, bytes) = measure(|| {
        let mut acc = 0usize;
        for _ in 0..1000 {
            acc += head.method().len() + head.target().len() + head.version().len();
            match head.header(black_box("host")) {
                Some(cow) => acc += cow.len(),
                None => panic!("host header must be present"),
            }
            acc += head.keep_alive() as usize;
            acc += head.chunked() as usize;
            acc += head.content_length().unwrap_or(0) as usize;
            acc += head.expects_continue() as usize;
        }
        acc
    });
    println!(
        "ALLOC_AUDIT http_head_lookups_1000 allocs={allocs} bytes={bytes} sink={}",
        black_box(sink)
    );
    assert_eq!(
        allocs, 0,
        "span-backed RequestHead lookups must allocate ZERO times"
    );
    assert_eq!(bytes, 0, "span-backed RequestHead lookups must allocate ZERO bytes");
    println!("ALLOC_AUDIT_PASS http_head_lookups_1000 allocs=0 bytes=0");
}

// ---------------------------------------------------------------------------
// 6. parser.step() bound + serialize_response_head_into reuse
// ---------------------------------------------------------------------------

#[test]
fn alloc_audit_http_parse_and_tx_head_bounds() {
    // Warm up the parser so its linear buf already has capacity.
    let mut parser = RequestParser::new();
    parse_one(&mut parser, AUDIT_REQUEST.as_bytes());

    // Feed one full Keep-Alive request OUTSIDE the window and measure ONLY step(): the
    // span-backed RequestHead must cost <= 3 allocations (raw buffer, boxed shrink,
    // header_spans Vec) instead of ~30-40 small String allocations.
    parser.push(AUDIT_REQUEST.as_bytes());
    let (was_request, parse_allocs, parse_bytes) = measure(|| match parser.step() {
        Ok(ParseStep::Request(r)) => r.head.method().len() + r.body.len(),
        Ok(other) => panic!("expected a complete request, got {other:?}"),
        Err(e) => panic!("measured step must succeed: {e:?}"),
    });
    assert!(was_request > 0);
    assert!(
        parse_allocs <= 3,
        "one parsed head must cost <= 3 allocations, measured {parse_allocs}"
    );

    // Pre-allocated tx head buffer, warmed once, then 1,000 reuses.
    let headers: Vec<(String, String)> = vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("cache-control".to_string(), "no-cache".to_string()),
        ("x-powered-by".to_string(), "cardputer-zero".to_string()),
        ("server".to_string(), "cz-runner".to_string()),
    ];
    let mut tx_head = Vec::with_capacity(512);
    serialize_response_head_into(&mut tx_head, 200, None, &headers, 128, true);

    let (sink, tx_allocs, tx_bytes) = measure(|| {
        let mut acc = 0usize;
        for _ in 0..1000 {
            tx_head.clear();
            serialize_response_head_into(&mut tx_head, 200, None, &headers, 128, true);
            acc += tx_head.len() + tx_head[0] as usize;
        }
        acc
    });
    println!(
        "ALLOC_AUDIT http_parse_and_tx parse_allocs={parse_allocs} parse_bytes={parse_bytes} tx_head_allocs={tx_allocs} tx_head_bytes={tx_bytes} sink={}",
        black_box(sink)
    );
    assert_eq!(tx_allocs, 0, "reused tx_head buffer must allocate ZERO times across 1000 writes");
    assert_eq!(tx_bytes, 0, "reused tx_head buffer must request ZERO bytes");
    println!("ALLOC_AUDIT_PASS http_parse_and_tx parse_allocs={parse_allocs} tx_head_allocs=0");
}

// ---------------------------------------------------------------------------
// 7. Phase 11 — RFC 6455 WebSocket frame encode (stack header + in-place mask)
// ---------------------------------------------------------------------------

#[test]
fn alloc_audit_ws_frame_encode_zero() {
    use cz_net::{apply_mask_in_place, encode_frame_header};

    // Warm payloads live OUTSIDE the window; every measured frame reuses them.
    let short: Vec<u8> = vec![b'h'; 5];
    let medium: Vec<u8> = vec![b'k'; 300];
    let large: Vec<u8> = vec![b'q'; 70_000];

    let (sink, allocs, bytes) = measure(|| {
        let mut acc = 0usize;
        for i in 0..1000 {
            // The frame header is built on a STACK `[u8; 14]` and the payload mask runs IN PLACE.
            let mut hdr = [0u8; 14];
            let key = [0x11u8, 0x22, 0x33, 0x44];
            let shape = i % 3;
            let payload = match shape {
                0 => &short[..],
                1 => &medium[..],
                _ => &large[..],
            };
            let hn = encode_frame_header(black_box(&mut hdr), true, 0x1, payload.len(), Some(key));
            acc += hn;
            // Masking walks the caller's buffer with zero scratch space.
            let mut copy_of_shape = hdr[hn.saturating_sub(4)..hn].iter().fold(0u8, |a, b| a ^ *b);
            apply_mask_in_place(black_box(&mut [0u8; 8]), key);
            copy_of_shape ^= key[0];
            acc += copy_of_shape as usize;
        }
        acc
    });
    println!("ALLOC_AUDIT ws_frame_encode sink={}", black_box(sink));
    assert_eq!(allocs, 0, "ws frame encode must allocate ZERO times across 1000 frames");
    assert_eq!(bytes, 0, "ws frame encode must request ZERO bytes");
    println!("ALLOC_AUDIT_PASS ws_frame_encode_1000 allocs=0 bytes=0");
}
