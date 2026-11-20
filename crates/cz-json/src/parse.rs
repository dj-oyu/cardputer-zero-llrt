//! Strict RFC 8259 recursive-descent core shared by BOTH the fast DOM parser and the Tape builder.
//! One single validator drives two sinks (`Sink`), so validation semantics can never diverge
//! between `< 4096` byte parses and `>= 4096` byte Lazy builds. Never panics: malformed input is
//! always `Err(ParseErr)` which the JS bridge turns into a `SyntaxError`.

use std::borrow::Cow;

use crate::simd::{is_ascii_fast, scan_unescaped_run};
use crate::val::{Span, Val};

/// Nesting cap (mirrors QuickJS's recursion guard) so adversarial documents cannot overflow.
pub const MAX_DEPTH: usize = 120;

/// Syntax error carrying the offending byte offset.
#[derive(Clone, Debug, PartialEq)]
pub struct ParseErr {
    pub pos: usize,
    pub msg: String,
}

impl ParseErr {
    pub fn new(pos: usize, msg: impl Into<String>) -> Self {
        Self { pos, msg: msg.into() }
    }
}

impl std::fmt::Display for ParseErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JSON syntax error at byte {}: {}", self.pos, self.msg)
    }
}

impl std::error::Error for ParseErr {}

/// An object member key: decoded text, its 16-bit fingerprint and the raw content span.
#[derive(Clone, Debug)]
pub struct Key<'a> {
    pub text: Cow<'a, str>,
    /// Fingerprint used by `TapeDoc` for cheap mismatch rejection before real key comparison.
    pub fp: u16,
    pub span: Span,
    pub escaped: bool,
}

/// Callback sink: every validated token reports to exactly one implementation.
pub trait Sink<'a> {
    fn null(&mut self);
    fn boolean(&mut self, is_true: bool);
    fn integer(&mut self, value: i64, span: Span);
    fn float(&mut self, value: f64, span: Span);
    fn string(&mut self, text: Cow<'a, str>, span: Span, escaped: bool);
    fn array_begin(&mut self);
    fn array_end(&mut self, count: usize, span: Span);
    fn object_begin(&mut self);
    fn object_key(&mut self, key: Key<'a>);
    fn object_end(&mut self, count: usize, span: Span);
}

/// Byte cursor over the document.
pub struct Scanner<'a> {
    pub b: &'a [u8],
    pub p: usize,
}

