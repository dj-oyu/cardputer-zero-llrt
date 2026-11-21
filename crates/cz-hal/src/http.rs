//! Minimal but strict HTTP/1.1 protocol layer for the on-device web server.
//!
//! This module is transport-agnostic: it only turns bytes into requests and responses back into
//! bytes. The TCP engine lives in [`crate::http_server`]. Design goals (matching LLRT on
//! CardputerZero):
//!
//! - **Keep-Alive**: one parser instance can consume many sequential requests from a single
//!   stream buffer, so a client can issue several requests over one TCP connection.
//! - **Body limits**: `Content-Length` and `Transfer-Encoding: chunked` bodies are capped at
//!   [`MAX_BODY_BYTES`] (1 MiB) so a bad client cannot exhaust the 512MB device.
//! - **Robustness**: malformed request lines / headers never panic; they surface as
//!   [`HttpError::BadRequest`] so the caller can answer `400 Bad Request` and drop that
//!   connection while the server keeps running.
//! - **No panics** on hostile input.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::io::Write as _;

/// Hard cap on decoded request body bytes (1 MiB) — protects the 512MB CardputerZero.
pub const MAX_BODY_BYTES: usize = 1_048_576;
/// Hard cap on the request header section (64 KiB).
pub const MAX_HEAD_BYTES: usize = 64 * 1024;
/// Hard cap on a single chunk-size line while decoding chunked bodies.
pub const MAX_CHUNK_LINE_BYTES: usize = 128;

/// Standard reason phrases (subset of Node.js `http.STATUS_CODES`).
pub const STATUS_CODES: &[(u16, &str)] = &[
    (100, "Continue"),
    (101, "Switching Protocols"),
    (200, "OK"),
    (201, "Created"),
    (202, "Accepted"),
    (204, "No Content"),
    (206, "Partial Content"),
    (301, "Moved Permanently"),
    (302, "Found"),
    (303, "See Other"),
    (304, "Not Modified"),
    (307, "Temporary Redirect"),
    (308, "Permanent Redirect"),
    (400, "Bad Request"),
    (401, "Unauthorized"),
    (402, "Payment Required"),
    (403, "Forbidden"),
    (404, "Not Found"),
    (405, "Method Not Allowed"),
    (406, "Not Acceptable"),
    (408, "Request Timeout"),
    (409, "Conflict"),
    (410, "Gone"),
    (411, "Length Required"),
    (412, "Precondition Failed"),
    (413, "Payload Too Large"),
    (414, "URI Too Long"),
    (415, "Unsupported Media Type"),
    (416, "Range Not Satisfiable"),
    (417, "Expectation Failed"),
    (418, "I'm a teapot"),
    (421, "Misdirected Request"),
    (422, "Unprocessable Entity"),
    (428, "Precondition Required"),
    (429, "Too Many Requests"),
    (431, "Request Header Fields Too Large"),
    (451, "Unavailable For Legal Reasons"),
    (500, "Internal Server Error"),
    (501, "Not Implemented"),
    (502, "Bad Gateway"),
    (503, "Service Unavailable"),
    (504, "Gateway Timeout"),
    (505, "HTTP Version Not Supported"),
];

/// Lookup the canonical reason phrase for a status code (`"Unknown"` when unmapped).
pub fn reason_phrase(code: u16) -> &'static str {
    for (c, phrase) in STATUS_CODES {
        if *c == code {
            return phrase;
        }
    }
    "Unknown"
}

/// One parsed request head (request line + header fields, names lowercased).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestHead {
    /// Uppercase method token, e.g. `"GET"`.
    pub method: String,
    /// Raw request target including any query string, e.g. `"/api/status?v=1"`.
    pub target: String,
    /// HTTP version digits without the `HTTP/` prefix, e.g. `"1.1"`.
    pub version: String,
    /// Header fields in wire order with lowercased names.
    pub headers: Vec<(String, String)>,
}

impl RequestHead {
    /// Case-insensitive iterator over all values for `name` in wire order (zero allocations).
    pub fn header_values<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.headers
            .iter()
            .filter(move |(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Case-insensitive lookup; duplicate fields are joined with `", "` like Node.js.
    pub fn header(&self, name: &str) -> Option<Cow<'_, str>> {
        let mut vals = self
            .headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str());
        let first = vals.next()?;
        match vals.next() {
            None => Some(Cow::Borrowed(first)),
            Some(second) => {
                let mut joined = String::with_capacity(first.len() + 2 + second.len());
                joined.push_str(first);
                joined.push_str(", ");
                joined.push_str(second);
                for v in vals {
                    joined.push_str(", ");
                    joined.push_str(v);
                }
                Some(Cow::Owned(joined))
            }
        }
    }

    /// True when the request is HTTP/1.0 (default connection semantics differ).
    pub fn is_http10(&self) -> bool {
        self.version == "1.0"
    }

    /// Whether this connection should stay open after the response (HTTP/1.1 default keep-alive,
    /// HTTP/1.0 default close unless the client asks for keep-alive).
    pub fn keep_alive(&self) -> bool {
        let mut saw_keep_alive = false;
        for v in self.header_values("connection") {
            if contains_ascii_ignore_case(v, "close") {
                return false;
            }
            if contains_ascii_ignore_case(v, "keep-alive") {
                saw_keep_alive = true;
            }
        }
        if saw_keep_alive {
            true
        } else {
            !self.is_http10()
        }
    }

    /// Client asked for `Expect: 100-continue` before the body is transmitted.
    pub fn expects_continue(&self) -> bool {
        let mut vals = self.header_values("expect");
        let Some(first) = vals.next() else {
            return false;
        };
        vals.next().is_none() && first.trim().eq_ignore_ascii_case("100-continue")
    }

    /// `Transfer-Encoding: chunked` present?
    pub fn chunked(&self) -> bool {
        self.header_values("transfer-encoding")
            .any(|v| contains_ascii_ignore_case(v, "chunked"))
    }

    /// Declared `Content-Length` when present and numeric.
    pub fn content_length(&self) -> Option<u64> {
        let mut vals = self.header_values("content-length");
        let first = vals.next()?;
        if vals.next().is_some() {
            return None;
        }
        first.trim().parse::<u64>().ok()
    }
}

fn contains_ascii_ignore_case(haystack: &str, needle: &str) -> bool {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() {
        return true;
    }
    h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

/// A fully parsed request (head + complete decoded body).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRequest {
    pub head: RequestHead,
    pub body: Vec<u8>,
    /// Whether the connection should remain open after responding.
    pub keep_alive: bool,
}

/// Protocol level failures that must be answered and then close the connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    /// Malformed request line, headers or chunk framing => `400 Bad Request`.
    BadRequest(String),
    /// Body exceeded [`MAX_BODY_BYTES`] => `413 Payload Too Large`.
    PayloadTooLarge(String),
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpError::BadRequest(m) => write!(f, "bad request: {m}"),
            HttpError::PayloadTooLarge(m) => write!(f, "payload too large: {m}"),
        }
    }
}