impl<'a> Scanner<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Self { b, p: 0 }
    }

    #[inline]
    pub fn peek(&self) -> Option<u8> {
        self.b.get(self.p).copied()
    }

    /// Skip exactly the four RFC 8259 whitespace bytes.
    pub fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c == b' ' || c == b'\t' || c == b'\n' || c == b'\r' {
                self.p += 1;
            } else {
                break;
            }
        }
    }

    fn expect(&mut self, c: u8) -> Result<(), ParseErr> {
        match self.peek() {
            Some(x) if x == c => {
                self.p += 1;
                Ok(())
            }
            _ => Err(ParseErr::new(self.p, format!("Expected '{c:?}'"))),
        }
    }

    fn fail(&self, msg: impl Into<String>) -> ParseErr {
        ParseErr::new(self.p, msg)
    }

    /// Locate the unescaped closing quote of a string body that contains escapes. Escapes are
    /// stepped over positionally (`\uXXXX` plus an optional low-surrogate partner); raw control
    /// bytes and running off the end stop the walk so `scan_string` can report them.
    fn find_close_quote(&self, from: usize) -> Option<usize> {
        let mut i = from;
        loop {
            let r = scan_unescaped_run(self.b, i);
            if r >= self.b.len() {
                return None;
            }
            let c = self.b[r];
            if c == b'"' {
                return Some(r);
            }
            if c < 0x20 {
                return None; // invalid control byte: caller reports it
            }
            let esc = *self.b.get(r + 1)?;
            if esc != b'u' {
                i = r + 2;
                continue;
            }
            // `\uXXXX`: only a HIGH surrogate may consume a following low-surrogate partner.
            match (hex4_opt(self.b, r + 2), hex4_opt(self.b, r + 8)) {
                (Some(hi), Some(lo))
                    if (0xD800..0xDC00).contains(&hi)
                        && (0xDC00..0xE000).contains(&lo)
                        && self.b.get(r + 6) == Some(&b'\\')
                        && self.b.get(r + 7) == Some(&b'u') =>
                {
                    i = r + 12;
                }
                _ => i = r + 6,
            }
        }
    }

    /// Scan a JSON string literal (quotes included in consumption). Returns the decoded body,
    /// whether escapes were present, and the CONTENT span `[open+1, close)`.
    pub fn scan_string(&mut self) -> Result<(Cow<'a, str>, bool, Span), ParseErr> {
        let open = self.p;
        self.expect(b'"')?;
        let cs = self.p;
        let first = scan_unescaped_run(self.b, cs);
        if first < self.b.len() && self.b[first] == b'"' {
            // Clean run: borrow straight from the input buffer (zero copy, no intermediate Vec).
            let body = &self.b[cs..first];
            // Fast path: pure-ASCII bodies skip the UTF-8 state machine entirely.
            let s = if is_ascii_fast(body) {
                // SAFETY: every byte < 0x80, which is valid single-byte UTF-8 by construction.
                unsafe { std::str::from_utf8_unchecked(body) }
            } else {
                std::str::from_utf8(body)
                    .map_err(|_| ParseErr::new(cs, "Invalid UTF-8 in string"))?
            };
            self.p = first + 1;
            return Ok((Cow::Borrowed(s), false, (cs as u32, first as u32)));
        }
        if first < self.b.len() && self.b[first] < 0x20 {
            return Err(ParseErr::new(first, "Unescaped control character in string"));
        }
        // Escape path: locate the closing quote, then decode the body with shared rules.
        let close = self
            .find_close_quote(cs)
            .ok_or_else(|| ParseErr::new(open, "Unterminated string or raw control byte"))?;
        self.p = close + 1;
        let owned = decode_string_content(&self.b[cs..close])?;
        Ok((Cow::Owned(owned), true, (cs as u32, close as u32)))
    }

    /// Scan an RFC 8259 number token; returns its span plus the integer fast-path result when the
    /// token is pure digits (no `.`/`e`/`E`), otherwise the parsed `f64`.
    pub fn scan_number(&mut self) -> Result<(Span, Option<i64>, f64), ParseErr> {
        let start = self.p;
        let neg = self.peek() == Some(b'-');
        if neg {
            self.p += 1;
        }
        // Integer part: 0 | [1-9][0-9]*  (rejects leading zeros such as "01"). Single-pass
        // digit accumulation so integer tokens never get re-parsed from their slice.
        let mut acc: u64 = 0;
        let mut digits: usize = 0;
        match self.peek() {
            Some(b'0') => {
                digits = 1;
                self.p += 1;
            }
            Some(c) if c.is_ascii_digit() => {
                while matches!(self.peek(), Some(d) if d.is_ascii_digit()) {
                    acc = acc.wrapping_mul(10).wrapping_add((self.b[self.p] - b'0') as u64);
                    digits += 1;
                    self.p += 1;
                }
            }
            _ => return Err(ParseErr::new(start, "Invalid number")),
        }
        let mut is_int = true;
        if self.peek() == Some(b'.') {
            self.p += 1;
            if !matches!(self.peek(), Some(d) if d.is_ascii_digit()) {
                return Err(ParseErr::new(self.p, "Missing digits after '.'"));
            }
            while matches!(self.peek(), Some(d) if d.is_ascii_digit()) {
                self.p += 1;
            }
            is_int = false;
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            self.p += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.p += 1;
            }
            if !matches!(self.peek(), Some(d) if d.is_ascii_digit()) {
                return Err(ParseErr::new(self.p, "Missing exponent digits"));
            }
            while matches!(self.peek(), Some(d) if d.is_ascii_digit()) {
                self.p += 1;
            }
            is_int = false;
        }
        let end = self.p;
        let tok = &self.b[start..end];
        let span = (start as u32, end as u32);
        if is_int {
            if digits > 1 && tok[usize::from(neg)] == b'0' {
                return Err(ParseErr::new(start, "Invalid number token with leading zero"));
            }
            // Reuse the value accumulated during the digit scan (no second pass over the token).
            let fits = if neg { acc <= 9223372036854775808 } else { acc <= i64::MAX as u64 };
            if fits {
                let v = if neg && acc == 9223372036854775808 {
                    i64::MIN
                } else if neg {
                    -(acc as i64)
                } else {
                    acc as i64
                };
                return Ok((span, Some(v), v as f64));
            }
            // Beyond i64 magnitude: QuickJS keeps such numbers as floats.
        }
        let text = if is_ascii_fast(tok) {
            // SAFETY: JSON number grammar tokens are pure ASCII (`-`, digits, `.`, `e`, `+`).
            unsafe { std::str::from_utf8_unchecked(tok) }
        } else {
            std::str::from_utf8(tok).map_err(|_| ParseErr::new(start, "Invalid number"))?
        };
        let f = match text.parse::<f64>() {
            Ok(f) => f,
            // Grammar already validated, so a parse failure can only be magnitude overflow.
            Err(_) => {
                if tok.first() == Some(&b'-') {
                    f64::NEG_INFINITY
                } else {
                    f64::INFINITY
                }
            }
        };
        Ok((span, None, f))
    }

    fn scan_literal(&mut self, word: &[u8]) -> Result<(), ParseErr> {
        let at = self.p;
        if self.b[at..].starts_with(word) {
            self.p += word.len();
            Ok(())
        } else {
            Err(ParseErr::new(at, "Unexpected token"))
        }
    }
}

/// `const`-built 256-entry hex nibble map: valid `[0-9a-fA-F]` -> 0..15, everything else -> -1.
/// Replaces the match-chain `hex_val`, so four digits decode through independent table loads
/// with zero serial branches (Round 3 A53 stall work — see audit entry for `hex4_opt`).
const HEX_NIBBLE: [i8; 256] = build_hex_nibble();

const fn build_hex_nibble() -> [i8; 256] {
    let mut t = [-1i8; 256];
    let mut c = 0usize;
    while c < 10 {
        t[b'0' as usize + c] = c as i8;
        c += 1;
    }
    let mut c = 0usize;
    while c < 6 {
        t[b'a' as usize + c] = (c + 10) as i8;
        t[b'A' as usize + c] = (c + 10) as i8;
        c += 1;
    }
    t
}

/// Four hex digits at `at` inside `buf`, or `None` when truncated/non-hex (positional walker form).
/// The four bytes arrive as ONE little-endian word (A53 handles unaligned loads), and rejection
/// is a single OR-of-sign-bits test — no serial `?` chain across the four digits.
#[inline]
fn hex4_opt(buf: &[u8], at: usize) -> Option<u32> {
    if at + 4 > buf.len() {
        return None;
    }
    let w = u32::from_le_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]]);
    let t0 = HEX_NIBBLE[(w & 0xff) as usize] as u32;
    let t1 = HEX_NIBBLE[((w >> 8) & 0xff) as usize] as u32;
    let t2 = HEX_NIBBLE[((w >> 16) & 0xff) as usize] as u32;
    let t3 = HEX_NIBBLE[(w >> 24) as usize] as u32;
    // Any invalid lane sign-extends to 0xFFFFFFFF; one test rejects all four.
    if (t0 | t1 | t2 | t3) & 0x8000_0000 != 0 {
        return None;
    }
    // First byte is the most significant nibble, exactly like the old shift loop.
    Some((t0 << 12) | (t1 << 8) | (t2 << 4) | t3)
}

/// Four hex digits at `at` inside `buf`. Error positions match the old byte-wise walk exactly:
/// truncation reports `at`, and the FIRST bad digit reports `at + k`.
fn hex4_at(buf: &[u8], at: usize) -> Result<u32, ParseErr> {
    if at + 4 > buf.len() {
        return Err(ParseErr::new(at, "Truncated \\u escape"));
    }
    match hex4_opt(buf, at) {
        Some(v) => Ok(v),
        None => {
            // Cold path: locate the first offending byte for the exact legacy message/offset.
            for k in 0..4 {
                if HEX_NIBBLE[buf[at + k] as usize] < 0 {
                    return Err(ParseErr::new(at + k, "Invalid hex digit"));
                }
            }
            Err(ParseErr::new(at, "Invalid hex digit")) // unreachable fallback
        }
    }
}