impl HttpError {
    pub fn status(&self) -> u16 {
        match self {
            HttpError::BadRequest(_) => 400,
            HttpError::PayloadTooLarge(_) => 413,
        }
    }
}

/// Progress of an incremental parse step.
#[derive(Debug, PartialEq, Eq)]
pub enum ParseStep {
    /// More bytes are required before the request is complete.
    NeedMore,
    /// The client set `Expect: 100-continue`; caller should write an interim `100 Continue` and
    /// keep feeding body bytes.
    InterimContinue,
    /// A complete request was parsed (leftover bytes stay buffered for pipelined requests).
    Request(ParsedRequest),
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum State {
    Head,
    /// Waiting for exactly `n` body bytes.
    Length(u64),
    Chunked(ChunkPhase),
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum ChunkPhase {
    SizeLine,
    Data(u64),
    DataEol,
    Trailers,
}

/// Incremental HTTP/1.1 request parser. Feed bytes with [`RequestParser::push`] and pull steps
/// with [`RequestParser::step`]. After a request completes the parser resets automatically so the
/// same instance keeps serving the connection (keep-alive / pipelining).
pub struct RequestParser {
    buf: VecDeque<u8>,
    state: State,
    /// Head parsed from the request currently being assembled (reset once the request completes).
    parsed_head: Option<RequestHead>,
    body: Vec<u8>,
    body_read: u64,
    continue_sent: bool,
    sticky: Option<HttpError>,
}

impl Default for RequestParser {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestParser {
    pub fn new() -> Self {
        Self {
            buf: VecDeque::new(),
            state: State::Head,
            parsed_head: None,
            body: Vec::new(),
            body_read: 0,
            continue_sent: false,
            sticky: None,
        }
    }

    /// Append raw bytes received from the stream.
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend(data.iter().copied());
    }

    /// Buffered (not yet consumed) byte count.
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// Bytes of request body decoded so far on the current request.
    pub fn body_progress(&self) -> u64 {
        self.body_read
    }

    /// A sticky fatal error short-circuits every later step (caller must close the connection).
    pub fn sticky_error(&self) -> Option<&HttpError> {
        self.sticky.as_ref()
    }

    /// Drive the state machine; returns `NeedMore` when more input is required. Any protocol
    /// violation becomes *sticky*: later steps keep reporting the same failure so the caller can
    /// answer once and drop the offending connection instead of resurrecting it.
    pub fn step(&mut self) -> Result<ParseStep, HttpError> {
        if let Some(err) = self.sticky.clone() {
            return Err(err);
        }
        let res = self.step_inner();
        if let Err(e) = &res {
            self.sticky = Some(e.clone());
        }
        res
    }

    fn step_inner(&mut self) -> Result<ParseStep, HttpError> {
        loop {
            match self.state {
                State::Head => match self.try_head()? {
                    Some(mode) => {
                        let wants_body = !matches!(mode, State::Length(0));
                        self.state = mode;
                        // Honor `Expect: 100-continue` before blocking on the body.
                        if wants_body && !self.continue_sent && self.head_expects_continue() {
                            self.continue_sent = true;
                            return Ok(ParseStep::InterimContinue);
                        }
                        continue;
                    }
                    None => return Ok(ParseStep::NeedMore),
                },
                State::Length(n) => {
                    let need = n.saturating_sub(self.body_read);
                    if need == 0 {
                        return self.finish_request();
                    }
                    let take = std::cmp::min(need as usize, self.buf.len());
                    if take == 0 {
                        return Ok(ParseStep::NeedMore);
                    }
                    self.append_body(take)?;
                    if self.body_read >= n {
                        return self.finish_request();
                    }
                    return Ok(ParseStep::NeedMore);
                }
                State::Chunked(phase) => match self.step_chunked(phase)? {
                    // Progress was made (a chunk line consumed or body bytes appended): keep
                    // driving the machine instead of falsely reporting `NeedMore`.
                    ChunkMove::Advanced(p) => {
                        self.state = State::Chunked(p);
                        continue;
                    }
                    ChunkMove::NeedMore(p) => {
                        self.state = State::Chunked(p);
                        return Ok(ParseStep::NeedMore);
                    }
                    ChunkMove::Done => return self.finish_request(),
                },
            }
        }
    }

    fn try_head(&mut self) -> Result<Option<State>, HttpError> {
        // Tolerate leading empty lines (some clients send stray CRLFs before a request).
        let skip = {
            let contig = self.buf.make_contiguous();
            let mut n = 0usize;
            while n < contig.len() {
                if contig[n] == b'\n' {
                    n += 1;
                } else if contig[n] == b'\r' && n + 1 < contig.len() && contig[n + 1] == b'\n' {
                    n += 2;
                } else {
                    break;
                }
            }
            n
        };
        if skip > 0 {
            self.buf.drain(..skip);
        }

        let (end, term_len) = match find_head_terminator(&mut self.buf) {
            Some(v) => v,
            None => {
                if self.buf.len() > MAX_HEAD_BYTES {
                    return Err(HttpError::BadRequest("header section too large".into()));
                }
                return Ok(None);
            }
        };

        if end > MAX_HEAD_BYTES {
            return Err(HttpError::BadRequest("header section too large".into()));
        }
        let head_res = parse_head_lines(&self.buf.make_contiguous()[..end]);
        self.buf.drain(..end + term_len);
        let head = head_res?;

        // Chunked transfer-encoding wins over Content-Length per RFC 9112 / Node.js behavior.
        let mode = if head.chunked() {
            State::Chunked(ChunkPhase::SizeLine)
        } else if let Some(cl) = head.content_length() {
            if cl > MAX_BODY_BYTES as u64 {
                return Err(HttpError::PayloadTooLarge(format!(
                    "content-length {cl} exceeds {} bytes",
                    MAX_BODY_BYTES
                )));
            } else if cl == 0 {
                State::Length(0)
            } else {
                State::Length(cl)
            }
        } else {
            State::Length(0)
        };
        self.parsed_head = Some(head);
        self.continue_sent = false;
        self.body.clear();
        self.body_read = 0;
        Ok(Some(mode))
    }

    fn head_expects_continue(&self) -> bool {
        self.parsed_head.as_ref().map(|h| h.expects_continue()).unwrap_or(false)
    }

    fn append_body(&mut self, take: usize) -> Result<(), HttpError> {
        let next = self.body_read + take as u64;
        if next > MAX_BODY_BYTES as u64 {
            return Err(HttpError::PayloadTooLarge(format!(
                "body exceeds {} bytes",
                MAX_BODY_BYTES
            )));
        }
        let (front, back) = self.buf.as_slices();
        let n1 = take.min(front.len());
        self.body.extend_from_slice(&front[..n1]);
        let n2 = take - n1;
        if n2 > 0 {
            self.body.extend_from_slice(&back[..n2]);
        }
        self.buf.drain(..take);
        self.body_read = next;
        Ok(())
    }

    fn step_chunked(&mut self, phase: ChunkPhase) -> Result<ChunkMove, HttpError> {
        match phase {
            ChunkPhase::SizeLine => {
                let (line_len, drain_len) = match peek_line_len(&mut self.buf) {
                    Some(v) => v,
                    None => {
                        if self.buf.len() > MAX_CHUNK_LINE_BYTES {
                            return Err(HttpError::BadRequest("chunk-size line too long".into()));
                        }
                        return Ok(ChunkMove::NeedMore(ChunkPhase::SizeLine));
                    }
                };
                let size_res = {
                    let line = &self.buf.make_contiguous()[..line_len];
                    let token = String::from_utf8_lossy(line);
                    let hex = token.split(';').next().unwrap_or("").trim();
                    u64::from_str_radix(hex, 16)
                        .map_err(|_| HttpError::BadRequest(format!("invalid chunk size {hex:?}")))
                };
                self.buf.drain(..drain_len);
                let size = size_res?;
                if self.body_read + size > MAX_BODY_BYTES as u64 {
                    return Err(HttpError::PayloadTooLarge(format!(
                        "chunked body exceeds {} bytes",
                        MAX_BODY_BYTES
                    )));
                }
                if size == 0 {
                    // The terminating chunk still needs its trailer section (up to one blank line).
                    return Ok(ChunkMove::Advanced(ChunkPhase::Trailers));
                }
                Ok(ChunkMove::Advanced(ChunkPhase::Data(size)))
            }
            ChunkPhase::Data(size) => {
                if size == 0 {
                    return Ok(ChunkMove::Advanced(ChunkPhase::DataEol));
                }
                let take = std::cmp::min(size as usize, self.buf.len());
                if take == 0 {
                    return Ok(ChunkMove::NeedMore(ChunkPhase::Data(size)));
                }
                self.append_body(take)?;
                let remaining = size - take as u64;
                if remaining == 0 {
                    Ok(ChunkMove::Advanced(ChunkPhase::DataEol))
                } else {
                    Ok(ChunkMove::Advanced(ChunkPhase::Data(remaining)))
                }
            }
            ChunkPhase::DataEol => match peek_line_len(&mut self.buf) {
                Some((_, drain_len)) => {
                    self.buf.drain(..drain_len);
                    Ok(ChunkMove::Advanced(ChunkPhase::SizeLine))
                }
                None => {
                    if self.buf.len() > MAX_CHUNK_LINE_BYTES {
                        return Err(HttpError::BadRequest("chunk framing error: missing CRLF".into()));
                    }
                    Ok(ChunkMove::NeedMore(ChunkPhase::DataEol))
                }
            },
            ChunkPhase::Trailers => match peek_line_len(&mut self.buf) {
                Some((line_len, drain_len)) => {
                    self.buf.drain(..drain_len);
                    if line_len == 0 {
                        Ok(ChunkMove::Done)
                    } else {
                        Ok(ChunkMove::Advanced(ChunkPhase::Trailers))
                    }
                }
                None => {
                    if self.buf.len() > MAX_HEAD_BYTES {
                        return Err(HttpError::BadRequest("trailer section too large".into()));
                    }
                    Ok(ChunkMove::NeedMore(ChunkPhase::Trailers))
                }
            },
        }
    }

    fn finish_request(&mut self) -> Result<ParseStep, HttpError> {
        let head = self
            .parsed_head
            .take()
            .ok_or_else(|| HttpError::BadRequest("internal parser state".into()))?;
        let keep_alive = head.keep_alive();
        let body = std::mem::take(&mut self.body);
        // Reset for the next request on this (keep-alive) connection.
        self.state = State::Head;
        self.body_read = 0;
        self.continue_sent = false;
        Ok(ParseStep::Request(ParsedRequest { head, body, keep_alive }))
    }
}

// Small private helpers -------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
enum ChunkMove {
    /// Some buffered input was consumed; the caller should step again immediately.
    Advanced(ChunkPhase),
    /// Genuinely waiting for more stream bytes.
    NeedMore(ChunkPhase),
    /// Terminating `0\r\n\r\n` chunk plus trailers complete.
    Done,
}

/// Find the end of the header section: either `\r\n\r\n` or bare `\n\n`, whichever comes first.
/// Returns `(header_block_end_index, terminator_length)`.
///
/// Round 3 (Stall #1 kill): instead of two byte-at-a-time `find_subslice` walks over the whole
/// buffer, one vectorized newline-candidate scan (`scan_newline`) drives exact-local checks at
/// each `\n`. Matches are discovered in ascending START order (`\n\n` starts at the candidate,
/// `\r\n\r\n` starts one byte earlier), so the first event reported equals the legacy
/// `min(crlf_first, lf_first)` with crlf winning a tie. Semantics identical, including the
/// "pattern must be fully present" rule.
fn find_head_terminator(buf: &mut VecDeque<u8>) -> Option<(usize, usize)> {
    let contig = buf.make_contiguous();
    let n = contig.len();
    let mut p = scan_newline(contig, 0);
    loop {
        let Some(nl) = p else { return None };
        // Bare \n\n at `nl`: needs a second newline immediately after it.
        if nl + 1 < n && contig[nl + 1] == b'\n' {
            return Some((nl, 2));
        }
        // \r\n\r\n starting at nl - 1: \r before, \r then \n two bytes ahead.
        if nl >= 1
            && nl + 2 < n
            && contig[nl - 1] == b'\r'
            && contig[nl + 1] == b'\r'
            && contig[nl + 2] == b'\n'
        {
            return Some((nl - 1, 4));
        }
        p = scan_newline(&contig[nl + 1..], 0).map(|q| q + nl + 1);
    }
}

/// Index of the first `\n` at or after `start`, or `None`. ARMv8.0 NEON (32B interleaved, then
/// 16B) on aarch64; identical-semantics SWAR words elsewhere; scalar tail keeps exact parity.
fn scan_newline(bytes: &[u8], start: usize) -> Option<usize> {
    let n = bytes.len();
    if start >= n {
        return None;
    }
    #[cfg(target_arch = "aarch64")]
    {
        use core::arch::aarch64::{
            vbslq_u8, vceqq_u8, vdupq_n_u8, vld1q_u8, vminvq_u8, vmaxvq_u8,
        };
        let lanes: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        let mut i = start;
        unsafe {
            let p = bytes.as_ptr();
            let lane_v = vld1q_u8(lanes.as_ptr());
            let nl_v = vdupq_n_u8(b'\n');
            let all_ones = vdupq_n_u8(u8::MAX);
            while i + 32 <= n {
                // Two independent loads back-to-back hide the A53 Load-to-Use stall.
                let v0 = vld1q_u8(p.add(i));
                let v1 = vld1q_u8(p.add(i + 16));
                let hit0 = vceqq_u8(v0, nl_v);
                let hit1 = vceqq_u8(v1, nl_v);
                if vmaxvq_u8(hit0) == 0 && vmaxvq_u8(hit1) == 0 {
                    i += 32;
                    continue;
                }
                if vmaxvq_u8(hit0) != 0 {
                    return Some(i + vminvq_u8(vbslq_u8(hit0, lane_v, all_ones)) as usize);
                }
                return Some(i + 16 + vminvq_u8(vbslq_u8(hit1, lane_v, all_ones)) as usize);
            }
            while i + 16 <= n {
                let v = vld1q_u8(p.add(i));
                let hit = vceqq_u8(v, nl_v);
                if vmaxvq_u8(hit) != 0 {
                    return Some(i + vminvq_u8(vbslq_u8(hit, lane_v, all_ones)) as usize);
                }
                i += 16;
            }
        }
        while i < n {
            if bytes[i] == b'\n' {
                return Some(i);
            }
            i += 1;
        }
        None
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        const ONES: u64 = 0x0101010101010101;
        const HIGH: u64 = 0x8080808080808080;
        #[inline]
        fn swar_eq(w: u64, v: u8) -> u64 {
            let x = w ^ (ONES * u64::from(v));
            x.wrapping_sub(ONES) & !x & HIGH
        }
        #[inline]
        fn load8(b: &[u8], i: usize) -> u64 {
            // SAFETY: read_unaligned handles any alignment; callers guarantee len - i >= 8.
            unsafe { core::ptr::read_unaligned(b.as_ptr().add(i) as *const u64) }
        }
        let mut i = start;
        while i + 16 <= n {
            let w0 = load8(bytes, i);
            let w1 = load8(bytes, i + 8);
            let m0 = swar_eq(w0, b'\n');
            let m1 = swar_eq(w1, b'\n');
            if (m0 | m1) == 0 {
                i += 16;
                continue;
            }
            if m0 != 0 {
                return Some(i + (m0.trailing_zeros() / 8) as usize);
            }
            return Some(i + 8 + (m1.trailing_zeros() / 8) as usize);
        }
        while i + 8 <= n {
            let m = swar_eq(load8(bytes, i), b'\n');
            if m != 0 {
                return Some(i + (m.trailing_zeros() / 8) as usize);
            }
            i += 8;
        }
        while i < n {
            if bytes[i] == b'\n' {
                return Some(i);
            }
            i += 1;
        }
        None
    }
}

/// Inspect the next `\r\n` or `\n` terminated line at the front of `buf` without allocating.
/// Returns `(content_len_without_cr, drain_len_including_lf)` when a full line has arrived.
fn peek_line_len(buf: &mut VecDeque<u8>) -> Option<(usize, usize)> {
    let contig = buf.make_contiguous();
    let nl = scan_newline(contig, 0)?;
    let content_len = if nl > 0 && contig[nl - 1] == b'\r' { nl - 1 } else { nl };
    Some((content_len, nl + 1))
}

/// Remove one header/trailer line (`\r\n` or `\n` terminated) from the front of `buf`.
/// Returns `None` when the terminator has not arrived yet.
#[cfg(test)]
fn take_line(buf: &mut VecDeque<u8>) -> Option<Vec<u8>> {
    let contig = buf.make_contiguous();
    let nl = scan_newline(contig, 0)?;
    let mut line: Vec<u8> = contig[..nl].to_vec();
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    buf.drain(..nl + 1);
    Some(line)
}

/// Parse the request line plus header fields out of raw head bytes.
fn parse_head_lines(head_bytes: &[u8]) -> Result<RequestHead, HttpError> {
    let text = String::from_utf8_lossy(head_bytes);
    let mut lines = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l));
    let req_line = lines
        .next()
        .ok_or_else(|| HttpError::BadRequest("missing request line".into()))?;
    let mut parts = req_line.split_whitespace();
    let (Some(raw_method), Some(raw_target), Some(version_token)) =
        (parts.next(), parts.next(), parts.next())
    else {
        return Err(HttpError::BadRequest(format!(
            "invalid request line {req_line:?}"
        )));
    };
    if parts.next().is_some() {
        return Err(HttpError::BadRequest(format!(
            "invalid request line {req_line:?}"
        )));
    }
    if !version_token.starts_with("HTTP/1.") || version_token.len() < 8 {
        return Err(HttpError::BadRequest(format!(
            "unsupported http version {version_token:?}"
        )));
    }
    let minor = &version_token[7..];
    if minor.len() > 1 || !minor.chars().all(|c| c.is_ascii_digit()) {
        return Err(HttpError::BadRequest(format!(
            "unsupported http version {version_token:?}"
        )));
    }
    let method = raw_method.to_ascii_uppercase();
    if method.is_empty() || !method.chars().all(|c| c.is_ascii_graphic()) {
        return Err(HttpError::BadRequest("invalid method token".into()));
    }
    if raw_target.is_empty() {
        return Err(HttpError::BadRequest("empty request target".into()));
    }