/// Decode an escaped JSON string BODY (bytes strictly between the quotes). Identical rules to the
/// parse-time decoder (short escapes, `\uXXXX`, mandatory surrogate pairing) so Tape reads and
/// DOM parses always agree. Used lazily: escaped text materializes only when actually touched.
pub fn decode_string_content(content: &[u8]) -> Result<String, ParseErr> {
    // Pre-size for the output (escapes only ever SHRINK vs the escaped source length).
    let mut out = String::with_capacity(content.len());
    let mut i = 0usize;
    while i < content.len() {
        let r = scan_unescaped_run(content, i);
        if r > i {
            let run = &content[i..r];
            if crate::simd::is_ascii_fast(run) {
                // SAFETY: pure ASCII is valid UTF-8 by construction.
                out.push_str(unsafe { std::str::from_utf8_unchecked(run) });
            } else {
                out.push_str(
                    std::str::from_utf8(run)
                        .map_err(|_| ParseErr::new(i, "Invalid UTF-8 in string"))?,
                );
            }
        }
        if r >= content.len() {
            break;
        }
        let mut pair = false;
        let esc = content
            .get(r + 1)
            .copied()
            .ok_or_else(|| ParseErr::new(r, "Truncated escape"))?;
        let ch = match esc {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{08}',
            b'f' => '\u{0c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => {
                let hi = hex4_at(content, r + 2)?;
                if (0xD800..0xDC00).contains(&hi) {
                    if content.get(r + 6) != Some(&b'\\') || content.get(r + 7) != Some(&b'u') {
                        return Err(ParseErr::new(r + 2, "Lone high surrogate"));
                    }
                    let lo = hex4_at(content, r + 8)?;
                    if !(0xDC00..0xE000).contains(&lo) {
                        return Err(ParseErr::new(r + 8, "Invalid low surrogate"));
                    }
                    let cp = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                    pair = true;
                    char::from_u32(cp).ok_or_else(|| ParseErr::new(r + 2, "Bad surrogate pair"))?
                } else if (0xDC00..0xE000).contains(&hi) {
                    return Err(ParseErr::new(r + 2, "Lone low surrogate"));
                } else {
                    char::from_u32(hi).ok_or_else(|| ParseErr::new(r + 2, "Invalid \\u escape"))?
                }
            }
            _ => return Err(ParseErr::new(r + 1, "Invalid escape sequence")),
        };
        out.push(ch);
        i = if esc == b'u' {
            // Advance 12 bytes ONLY when this escape is a high surrogate whose low partner follows.
            if pair { r + 12 } else { r + 6 }
        } else {
            r + 2
        };
    }
    Ok(out)
}

/// Drive a whole document through `sink`: whitespace, one strict value, then EOF.
pub fn run_parse<'a, S: Sink<'a>>(input: &'a [u8], sink: &mut S) -> Result<(), ParseErr> {
    // No full-document UTF-8 pre-validation pass: every string body and member key is validated
    // when borrowed (`scan_string` clean path / `key_bytes`), so an invalid byte always errors at
    // its own offset. Skipping the extra whole-document checker removes one 100% memory pass.
    let mut sc = Scanner::new(input);
    sc.skip_ws();
    if sc.peek().is_none() {
        return Err(ParseErr::new(0, "Unexpected empty input"));
    }
    parse_value(&mut sc, sink, 0)?;
    sc.skip_ws();
    if sc.peek().is_some() {
        return Err(sc.fail("Unexpected trailing characters"));
    }
    Ok(())
}

fn parse_value<'a, S: Sink<'a>>(sc: &mut Scanner<'a>, sink: &mut S, depth: usize) -> Result<(), ParseErr> {
    if depth > MAX_DEPTH {
        return Err(ParseErr::new(sc.p, "Nesting too deep"));
    }
    match sc.peek() {
        Some(b'{') => {
            let start = sc.p;
            sc.expect(b'{')?;
            sink.object_begin();
            sc.skip_ws();
            if sc.peek() == Some(b'}') {
                sc.p += 1;
                sink.object_end(0, (start as u32, sc.p as u32));
                return Ok(());
            }
            let mut count = 0usize;
            loop {
                sc.skip_ws();
                if sc.peek() != Some(b'"') {
                    return Err(sc.fail("Object keys must be strings"));
                }
                let (text, escaped, span) = sc.scan_string()?;
                let fp = crate::tape::key_fingerprint(text.as_ref());
                sink.object_key(Key { text, fp, span, escaped });
                sc.skip_ws();
                sc.expect(b':')?;
                sc.skip_ws();
                parse_value(sc, sink, depth + 1)?;
                count += 1;
                sc.skip_ws();
                match sc.peek() {
                    Some(b',') => sc.p += 1,
                    Some(b'}') => {
                        sc.p += 1;
                        sink.object_end(count, (start as u32, sc.p as u32));
                        return Ok(());
                    }
                    _ => return Err(sc.fail("Expected ',' or '}' in object")),
                }
            }
        }
        Some(b'[') => {
            let start = sc.p;
            sc.expect(b'[')?;
            sink.array_begin();
            sc.skip_ws();
            if sc.peek() == Some(b']') {
                sc.p += 1;
                sink.array_end(0, (start as u32, sc.p as u32));
                return Ok(());
            }
            let mut count = 0usize;
            loop {
                sc.skip_ws();
                parse_value(sc, sink, depth + 1)?;
                count += 1;
                sc.skip_ws();
                match sc.peek() {
                    Some(b',') => sc.p += 1,
                    Some(b']') => {
                        sc.p += 1;
                        sink.array_end(count, (start as u32, sc.p as u32));
                        return Ok(());
                    }
                    _ => return Err(sc.fail("Expected ',' or ']' in array")),
                }
            }
        }
        Some(b'"') => {
            let (text, escaped, span) = sc.scan_string()?;
            sink.string(text, span, escaped);
            Ok(())
        }
        Some(b't') => {
            sc.scan_literal(b"true")?;
            sink.boolean(true);
            Ok(())
        }
        Some(b'f') => {
            sc.scan_literal(b"false")?;
            sink.boolean(false);
            Ok(())
        }
        Some(b'n') => {
            sc.scan_literal(b"null")?;
            sink.null();
            Ok(())
        }
        Some(c) if c == b'-' || c.is_ascii_digit() => {
            let (span, int, f) = sc.scan_number()?;
            match int {
                Some(v) => sink.integer(v, span),
                None => sink.float(f, span),
            }
            Ok(())
        }
        _ => Err(ParseErr::new(sc.p, "Unexpected token")),
    }
}

// ---------------------------------------------------------------------------
// DOM sink: the fast `< 4096` byte zero-copy parse path
// ---------------------------------------------------------------------------

enum Frame<'a> {
    Arr(Vec<Val<'a>>),
    Obj(Vec<(Cow<'a, str>, Val<'a>)>, Option<Cow<'a, str>>),
}

/// Builds a borrowed `Val` tree; unescaped strings point straight into the input buffer.
pub struct DomSink<'a> {
    stack: Vec<Frame<'a>>,
    root: Option<Val<'a>>,
}

impl<'a> DomSink<'a> {
    pub fn new() -> Self {
        Self { stack: Vec::new(), root: None }
    }

    fn push_value(&mut self, v: Val<'a>) {
        match self.stack.last_mut() {
            Some(Frame::Arr(items)) => items.push(v),
            Some(Frame::Obj(fields, pending)) => {
                let k = pending.take().expect("object key reported before its member value");
                fields.push((k, v));
            }
            None => self.root = Some(v),
        }
    }
}

impl<'a> Default for DomSink<'a> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> Sink<'a> for DomSink<'a> {
    fn null(&mut self) {
        self.push_value(Val::Null);
    }
    fn boolean(&mut self, is_true: bool) {
        self.push_value(Val::Bool(is_true));
    }
    fn integer(&mut self, value: i64, _span: Span) {
        self.push_value(Val::Int(value));
    }
    fn float(&mut self, value: f64, _span: Span) {
        self.push_value(Val::Float(value));
    }
    fn string(&mut self, text: Cow<'a, str>, _span: Span, _escaped: bool) {
        self.push_value(Val::Str(text));
    }
    fn array_begin(&mut self) {
        self.stack.push(Frame::Arr(Vec::new()));
    }
    fn array_end(&mut self, _count: usize, _span: Span) {
        match self.stack.pop() {
            Some(Frame::Arr(items)) => self.push_value(Val::Arr(items)),
            _ => unreachable!("balanced array frame"),
        }
    }
    fn object_begin(&mut self) {
        self.stack.push(Frame::Obj(Vec::new(), None));
    }
    fn object_key(&mut self, key: Key<'a>) {
        match self.stack.last_mut() {
            Some(Frame::Obj(_, pending)) => *pending = Some(key.text),
            _ => unreachable!("balanced object frame"),
        }
    }
    fn object_end(&mut self, _count: usize, _span: Span) {
        match self.stack.pop() {
            Some(Frame::Obj(fields, _)) => self.push_value(Val::Obj(fields)),
            _ => unreachable!("balanced object frame"),
        }
    }
}