    let mut headers: Vec<(String, String)> = Vec::with_capacity(16);
    for line in lines {
        if line.is_empty() {
            break;
        }
        // Obs-fold continuation lines (starting with space/tab) are appended to the last value.
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(last) = headers.last_mut() {
                last.1.push(' ');
                last.1.push_str(line.trim());
                continue;
            }
            return Err(HttpError::BadRequest("header continuation without field".into()));
        }
        let idx = line
            .find(':')
            .ok_or_else(|| HttpError::BadRequest(format!("invalid header field {line:?}")))?;
        let name = line[..idx].trim().to_ascii_lowercase();
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_graphic() && c != ':') {
            return Err(HttpError::BadRequest(format!("invalid header name {line:?}")));
        }
        let value = line[idx + 1..].trim().to_string();
        headers.push((name, value));
    }

    let mut version = String::with_capacity(2 + minor.len());
    version.push_str("1.");
    version.push_str(minor);

    Ok(RequestHead {
        method,
        target: raw_target.to_string(),
        version,
        headers,
    })
}

/// A response frame ready to be serialized onto a TCP stream.
#[derive(Debug, Clone)]
pub struct ResponseFrame {
    pub status: u16,
    /// Optional explicit reason phrase; defaults to [`reason_phrase`].
    pub reason: Option<String>,
    /// Header fields in emission order (user supplied).
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Whether the connection stays open after this response.
    pub keep_alive: bool,
}