/// Parse into a borrowed value tree (clean strings borrow the input bytes).
pub fn parse_dom(input: &[u8]) -> Result<Val<'_>, ParseErr> {
    let mut sink = DomSink::new();
    run_parse(input, &mut sink)?;
    sink.root.ok_or_else(|| ParseErr::new(0, "No root value"))
}

/// Parse and fully own the tree (needed when the DOM must outlive its input buffer).
pub fn parse_owned(input: &[u8]) -> Result<Val<'static>, ParseErr> {
    Ok(parse_dom(input)?.to_owned_val())
}

/// Strict validation only, constructing nothing (cheap conformance checks).
pub fn validate(input: &[u8]) -> Result<(), ParseErr> {
    struct NullSink;
    impl<'a> Sink<'a> for NullSink {
        fn null(&mut self) {}
        fn boolean(&mut self, _: bool) {}
        fn integer(&mut self, _: i64, _: Span) {}
        fn float(&mut self, _: f64, _: Span) {}
        fn string(&mut self, _: Cow<'a, str>, _: Span, _: bool) {}
        fn array_begin(&mut self) {}
        fn array_end(&mut self, _: usize, _: Span) {}
        fn object_begin(&mut self) {}
        fn object_key(&mut self, _: Key<'a>) {}
        fn object_end(&mut self, _: usize, _: Span) {}
    }
    run_parse(input, &mut NullSink)
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    #[test]
    fn parses_full_ecma_shape_with_unicode_and_escapes() {
        let src = r#"{"a":1,"b":-42,"c":3.14159,"d":1e5,"e":true,"f":false,"g":null,
            "h":"hello\n\t\"world\"","jp":"日本語テスト","emoji":"\uD83D\uDE00",
            "arr":[1,2,{"nested":[true,false]}],"empty":{},"emptyArr":[]}"#;
        let v = parse_dom(src.as_bytes()).expect("valid document");
        assert_eq!(v.get("a").and_then(|x| x.as_f64()), Some(1.0));
        assert_eq!(v.get("b").and_then(|x| x.as_f64()), Some(-42.0));
        assert!((v.get("c").and_then(|x| x.as_f64()).unwrap() - 3.14159).abs() < 1e-12);
        assert_eq!(v.get("d").and_then(|x| x.as_f64()), Some(100000.0));
        assert_eq!(v.get("h").and_then(|x| x.as_str()), Some("hello\n\t\"world\""));
        assert_eq!(v.get("jp").and_then(|x| x.as_str()), Some("日本語テスト"));
        assert_eq!(v.get("emoji").and_then(|x| x.as_str()), Some("😀"));
        let arr = v.get("arr").expect("arr");
        assert_eq!(arr.len(), 3);
        assert_eq!(arr.to_json_string(), r#"[1,2,{"nested":[true,false]}]"#);
        assert_eq!(v.get("empty").map(|x| x.len()), Some(0));
    }

    #[test]
    fn clean_strings_borrow_the_input_buffer() {
        // Zero-copy proof: a DOM string points into the source bytes, no intermediate Vec.
        let src = b"{\"k\":\"plain unescaped run of text\"}";
        let v = parse_dom(src).expect("parse");
        let s = v.get("k").and_then(|x| x.as_str()).expect("value");
        let same_pointer = std::ptr::eq(s.as_bytes().as_ptr(), &src[6]);
        assert!(same_pointer, "unescaped strings must alias the input buffer");
    }

    #[test]
    fn invalid_documents_are_rejected_never_panicking() {
        let bad: Vec<&str> = vec![
            "",
            "{",
            "{\"a\":}",
            "[1,]",
            "{\"a\":1} trailing",
            "\"unterminated",
            "tru",
            "nul",
            "01",
            ".5",
            "1.",
            "e5",
            "1e",
            "NaN",
            "Infinity",
            "{a:1}",
            "{\"a\":1,,}",
            "[1 2]",
            "\"raw\tcontrol\"",
            "\"lone\\uD800surrogate\"",
            "\"lone\\uDC00low\"",
            "\"\\x\"",
            "\"\\u12g4\"",
            "+5",
            "- 5",
        ];
        for s in bad {
            let err = parse_dom(s.as_bytes()).expect_err("must reject: {s}");
            assert!(err.msg.len() > 0, "empty error message for {s:?}");
        }
    }

    #[test]
    fn whitespace_forms_tolerated_but_trailing_rejected() {
        let ok = b"  \t\n\r  {\"a\": [1, 2]} \n ";
        assert!(validate(ok).is_ok());
        assert!(validate(b"{} extra").is_err());
        assert!(validate(b"{}\x00").is_err()); // NUL is not JSON whitespace
    }

    #[test]
    fn nesting_depth_guard_errors_instead_of_overflowing() {
        let deep = "[".repeat(400) + &"]".repeat(400);
        let err = validate(deep.as_bytes()).expect_err("deep nesting must be rejected");
        assert!(err.msg.contains("Nesting too deep"), "{err}");
    }

    #[test]
    fn number_token_forms_round_trip_like_quickjs() {
        let src = r#"[0, -0, -0.5, 1e2, 1E+2, -3e-3, 1.7976931348623157e308, 9e999, 12345678901234567890]"#;
        let v = parse_dom(src.as_bytes()).expect("numbers");
        let items = match &v { Val::Arr(a) => a, _ => panic!("array expected") };
        assert_eq!(items[0].as_f64(), Some(0.0));
        assert_eq!(items[1].as_f64(), Some(0.0));
        assert_eq!(items[2].as_f64(), Some(-0.5));
        assert_eq!(items[3].as_f64(), Some(100.0));
        assert_eq!(items[4].as_f64(), Some(100.0));
        assert!((items[5].as_f64().unwrap() + 0.003).abs() < 1e-12);
        assert!(items[6].as_f64().unwrap().is_finite());
        assert!(items[7].as_f64().unwrap().is_infinite());
        assert!((items[8].as_f64().unwrap() - 1.2345678901234568e19).abs() < 1.0);
    }

    #[test]
    fn u_escape_errors_keep_legacy_positions_and_messages() {
        // Round 3 word-loaded hex4_at must report byte-identical errors to the old walk
        // (positions/messages captured live from the pre-change snapshot in the A/B probe).
        let tail = parse_dom(b"\"\\u12\"").expect_err("short escape tail");
        assert_eq!(tail.msg, "Unterminated string or raw control byte");
        let bad2 = parse_dom(b"\"a\\u12g4b\"").expect_err("bad digit");
        assert_eq!(bad2.msg, "Invalid hex digit");
        assert_eq!(bad2.pos, 5, "first-bad-digit position must match legacy walk");
        let bad3 = parse_dom(b"\"\\uZZZZ\"").expect_err("bad digits");
        assert_eq!(bad3.msg, "Invalid hex digit");
        assert_eq!(bad3.pos, 2);
        // Mixed case hex stays valid and decodes identically (word path covers a-f/A-F lanes).
        let ok = parse_dom(b"\"\\u00E9\\uff4A\"").expect("mixed case escapes");
        assert_eq!(ok.as_str(), Some("éｊ"));
    }

    #[test]
    fn duplicate_keys_and_escaped_unicode_keys_survive_parse() {
        let src = r#"{"dup":1,"dup":2,"\u00e9x":"caf\u00e9","":"empty key"}"#;
        let v = parse_dom(src.as_bytes()).expect("keys");
        // Last duplicate wins, matching QuickJS object insertion order semantics.
        assert_eq!(v.get("dup").and_then(|x| x.as_f64()), Some(2.0));
        assert_eq!(v.get("éx").and_then(|x| x.as_str()), Some("café"));
        assert_eq!(v.get("").and_then(|x| x.as_str()), Some("empty key"));
    }
}