impl ResponseFrame {
    /// Wire bytes with `Content-Length` auto-set and a correct `Connection` header.
    pub fn encode(&self) -> Vec<u8> {
        serialize_response(self.status, self.reason.as_deref(), &self.headers, &self.body, self.keep_alive)
    }

    /// Simple text/plain error page for protocol failures.
    pub fn error(status: u16, detail: &str, keep_alive: bool) -> Self {
        let body = format!("{status} {}\n{}", reason_phrase(status), detail);
        Self {
            status,
            reason: None,
            headers: vec![("content-type".into(), "text/plain; charset=utf-8".into())],
            body: body.into_bytes(),
            keep_alive,
        }
    }
}

fn write_filtered_headers(
    out: &mut Vec<u8>,
    headers: &[(String, String)],
    skip_content_length: bool,
) -> bool {
    let mut has_len = false;
    let mut seen: Vec<&str> = Vec::with_capacity(headers.len());
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("connection")
            || name.eq_ignore_ascii_case("transfer-encoding")
            || (skip_content_length && name.eq_ignore_ascii_case("content-length"))
            || seen.iter().any(|s| s.eq_ignore_ascii_case(name))
        {
            continue;
        }
        seen.push(name.as_str());
        // Header names must be field-content safe; skip anything hostile instead of crashing.
        if name.chars().all(|c| c.is_ascii_graphic() && c != ':')
            && !value.contains('\n')
            && !value.contains('\r')
        {
            out.reserve(name.len() + value.len() + 4);
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(b": ");
            out.extend_from_slice(value.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        if name.eq_ignore_ascii_case("content-length") {
            has_len = true;
        }
    }
    has_len
}

/// Serialize a complete HTTP/1.1 response. `Content-Length` is added when absent and any
/// caller-supplied `Connection` / duplicated reserved headers are dropped in favor of the engine
/// decision (`Connection: keep-alive` or `Connection: close`).
pub fn serialize_response(
    status: u16,
    reason: Option<&str>,
    headers: &[(String, String)],
    body: &[u8],
    keep_alive: bool,
) -> Vec<u8> {
    let phrase = reason.unwrap_or_else(|| reason_phrase(status));
    // H23 (semantics-identical): exact-capacity reserve and direct byte emission instead of a
    // format! allocation per line. Dedup/skip rules are byte-for-byte the original logic.
    let mut out: Vec<u8> = Vec::with_capacity(64 + headers.len() * 40 + body.len());
    let _ = write!(out, "HTTP/1.1 {status} {phrase}\r\n");

    let has_len = write_filtered_headers(&mut out, headers, false);
    if !has_len {
        let _ = write!(out, "Content-Length: {}\r\n", body.len());
    }
    out.extend_from_slice(if keep_alive {
        b"Connection: keep-alive\r\n\r\n"
    } else {
        b"Connection: close\r\n\r\n"
    });
    out.extend_from_slice(body);
    out
}

/// Serialize only the status line + headers for a **streaming** (chunked) response. The engine
/// reserves `Content-Length`, `Transfer-Encoding` and `Connection`: caller-supplied copies of
/// those are dropped, `Transfer-Encoding: chunked` is emitted instead of any auto
/// `Content-Length`, and the `Connection` header follows the engine's keep-alive decision.
pub fn serialize_stream_head(
    status: u16,
    reason: Option<&str>,
    headers: &[(String, String)],
    keep_alive: bool,
) -> Vec<u8> {
    let phrase = reason.unwrap_or_else(|| reason_phrase(status));
    // H23 idiom mirrored to the streaming head: capacity reserve + direct emission, no format!
    // per line; skip/dedup semantics unchanged.
    let mut out: Vec<u8> = Vec::with_capacity(72 + headers.len() * 40);
    let _ = write!(out, "HTTP/1.1 {status} {phrase}\r\n");
    write_filtered_headers(&mut out, headers, true);
    out.extend_from_slice(b"Transfer-Encoding: chunked\r\n");
    out.extend_from_slice(if keep_alive {
        b"Connection: keep-alive\r\n\r\n"
    } else {
        b"Connection: close\r\n\r\n"
    });
    out
}

/// Encode one HTTP/1.1 chunk frame: `<hex-size>\r\n<data>\r\n`.
pub fn chunk_frame(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(18 + data.len());
    let _ = write!(out, "{:x}\r\n", data.len());
    out.extend_from_slice(data);
    out.extend_from_slice(b"\r\n");
    out
}

/// The terminating zero-length chunk plus empty trailer section (`0\r\n\r\n`).
pub const LAST_CHUNK_FRAME: &[u8] = b"0\r\n\r\n";

/// Interim `HTTP/1.1 100 Continue` wire bytes for `Expect: 100-continue` clients.
pub const CONTINUE_FRAME: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";

/// Build an interim `HTTP/1.1 100 Continue` frame for `Expect: 100-continue` clients.
pub fn continue_frame() -> Vec<u8> {
    CONTINUE_FRAME.to_vec()
}

/// Convenience: turn a protocol error into the bytes that must be written before closing.
pub fn error_response(err: &HttpError) -> Vec<u8> {
    ResponseFrame::error(err.status(), &err.to_string(), false).encode()
}

#[cfg(test)]
mod http_tests {
    use super::*;

    fn feed_all(src: &str) -> RequestParser {
        let mut p = RequestParser::new();
        p.push(src.as_bytes());
        p
    }

    fn one_request(src: &str) -> Result<ParsedRequest, HttpError> {
        let mut p = feed_all(src);
        loop {
            match p.step()? {
                ParseStep::NeedMore => return Err(HttpError::BadRequest("incomplete fixture".into())),
                ParseStep::InterimContinue => continue,
                ParseStep::Request(r) => return Ok(r),
            }
        }
    }

    fn legacy_find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        if needle.len() > haystack.len() {
            return None;
        }
        for i in 0..=(haystack.len() - needle.len()) {
            if &haystack[i..i + needle.len()] == needle {
                return Some(i);
            }
        }
        None
    }

    fn legacy_head_terminator(contig: &[u8]) -> Option<(usize, usize)> {
        let crlf = legacy_find_subslice(contig, b"\r\n\r\n");
        let lf = legacy_find_subslice(contig, b"\n\n");
        match (crlf, lf) {
            (Some(a), Some(b)) => Some(if a <= b { (a, 4) } else { (b, 2) }),
            (Some(a), None) => Some((a, 4)),
            (None, Some(b)) => Some((b, 2)),
            (None, None) => None,
        }
    }

    #[test]
    fn scan_newline_matches_naive_index_on_tricky_buffers() {
        // Mixes CRLF clusters, lone LF/CR, padding runs and every chunk-boundary offset class.
        let mut buf: Vec<u8> = Vec::new();
        let mut s = 0xE5A5AA5Au32;
        for block in 0..40usize {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            match block % 7 {
                0 => buf.extend_from_slice(b"\r\n\r\n"),
                1 => buf.extend_from_slice(b"\n\n"),
                2 => buf.extend_from_slice(b"\r"),
                3 => buf.push(b'\n'),
                _ => buf.extend_from_slice(b"X-Multi-Word-Header-Line: value0123456789!"),
            }
            if s & 3 == 0 {
                buf.push(b'\n');
            }
        }
        for start in 0..=buf.len() {
            assert_eq!(
                scan_newline(&buf, start),
                buf[start..].iter().position(|b| *b == b'\n').map(|p| p + start),
                "scan_newline mismatch at start={start}"
            );
        }
    }

    #[test]
    fn head_terminator_and_take_line_match_legacy_on_wrapped_buffers() {
        // Deterministic fuzz: every pattern class, compared against the legacy two-subsearch
        // algorithm, including VecDeque states whose data wraps (non-contiguous ring layout).
        let mut s = 0xC0FFEE11u32;
        for trial in 0..240usize {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            let mut data: Vec<u8> = Vec::new();
            for piece in 0..(4 + (s as usize % 6)) {
                match ((s >> (piece * 3)) % 9) as u8 {
                    0 => data.extend_from_slice(b"GET / HTTP/1.1\r\n"),
                    1 => data.extend_from_slice(b"Host: x\r\n"),
                    2 => data.extend_from_slice(b"\r\n"),
                    3 => data.extend_from_slice(b"\n"),
                    4 => data.extend_from_slice(b"a\n"),
                    5 => data.extend_from_slice(b"\r\n\r\n"),
                    6 => data.extend_from_slice(b"long padding value line for vector lanes!!"),
                    7 => data.push(b'\r'),
                    _ => data.extend_from_slice(b"\n\r"),
                }
            }
            let want = legacy_head_terminator(&data);
            // Force a wrapped ring layout on odd trials: push, drain a prefix, push the rest,
            // then compare BOTH implementations on the same logical queue content.
            let mut q = VecDeque::with_capacity(16);
            if trial % 2 == 1 && data.len() > 10 {
                q.extend(data[..6].iter().copied());
                q.drain(..4);
                q.extend(data[6..].iter().copied());
                let lin = q.make_contiguous().to_vec();
                assert_eq!(find_head_terminator(&mut q), legacy_head_terminator(&lin), "wrapped terminator mismatch trial={trial}");
                continue;
            } else {
                q.extend(data.iter().copied());
            }
            assert_eq!(find_head_terminator(&mut q), want, "terminator mismatch trial={trial}");
            // take_line parity over the first line of the same buffer state.
            let mut q2 = VecDeque::<u8>::with_capacity(16);
            q2.extend(data.iter().copied());
            let nl_ref = data.iter().position(|b| *b == b'\n');
            let got = take_line(&mut q2);
            match nl_ref {
                Some(nl) => {
                    let line = &data[..nl];
                    let expect = if line.last() == Some(&b'\r') { &line[..line.len() - 1] } else { line };
                    assert_eq!(got.as_deref(), Some(expect), "take_line mismatch trial={trial}");
                }
                None => assert_eq!(got, None, "take_line must be None trial={trial}"),
            }
        }
    }

    #[test]
    fn request_line_headers_and_lowercase_names() {
        let r = one_request(
            "GET /api/status?v=1&src=tailscale HTTP/1.1\r\nHost: cz0\r\nX-Test-Client: phone\r\nUser-Agent: curl/8\r\n\r\n",
        )
        .expect("parse");
        assert_eq!(r.head.method, "GET");
        assert_eq!(r.head.target, "/api/status?v=1&src=tailscale");
        assert_eq!(r.head.version, "1.1");
        assert_eq!(r.body, Vec::<u8>::new());
        assert_eq!(r.head.header("x-test-client").as_deref(), Some("phone"));
        assert_eq!(r.head.header("X-TEST-CLIENT").as_deref(), Some("phone"));
        assert!(r.keep_alive, "HTTP/1.1 defaults to keep-alive");
        // Header order preserved and names lowercased on the wire struct.
        assert_eq!(r.head.headers[0].0, "host");
    }

    #[test]
    fn content_length_body_delimited_exactly_then_pipelining() {
        let mut p = feed_all(
            "POST /echo HTTP/1.1\r\nHost: cz0\r\nContent-Length: 5\r\n\r\nhelloGET /next HTTP/1.1\r\nHost: cz0\r\nConnection: close\r\n\r\n",
        );
        let first = match p.step() {
            Ok(ParseStep::Request(r)) => r,
            other => panic!("expected first request, got {other:?}"),
        };
        assert_eq!(first.head.method, "POST");
        assert_eq!(first.body, b"hello".to_vec());
        assert!(first.keep_alive);
        // Leftover bytes remain buffered: a second sequential request parses (keep-alive).
        let second = match p.step() {
            Ok(ParseStep::Request(r)) => r,
            other => panic!("expected pipelined request, got {other:?}"),
        };
        assert_eq!(second.head.target, "/next");
        assert!(!second.keep_alive, "Connection: close honored");
        assert_eq!(p.buffered(), 0);
    }

    #[test]
    fn http10_defaults_to_close_and_keepalive_header_overrides() {
        let a = one_request("GET / HTTP/1.0\r\nHost: cz0\r\n\r\n").expect("1.0 parse");
        assert!(!a.keep_alive);
        let b = one_request("GET / HTTP/1.0\r\nHost: cz0\r\nConnection: keep-alive\r\n\r\n").expect("parse");
        assert!(b.keep_alive);
        assert_eq!(b.head.version, "1.0");
    }

    #[test]
    fn chunked_body_two_chunks_with_trailer_and_keepalive() {
        let wire = concat!(
            "POST /upload HTTP/1.1\r\nHost: cz0\r\nTransfer-Encoding: chunked\r\n\r\n",
            "6\r\nabc123\r\n",
            "13\r\nsecond-chunk-value!\r\n", // 0x13 == 19 bytes, exactly this payload
            "0\r\nX-Trail: yes\r\n\r\n",
            "GET /after HTTP/1.1\r\nHost: cz0\r\n\r\n",
        );
        let mut p = feed_all(wire);
        let req = loop {
            match p.step() {
                Ok(ParseStep::Request(r)) => break r,
                Ok(other) => panic!("unexpected step {other:?}"),
                Err(e) => panic!("chunked parse failed: {e}"),
            }
        };
        assert_eq!(req.body, b"abc123second-chunk-value!".to_vec());
        assert!(req.keep_alive);
        match p.step() {
            Ok(ParseStep::Request(next)) => assert_eq!(next.head.target, "/after"),
            other => panic!("expected follow-up request after chunked body, got {other:?}"),
        }
    }

    #[test]
    fn chunk_extensions_and_hex_are_accepted() {
        // Chunk-size line "A;ext=1;x" declares 10 bytes, matching the 10-byte payload exactly.
        let r = one_request(
            "POST /x HTTP/1.1\r\nHost: cz0\r\nTransfer-Encoding: chunked\r\n\r\nA;ext=1;x\r\n0123456789\r\n0\r\n\r\n",
        )
        .expect("parse with extension");
        assert_eq!(r.body, b"0123456789".to_vec());
    }

    #[test]
    fn bare_lf_line_endings_tolerated() {
        let r = one_request("GET /lf HTTP/1.1\nHost: cz0\nContent-Length: 2\n\nhi").expect("bare lf parse");
        assert_eq!(r.head.target, "/lf");
        assert_eq!(r.body, b"hi".to_vec());
    }

    #[test]
    fn malformed_request_lines_yield_bad_request() {
        for bad in [
            "GARBAGE_NOT_HTTP\r\n\r\n",
            "GET HTTP/1.1\r\n\r\n",
            "GET /x\r\n\r\n",
            "WEIRD /x HTTP/2.0\r\n\r\n",
        ] {
            let err = one_request(bad).expect_err("must be rejected");
            assert_eq!(err.status(), 400, "fixture {bad:?} -> {err}");
            assert!(matches!(err, HttpError::BadRequest(_)));
        }
    }

    #[test]
    fn malformed_header_fields_and_continuation_yield_bad_request() {
        let err = one_request("GET /x HTTP/1.1\r\nHost cz0\r\n\r\n").expect_err("header without colon");
        assert_eq!(err.status(), 400);
        let err2 = one_request(" \tstray continuation first\r\nHost: cz0\r\n\r\n")
            .expect_err("continuation without field");
        assert_eq!(err2.status(), 400);
        let ok = one_request("GET /x HTTP/1.1\r\nHost: cz0\r\nX-Long: one\r\n\tfolded\r\n\r\n")
            .expect("obs-fold accepted");
        assert_eq!(ok.head.header("x-long").as_deref(), Some("one folded"));
    }

    #[test]
    fn invalid_chunk_framing_is_bad_request() {
        let err = one_request(
            "POST /x HTTP/1.1\r\nHost: cz0\r\nTransfer-Encoding: chunked\r\n\r\nZZ\r\nhello\r\n",
        )
        .expect_err("hex parse must fail");
        assert_eq!(err.status(), 400);
        assert!(format!("{err}").contains("chunk size"));
    }

    #[test]
    fn content_length_beyond_limit_is_payload_too_large() {
        let err = one_request(&format!(
            "POST /big HTTP/1.1\r\nHost: cz0\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        ))
        .expect_err("oversized CL");
        assert_eq!(err.status(), 413);
        assert!(matches!(err, HttpError::PayloadTooLarge(_)));
        // Sticky: the parser refuses further work so the connection is closed.
        let again = RequestParser::new();
        assert!(again.sticky_error().is_none());
    }

    #[test]
    fn oversized_declared_length_fails_before_any_body_bytes_arrive() {
        // A client announcing 2 MiB is rejected at head-parse time; the body never arrives.
        let mut p = RequestParser::new();
        p.push(b"POST /big HTTP/1.1\r\nHost: cz0\r\nContent-Length: 2097152\r\n\r\n");
        let err = p.step().expect_err("declared length above the cap is fatal");
        assert_eq!(err.status(), 413);
        assert_eq!(p.body_progress(), 0);
        // Sticky: repeated steps and later input keep reporting the same failure.
        assert_eq!(p.step().expect_err("sticky failure").status(), 413);
        p.push(b"GET /x HTTP/1.1\r\nHost: cz0\r\n\r\n");
        assert_eq!(p.step().expect_err("still sticky").status(), 413);
    }

    #[test]
    fn declared_length_beyond_cap_rejects_partial_body_streams() {
        let mut p = RequestParser::new();
        p.push(b"PUT /u HTTP/1.1\r\nHost: cz0\r\nContent-Length: 3000000\r\n\r\npartial-body-so-far");
        let err = p.step().expect_err("oversized PUT rejected before its body completes");
        assert_eq!(err.status(), 413);
        assert_eq!(p.body_progress(), 0, "must not buffer bytes past the cap");
    }

    #[test]
    fn chunked_body_boundary_exactly_one_mebibyte_then_plus_one() {
        // First chunk declares exactly MAX_BODY_BYTES (allowed), then a 1-byte chunk crosses it.
        let mut p = RequestParser::new();
        p.push(
            format!("PUT /u HTTP/1.1\r\nHost: cz0\r\nTransfer-Encoding: chunked\r\n\r\n{}\r\n", "100000")
                .as_bytes(),
        );
        assert_eq!(p.step().unwrap(), ParseStep::NeedMore, "size line alone is incomplete");
        p.push(&vec![b'z'; MAX_BODY_BYTES]);
        p.push(b"\r\n");
        assert_eq!(p.step().unwrap(), ParseStep::NeedMore);
        assert_eq!(p.body_progress(), MAX_BODY_BYTES as u64, "exact cap accepted");
        p.push(b"1\r\ny\r\n");
        let err = p.step().expect_err("one extra decoded byte must be rejected");
        assert_eq!(err.status(), 413);
        assert_eq!(p.body_progress(), MAX_BODY_BYTES as u64, "no bytes stored past the cap");
        // Sticky after the violation, even if more (valid-looking) input arrives.
        p.push(b"GET /x HTTP/1.1\r\nHost: cz0\r\n\r\n");
        assert_eq!(p.step().expect_err("sticky after 413").status(), 413);
    }

    #[test]
    fn header_section_over_cap_is_bad_request() {
        let mut p = RequestParser::new();
        let huge = "X-Pad: ".to_string() + &"a".repeat(MAX_HEAD_BYTES + 10) + "\r\n\r\n";
        p.push(format!("GET /x HTTP/1.1\r\nHost: cz0\r\n{huge}").as_bytes());
        let err = p.step().expect_err("oversized header section");
        assert_eq!(err.status(), 400);
    }

    #[test]
    fn expect_100_continue_emits_interim_then_full_request() {
        let mut p = RequestParser::new();
        p.push(b"PUT /up HTTP/1.1\r\nHost: cz0\r\nExpect: 100-continue\r\nContent-Length: 4\r\n\r\n");
        assert_eq!(p.step().unwrap(), ParseStep::InterimContinue);
        assert_eq!(p.step().unwrap(), ParseStep::NeedMore);
        p.push(b"data");
        match p.step().unwrap() {
            ParseStep::Request(r) => {
                assert_eq!(r.head.method, "PUT");
                assert_eq!(r.body, b"data".to_vec());
            }
            other => panic!("expected request after continue, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_headers_join_and_transfer_encoding_wins_over_length() {
        let r = one_request(
            "POST /x HTTP/1.1\r\nHost: a\r\nHost: b\r\nContent-Length: 99\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n",
        )
        .expect("parse");
        assert_eq!(r.head.header("host").as_deref(), Some("a, b"));
        assert_eq!(r.body, b"abc".to_vec());
    }

    #[test]
    fn header_lookup_borrows_single_match_and_iterates_values_zero_alloc() {
        let r = one_request(
            "GET /x HTTP/1.1\r\nHost: cz0\r\nX-Dup: one\r\nX-Dup: two\r\nX-Dup: three\r\nExpect:  100-CONTINUE \r\n\r\n",
        )
        .expect("parse");
        assert!(r.head.header("missing").is_none());
        assert!(matches!(r.head.header("HOST"), Some(Cow::Borrowed("cz0"))));
        assert!(matches!(r.head.header("x-dup"), Some(Cow::Owned(ref s)) if s == "one, two, three"));
        let dups: Vec<&str> = r.head.header_values("X-DUP").collect();
        assert_eq!(dups, vec!["one", "two", "three"]);
        assert!(r.head.expects_continue());
    }

    #[test]
    fn response_serializer_sets_content_length_and_connection() {
        let bytes = serialize_response(
            200,
            None,
            &[("Content-Type".into(), "application/json".into())],
            b"{\"ok\":true}",
            true,
        );
        let text = String::from_utf8(bytes).expect("utf8 frame");
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Type: application/json\r\n"));
        assert!(text.contains("Content-Length: 11\r\n")); // {"ok":true} is exactly 11 bytes
        assert!(text.contains("Connection: keep-alive\r\n\r\n"));
        assert!(text.ends_with("{\"ok\":true}"));
    }

    #[test]
    fn response_serializer_respects_user_length_and_custom_reason_and_dedupes() {
        let bytes = serialize_response(
            404,
            Some("Nope"),
            &[
                ("Content-Length".into(), "3".into()),
                ("content-length".into(), "9".into()),
                ("Connection".into(), "close".into()),
                ("X-Powered-By".into(), "CardputerZero-LLRT".into()),
            ],
            b"abc",
            false,
        );
        let text = String::from_utf8(bytes).expect("utf8");
        assert!(text.starts_with("HTTP/1.1 404 Nope\r\n"));
        assert_eq!(text.matches("Content-Length:").count(), 1);
        assert!(text.contains("Content-Length: 3\r\n"));
        assert!(text.contains("Connection: close\r\n\r\n"));
        assert!(text.contains("X-Powered-By: CardputerZero-LLRT\r\n"));
    }

    #[test]
    fn reason_phrase_table_and_error_pages() {
        for code in [200u16, 201, 204, 301, 302, 304, 400, 401, 403, 404, 405, 413, 500, 502, 503] {
            assert_ne!(reason_phrase(code), "Unknown", "code {code} must be mapped");
        }
        assert_eq!(reason_phrase(413), "Payload Too Large");
        assert_eq!(reason_phrase(666), "Unknown");
        let frame = ResponseFrame::error(413, "too big", false).encode();
        let text = String::from_utf8(frame).expect("utf8");
        assert!(text.starts_with("HTTP/1.1 413 Payload Too Large\r\n"));
        assert!(text.contains("Connection: close"));
        let bad = error_response(&HttpError::BadRequest("boom".into()));
        assert!(String::from_utf8(bad).expect("utf8").starts_with("HTTP/1.1 400 Bad Request\r\n"));
        assert_eq!(continue_frame(), b"HTTP/1.1 100 Continue\r\n\r\n".to_vec());
    }

    #[test]
    fn status_codes_table_shape_for_js_export() {
        let mut seen = std::collections::HashSet::new();
        for (code, phrase) in STATUS_CODES {
            assert!(!phrase.is_empty());
            assert!(seen.insert(*code), "duplicate status code {code}");
        }
        assert!(seen.contains(&200) && seen.contains(&404) && seen.contains(&413));
    }

    #[test]
    fn stream_head_forces_chunked_and_reserves_reserved_headers() {
        let bytes = serialize_stream_head(
            200,
            None,
            &[
                ("Content-Type".into(), "text/event-stream; charset=utf-8".into()),
                ("Cache-Control".into(), "no-cache".into()),
                // Reserved/duplicate fields must be dropped in favor of the engine decision.
                ("content-length".into(), "42".into()),
                ("Transfer-Encoding".into(), "gzip".into()),
                ("Connection".into(), "close".into()),
                ("X-Powered-By".into(), "Cz0".into()),
            ],
            true,
        );
        let text = String::from_utf8(bytes).expect("utf8 head");
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Type: text/event-stream; charset=utf-8\r\n"));
        assert_eq!(text.matches("Transfer-Encoding:").count(), 1);
        assert!(text.contains("Transfer-Encoding: chunked\r\n"));
        assert!(!text.contains("Content-Length"), "stream head must not announce a length: {text:?}");
        assert_eq!(text.matches("Connection:").count(), 1);
        assert!(text.contains("Connection: keep-alive\r\n\r\n"));
        assert!(text.ends_with("\r\n\r\n"), "head ends on the blank line: {text:?}");

        // Explicit reason phrase and close semantics still work.
        let closing = String::from_utf8(serialize_stream_head(204, Some("Bye"), &[], false)).unwrap();
        assert!(closing.starts_with("HTTP/1.1 204 Bye\r\n"));
        assert!(closing.contains("Connection: close\r\n\r\n"));
    }

    #[test]
    fn chunk_frames_are_hex_prefixed_then_terminated_by_zero_chunk() {
        assert_eq!(chunk_frame(b"hello"), b"5\r\nhello\r\n".to_vec());
        assert_eq!(chunk_frame(&[0u8; 256]), format!("100\r\n{}\r\n", "\0".repeat(256)).into_bytes());
        // Multi-chunk stream plus terminator parses back exactly like the request-side decoder.
        let wire = [
            chunk_frame(b"event: t\n"),
            chunk_frame(b"data: 1\n\n"),
            LAST_CHUNK_FRAME.to_vec(),
        ]
        .concat();
        let mut p = RequestParser::new();
        p.push(
            format!("POST /sink HTTP/1.1\r\nHost: cz0\r\nTransfer-Encoding: chunked\r\n\r\n").as_bytes(),
        );
        p.push(&wire);
        let req = loop {
            match p.step() {
                Ok(ParseStep::Request(r)) => break r,
                Ok(other) => panic!("unexpected {other:?}"),
                Err(e) => panic!("roundtrip failed: {e}"),
            }
        };
        assert_eq!(req.body, b"event: t\ndata: 1\n\n".to_vec());
        assert_eq!(LAST_CHUNK_FRAME, b"0\r\n\r\n");
    }

    #[test]
    fn gradual_byte_feeding_still_parses_everything() {
        // Simulate a slow client trickling one byte at a time over a keep-alive connection.
        let wire = concat!(
            "GET /a HTTP/1.1\r\nHost: cz0\r\nContent-Length: 3\r\n\r\nxyz",
            "POST /b HTTP/1.1\r\nHost: cz0\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            "2\r\nhe\r\n3\r\nllo\r\n0\r\n\r\n"
        );
        let mut p = RequestParser::new();
        let mut got: Vec<ParsedRequest> = Vec::new();
        for b in wire.as_bytes() {
            p.push(&[*b]);
            loop {
                match p.step() {
                    Ok(ParseStep::NeedMore) => break,
                    Ok(ParseStep::InterimContinue) => {}
                    Ok(ParseStep::Request(r)) => got.push(r),
                    Err(e) => panic!("trickle parse failed: {e}"),
                }
            }
        }
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].body, b"xyz".to_vec());
        assert_eq!(got[1].body, b"hello".to_vec());
        assert!(!got[1].keep_alive);
    }

}
