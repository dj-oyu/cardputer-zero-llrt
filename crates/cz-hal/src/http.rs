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

/// One parsed request head (request line + header fields, names lowercased) held in **one owned
/// buffer**: `raw` carries the normalized head bytes (method upper-cased, header field names
/// lower-cased in place, obs-fold continuations folded into the preceding value), and every field
/// is addressed through byte offset *spans* into that buffer instead of 20–40 independent heap
/// `String` allocations per request. All accessors borrow `&str` slices — zero allocations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestHead {
    /// Owned normalized head bytes (always valid UTF-8 by construction).
    raw: Box<[u8]>,
    /// `(start, end)` byte span of the uppercase method token inside [`Self::raw`].
    method_span: (usize, usize),
    /// `(start, end)` byte span of the raw request target (incl. query string) inside `raw`.
    target_span: (usize, usize),
    /// HTTP minor version digit (`0` for `"1.0"`, `1` for `"1.1"`, ...); the parser only accepts
    /// exactly one ASCII digit.
    version_minor: u8,
    /// Header fields in wire order as `(name_span, value_span)` pairs into `raw`; names lowercase.
    header_spans: Vec<((usize, usize), (usize, usize))>,
}

/// Static `"1.x"` version strings indexed by the parsed minor digit (borrowed, never rebuilt).
pub const HTTP_VERSIONS: [&str; 10] = [
    "1.0", "1.1", "1.2", "1.3", "1.4", "1.5", "1.6", "1.7", "1.8", "1.9",
];

impl RequestHead {
    /// Borrow a `&str` out of a span. Spans are built exclusively from valid UTF-8 slices, so the
    /// checked conversion borrows (never allocates); the defensive clamps make hostile offsets
    /// yield `""` instead of panicking.
    #[inline]
    fn span_str(&self, span: (usize, usize)) -> &str {
        let start = span.0.min(self.raw.len());
        let end = span.1.min(self.raw.len()).max(start);
        std::str::from_utf8(&self.raw[start..end]).unwrap_or("")
    }

    /// Uppercase method token borrowed from `raw`, e.g. `"GET"`.
    pub fn method(&self) -> &str {
        self.span_str(self.method_span)
    }

    /// Raw request target including any query string, borrowed from `raw`.
    pub fn target(&self) -> &str {
        self.span_str(self.target_span)
    }

    /// HTTP version digits without the `HTTP/` prefix as a static string, e.g. `"1.1"`.
    pub fn version(&self) -> &str {
        HTTP_VERSIONS
            .get(self.version_minor as usize)
            .copied()
            .unwrap_or("1.1")
    }

    /// Iterate `(name, value)` pairs in wire order, borrowing directly from `raw` (zero allocs).
    pub fn headers_iter(&self) -> impl Iterator<Item = (&str, &str)> + '_ {
        self.header_spans
            .iter()
            .map(|(name, value)| (self.span_str(*name), self.span_str(*value)))
    }

    /// Number of header fields parsed off the wire (duplicate names counted separately).
    pub fn header_count(&self) -> usize {
        self.header_spans.len()
    }

    /// Case-insensitive iterator over all values for `name` in wire order (zero allocations).
    pub fn header_values<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.headers_iter()
            .filter(move |(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v)
    }

    /// Case-insensitive lookup; duplicate fields are joined with `", "` like Node.js.
    pub fn header(&self, name: &str) -> Option<Cow<'_, str>> {
        // Inline chain (not via `header_values`) so the yielded values borrow ONLY from `self`,
        // never from the lookup-name lifetime.
        let mut vals = self
            .headers_iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v);
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
        self.version_minor == 0
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

    /// Declared `Content-Length` when present and numeric (legacy non-failing lookup).
    pub fn content_length(&self) -> Option<u64> {
        self.checked_content_length().ok().flatten()
    }

    /// Strict RFC 9112 `Content-Length` parsing: rejects duplicate, non-digit, or overflowing
    /// `Content-Length` headers as `400 Bad Request` so they never fall back to `Length(0)`.
    pub fn checked_content_length(&self) -> Result<Option<u64>, HttpError> {
        let mut vals = self.header_values("content-length");
        let Some(first) = vals.next() else {
            return Ok(None);
        };
        if vals.next().is_some() {
            return Err(HttpError::BadRequest("duplicate content-length header".into()));
        }
        let trimmed = first.trim();
        if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
            return Err(HttpError::BadRequest(format!(
                "invalid content-length {first:?}"
            )));
        }
        trimmed
            .parse::<u64>()
            .map(Some)
            .map_err(|_| HttpError::BadRequest(format!("invalid content-length {first:?}")))
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
///
/// Phase 10D: the input lives in a **flat linear `Vec<u8>` plus a `read_pos` cursor** — parsed
/// bytes are skipped in O(1) instead of drained, and the unread tail is memmoved ONLY when the
/// buffer empties or an append would exceed capacity. This removes every
/// `VecDeque::make_contiguous()` wrap-around memmove from the hot path.
pub struct RequestParser {
    /// Flat linear receive buffer (bytes before `read_pos` are already parsed away).
    buf: Vec<u8>,
    /// Cursor of the first not-yet-parsed byte inside `buf`.
    read_pos: usize,
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
            buf: Vec::new(),
            read_pos: 0,
            state: State::Head,
            parsed_head: None,
            body: Vec::new(),
            body_read: 0,
            continue_sent: false,
            sticky: None,
        }
    }

    /// Append raw bytes received from the stream. Compaction of the unread tail happens here and
    /// ONLY under the two conditions of the linear-cursor rule (buffer fully consumed, or the
    /// append cannot fit without moving bytes).
    pub fn push(&mut self, data: &[u8]) {
        self.compact_for(data.len());
        self.buf.extend_from_slice(data);
    }

    /// Buffered (not yet consumed) byte count.
    pub fn buffered(&self) -> usize {
        self.buf.len() - self.read_pos
    }

    /// Bytes of request body decoded so far on the current request.
    pub fn body_progress(&self) -> u64 {
        self.body_read
    }

    #[inline]
    fn readable(&self) -> &[u8] {
        &self.buf[self.read_pos..]
    }

    /// Skip `n` parsed bytes; when everything buffered has been consumed the cursor and buffer
    /// reset together (free, no memmove).
    fn consume(&mut self, n: usize) {
        self.read_pos += n;
        if self.read_pos == self.buf.len() {
            self.buf.clear();
            self.read_pos = 0;
        }
    }

    /// Linear-cursor compaction: move the unread tail to offset 0 with `copy_within` ONLY when
    /// `read_pos > 0` and either `read_pos == buf.len()` or an append of `incoming` bytes would
    /// exceed capacity. Mid-stream pushes leave bytes untouched in place.
    fn compact_for(&mut self, incoming: usize) {
        if self.read_pos == 0 {
            return;
        }
        if self.read_pos == self.buf.len() {
            self.buf.clear();
            self.read_pos = 0;
        } else if self.buf.len() - self.read_pos + incoming > self.buf.capacity() {
            self.buf.copy_within(self.read_pos.., 0);
            let kept = self.buf.len() - self.read_pos;
            self.buf.truncate(kept);
            self.read_pos = 0;
        }
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
                    let take = std::cmp::min(need as usize, self.buffered());
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
            let r = self.readable();
            let mut n = 0usize;
            while n < r.len() {
                if r[n] == b'\n' {
                    n += 1;
                } else if r[n] == b'\r' && n + 1 < r.len() && r[n + 1] == b'\n' {
                    n += 2;
                } else {
                    break;
                }
            }
            n
        };
        if skip > 0 {
            self.consume(skip);
        }

        let (end, term_len) = match find_head_terminator(self.readable()) {
            Some(v) => v,
            None => {
                if self.buffered() > MAX_HEAD_BYTES {
                    return Err(HttpError::BadRequest("header section too large".into()));
                }
                return Ok(None);
            }
        };

        if end > MAX_HEAD_BYTES {
            return Err(HttpError::BadRequest("header section too large".into()));
        }
        let head_res = parse_head_lines(&self.buf[self.read_pos..self.read_pos + end]);
        self.consume(end + term_len);
        let head = head_res?;

        // Chunked transfer-encoding wins over Content-Length per RFC 9112 / Node.js behavior.
        let mode = if head.chunked() {
            State::Chunked(ChunkPhase::SizeLine)
        } else if let Some(cl) = head.checked_content_length()? {
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
        let start = self.read_pos;
        // The linear buffer's readable slice is contiguous by construction — one copy, no ring.
        self.body.extend_from_slice(&self.buf[start..start + take]);
        self.consume(take);
        self.body_read = next;
        Ok(())
    }

    fn step_chunked(&mut self, phase: ChunkPhase) -> Result<ChunkMove, HttpError> {
        match phase {
            ChunkPhase::SizeLine => {
                let (line_len, drain_len) = match peek_line_len(self.readable()) {
                    Some(v) => v,
                    None => {
                        if self.buffered() > MAX_CHUNK_LINE_BYTES {
                            return Err(HttpError::BadRequest("chunk-size line too long".into()));
                        }
                        return Ok(ChunkMove::NeedMore(ChunkPhase::SizeLine));
                    }
                };
                let size_res = {
                    let line = &self.readable()[..line_len];
                    let token = String::from_utf8_lossy(line);
                    let hex = token.split(';').next().unwrap_or("").trim();
                    u64::from_str_radix(hex, 16)
                        .map_err(|_| HttpError::BadRequest(format!("invalid chunk size {hex:?}")))
                };
                self.consume(drain_len);
                let size = size_res?;
                if size > MAX_BODY_BYTES as u64
                    || self
                        .body_read
                        .checked_add(size)
                        .is_none_or(|sum| sum > MAX_BODY_BYTES as u64)
                {
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
                let take = std::cmp::min(size, self.buffered() as u64) as usize;
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
            ChunkPhase::DataEol => match peek_line_len(self.readable()) {
                Some((line_len, drain_len)) => {
                    // Phase 15B: the chunk-data CRLF MUST terminate an EMPTY line. Trailing
                    // garbage on that line is a framing violation; reject BEFORE consuming.
                    if line_len != 0 {
                        return Err(HttpError::BadRequest(
                            "chunk framing error: non-empty chunk-data terminator line".into(),
                        ));
                    }
                    self.consume(drain_len);
                    Ok(ChunkMove::Advanced(ChunkPhase::SizeLine))
                }
                None => {
                    if self.buffered() > MAX_CHUNK_LINE_BYTES {
                        return Err(HttpError::BadRequest("chunk framing error: missing CRLF".into()));
                    }
                    Ok(ChunkMove::NeedMore(ChunkPhase::DataEol))
                }
            },
            ChunkPhase::Trailers => match peek_line_len(self.readable()) {
                Some((line_len, drain_len)) => {
                    self.consume(drain_len);
                    if line_len == 0 {
                        Ok(ChunkMove::Done)
                    } else {
                        Ok(ChunkMove::Advanced(ChunkPhase::Trailers))
                    }
                }
                None => {
                    if self.buffered() > MAX_HEAD_BYTES {
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
/// Returns `(header_block_end_index, terminator_length)` relative to the given slice.
///
/// Round 3 (Stall #1 kill): instead of two byte-at-a-time `find_subslice` walks over the whole
/// buffer, one vectorized newline-candidate scan (`scan_newline`) drives exact-local checks at
/// each `\n`. Matches are discovered in ascending START order (`\n\n` starts at the candidate,
/// `\r\n\r\n` starts one byte earlier), so the first event reported equals the legacy
/// `min(crlf_first, lf_first)` with crlf winning a tie. Semantics identical, including the
/// "pattern must be fully present" rule. Phase 10D: operates directly on the parser's linear
/// readable slice — no `make_contiguous()` anywhere.
fn find_head_terminator(contig: &[u8]) -> Option<(usize, usize)> {
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

/// Inspect the next `\r\n` or `\n` terminated line at the front of the linear readable slice
/// without allocating. Returns `(content_len_without_cr, advance_len_including_lf)` when a full
/// line has arrived.
fn peek_line_len(contig: &[u8]) -> Option<(usize, usize)> {
    let nl = scan_newline(contig, 0)?;
    let content_len = if nl > 0 && contig[nl - 1] == b'\r' { nl - 1 } else { nl };
    Some((content_len, nl + 1))
}

/// Consume one `\r\n` / `\n` terminated line from a LINEAR cursor (copies it out and advances
/// `read_pos`; resets when the buffer empties). Mirrors the old VecDeque `take_line` semantics for
/// the parity fuzz test without memmoving the tail.
#[cfg(test)]
fn take_line(buf: &mut Vec<u8>, read_pos: &mut usize) -> Option<Vec<u8>> {
    let nl = scan_newline(&buf[*read_pos..], 0)?;
    let mut line: Vec<u8> = buf[*read_pos..*read_pos + nl].to_vec();
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    *read_pos += nl + 1;
    if *read_pos == buf.len() {
        buf.clear();
        *read_pos = 0;
    }
    Some(line)
}

/// Parse the request line plus header fields out of raw head bytes into a **span-backed**
/// [`RequestHead`]: exactly ONE owned normalized buffer is built (method upper-cased, field names
/// lower-cased in place, obs-fold continuations appended inside the preceding value span) and all
/// fields are recorded as `(start, end)` spans — no per-field `String` allocations.
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
    // Exactly one ASCII digit at this point (`version_token.len() >= 8` rules out an empty minor).
    let version_minor = minor.as_bytes()[0] - b'0';
    let method_upper_ok =
        !raw_method.is_empty() && raw_method.bytes().all(|b| b.to_ascii_uppercase().is_ascii_graphic());
    if !method_upper_ok {
        return Err(HttpError::BadRequest("invalid method token".into()));
    }
    if raw_target.is_empty() {
        return Err(HttpError::BadRequest("empty request target".into()));
    }

    // One owned buffer for the whole normalized head; spans address each field inside it.
    let mut raw: Vec<u8> = Vec::with_capacity(head_bytes.len() + 16);
    let m0 = raw.len();
    for b in raw_method.bytes() {
        raw.push(b.to_ascii_uppercase());
    }
    let method_span = (m0, raw.len());
    raw.push(b' ');
    let t0 = raw.len();
    raw.extend_from_slice(raw_target.as_bytes());
    let target_span = (t0, raw.len());
    raw.push(b' ');
    raw.extend_from_slice(b"HTTP/1.");
    raw.push(minor.as_bytes()[0]);
    raw.extend_from_slice(b"\r\n");

    let mut header_spans: Vec<((usize, usize), (usize, usize))> = Vec::with_capacity(16);
    for line in lines {
        if line.is_empty() {
            break;
        }
        // Obs-fold continuation lines (starting with space/tab) are appended to the last value —
        // INSIDE `raw`, extending that span so lookups still borrow a single contiguous slice.
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(last) = header_spans.last_mut() {
                // Fold the continuation INTO the previous value span: retract the field's line
                // separator, then append " " + trimmed continuation so lookups still borrow one
                // contiguous slice (byte-for-byte the legacy `" " + trim` join semantics).
                if raw.ends_with(b"\r\n") {
                    raw.truncate(raw.len() - 2);
                }
                raw.push(b' ');
                raw.extend_from_slice(line.trim().as_bytes());
                last.1.1 = raw.len();
                continue;
            }
            return Err(HttpError::BadRequest("header continuation without field".into()));
        }
        let idx = line
            .find(':')
            .ok_or_else(|| HttpError::BadRequest(format!("invalid header field {line:?}")))?;
        let name = line[..idx].trim();
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_graphic() && b != b':') {
            return Err(HttpError::BadRequest(format!("invalid header name {line:?}")));
        }
        let value = line[idx + 1..].trim();
        // Restore a missing line separator after a preceding obs-fold so `raw` stays a cleanly
        // normalized head; normal fields already end on "\r\n".
        if !raw.is_empty() && !raw.ends_with(b"\r\n") {
            raw.extend_from_slice(b"\r\n");
        }
        let n0 = raw.len();
        for b in name.bytes() {
            raw.push(b.to_ascii_lowercase());
        }
        let name_span = (n0, raw.len());
        raw.extend_from_slice(b": ");
        let v0 = raw.len();
        raw.extend_from_slice(value.as_bytes());
        let value_span = (v0, raw.len());
        raw.extend_from_slice(b"\r\n");
        header_spans.push((name_span, value_span));
    }

    Ok(RequestHead {
        raw: raw.into_boxed_slice(),
        method_span,
        target_span,
        version_minor,
        header_spans,
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

/// Duplicate-header bookkeeping for [`write_filtered_headers`]: up to `INLINE_SEEN` names live in
/// a stack buffer with ZERO heap allocations (Keep-Alive TX reuses one scratch head per response);
/// only anomalously long header lists spill onto the heap.
struct SeenNames<'a> {
    inline: [&'a str; 32],
    n: usize,
    spill: Vec<&'a str>,
}

impl<'a> SeenNames<'a> {
    #[inline]
    fn new() -> Self {
        Self { inline: [""; 32], n: 0, spill: Vec::new() }
    }

    #[inline]
    fn any_icase(&self, name: &str) -> bool {
        self.inline[..self.n].iter().any(|s| s.eq_ignore_ascii_case(name))
            || self.spill.iter().any(|s| s.eq_ignore_ascii_case(name))
    }

    #[inline]
    fn push(&mut self, name: &'a str) {
        if self.n < self.inline.len() {
            self.inline[self.n] = name;
            self.n += 1;
        } else {
            self.spill.push(name);
        }
    }
}

fn write_filtered_headers(
    out: &mut Vec<u8>,
    headers: &[(String, String)],
    skip_content_length: bool,
) -> bool {
    let mut has_len = false;
    let mut seen: SeenNames<'_> = SeenNames::new();
    for (name, value) in headers {
        // Phase 15B (RFC 6265): Set-Cookie must NEVER be collapsed into one comma-joined
        // field, so duplicate Set-Cookie headers are exempt from the dedup bookkeeping.
        let is_set_cookie = name.eq_ignore_ascii_case("set-cookie");
        if name.eq_ignore_ascii_case("connection")
            || name.eq_ignore_ascii_case("transfer-encoding")
            || (skip_content_length && name.eq_ignore_ascii_case("content-length"))
            || (!is_set_cookie && seen.any_icase(name))
        {
            continue;
        }
        // Header names must be non-empty and field-content safe; skip anything hostile before
        // updating dedup/length bookkeeping so dropped headers never disable auto Content-Length.
        if !name.is_empty()
            && name.chars().all(|c| c.is_ascii_graphic() && c != ':')
            && !value.contains('\n')
            && !value.contains('\r')
        {
            if !is_set_cookie {
                seen.push(name.as_str());
            }
            out.reserve(name.len() + value.len() + 4);
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(b": ");
            out.extend_from_slice(value.as_bytes());
            out.extend_from_slice(b"\r\n");
            if name.eq_ignore_ascii_case("content-length") {
                has_len = true;
            }
        }
    }
    has_len
}

fn safe_reason_phrase(status: u16, reason: Option<&str>) -> &str {
    match reason {
        Some(r) if !r.contains(['\r', '\n']) => r,
        _ => reason_phrase(status),
    }
}

/// Serialize **only** the status line + headers into a caller-owned reusable `out` buffer (the
/// Phase 10D zero-body-copy TX path): the response body is never copied here — `body_len` only
/// drives the auto `Content-Length`. `out` is cleared first, so a long-lived connection can reuse
/// one scratch allocation across Keep-Alive responses. Reserved/duplicate header skip rules are
/// identical to [`serialize_response`].
pub fn serialize_response_head_into(
    out: &mut Vec<u8>,
    status: u16,
    reason: Option<&str>,
    headers: &[(String, String)],
    body_len: usize,
    keep_alive: bool,
) {
    let phrase = safe_reason_phrase(status, reason);
    out.clear();
    out.reserve(64 + headers.len() * 40);
    let _ = write!(out, "HTTP/1.1 {status} {phrase}\r\n");
    let has_len = write_filtered_headers(out, headers, false);
    if !has_len {
        let _ = write!(out, "Content-Length: {}\r\n", body_len);
    }
    out.extend_from_slice(if keep_alive {
        b"Connection: keep-alive\r\n\r\n"
    } else {
        b"Connection: close\r\n\r\n"
    });
}

/// Serialize a complete HTTP/1.1 response. `Content-Length` is added when absent and any
/// caller-supplied `Connection` / duplicated reserved headers are dropped in favor of the engine
/// decision (`Connection: keep-alive` or `Connection: close`). Delegates the head to
/// [`serialize_response_head_into`] then appends the body once.
pub fn serialize_response(
    status: u16,
    reason: Option<&str>,
    headers: &[(String, String)],
    body: &[u8],
    keep_alive: bool,
) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(64 + headers.len() * 40 + body.len());
    serialize_response_head_into(&mut out, status, reason, headers, body.len(), keep_alive);
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
    let phrase = safe_reason_phrase(status, reason);
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
    fn head_terminator_and_take_line_match_legacy_on_linear_buffers() {
        // Deterministic fuzz for the Phase 10D LINEAR cursor: every pattern class compared
        // against the legacy two-subsearch algorithm — both on full buffers and on partially
        // consumed cursors whose unread tail was compacted with `copy_within` (the VecDeque ring
        // layout is gone; readable slices are contiguous by construction).
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
            if trial % 2 == 1 && data.len() > 10 {
                // Simulate a consumed prefix, then compact the unread tail to offset 0.
                let mut buf: Vec<u8> = Vec::with_capacity(16);
                buf.extend_from_slice(&data);
                buf.copy_within(4.., 0);
                buf.truncate(buf.len() - 4);
                assert_eq!(find_head_terminator(&buf), legacy_head_terminator(&data[4..]),
                    "compacted-tail terminator mismatch trial={trial}");
                continue;
            }
            assert_eq!(find_head_terminator(&data), want, "terminator mismatch trial={trial}");
            // take_line parity over the first line at several cursor offsets (no memmove).
            for shift in [0usize, 1, 3] {
                if shift > data.len() {
                    continue;
                }
                let mut b2 = data.clone();
                let mut p2 = shift;
                if p2 == b2.len() {
                    b2.clear();
                    p2 = 0;
                }
                let nl_ref = data[shift..].iter().position(|b| *b == b'\n');
                let got = take_line(&mut b2, &mut p2);
                match nl_ref {
                    Some(nl) => {
                        let line = &data[shift..shift + nl];
                        let expect = if line.last() == Some(&b'\r') { &line[..line.len() - 1] } else { line };
                        assert_eq!(got.as_deref(), Some(expect), "take_line mismatch trial={trial} shift={shift}");
                        if shift + nl + 1 == data.len() && !data.is_empty() {
                            assert_eq!((b2.len(), p2), (0, 0), "full line consumed must reset");
                        } else {
                            assert_eq!(p2, shift + nl + 1, "cursor advance mismatch trial={trial} shift={shift}");
                        }
                    }
                    None => assert_eq!(got, None, "take_line must be None trial={trial} shift={shift}"),
                }
            }
        }
    }

    #[test]
    fn request_line_headers_and_lowercase_names() {
        let r = one_request(
            "GET /api/status?v=1&src=tailscale HTTP/1.1\r\nHost: cz0\r\nX-Test-Client: phone\r\nUser-Agent: curl/8\r\n\r\n",
        )
        .expect("parse");
        assert_eq!(r.head.method(), "GET");
        assert_eq!(r.head.target(), "/api/status?v=1&src=tailscale");
        assert_eq!(r.head.version(), "1.1");
        assert_eq!(r.body, Vec::<u8>::new());
        assert_eq!(r.head.header("x-test-client").as_deref(), Some("phone"));
        assert_eq!(r.head.header("X-TEST-CLIENT").as_deref(), Some("phone"));
        assert!(r.keep_alive, "HTTP/1.1 defaults to keep-alive");
        // Header order preserved and names lowercased on the span-backed struct.
        assert_eq!(r.head.header_count(), 3);
        assert_eq!(r.head.headers_iter().next().unwrap().0, "host");
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
        assert_eq!(first.head.method(), "POST");
        assert_eq!(first.body, b"hello".to_vec());
        assert!(first.keep_alive);
        // Leftover bytes remain buffered: a second sequential request parses (keep-alive).
        let second = match p.step() {
            Ok(ParseStep::Request(r)) => r,
            other => panic!("expected pipelined request, got {other:?}"),
        };
        assert_eq!(second.head.target(), "/next");
        assert!(!second.keep_alive, "Connection: close honored");
        assert_eq!(p.buffered(), 0);
    }

    #[test]
    fn http10_defaults_to_close_and_keepalive_header_overrides() {
        let a = one_request("GET / HTTP/1.0\r\nHost: cz0\r\n\r\n").expect("1.0 parse");
        assert!(!a.keep_alive);
        let b = one_request("GET / HTTP/1.0\r\nHost: cz0\r\nConnection: keep-alive\r\n\r\n").expect("parse");
        assert!(b.keep_alive);
        assert_eq!(b.head.version(), "1.0");
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
            Ok(ParseStep::Request(next)) => assert_eq!(next.head.target(), "/after"),
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
        assert_eq!(r.head.target(), "/lf");
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
                assert_eq!(r.head.method(), "PUT");
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

    #[test]
    fn span_backed_head_accessors_borrow_from_one_owned_buffer() {
        let r = one_request(
            "post /spans?q=1 HTTP/1.1\r\nMixed-Case: Alpha\r\nhost: cz0\r\nEmpty:\r\n\r\n",
        )
        .expect("span head parse");
        assert_eq!(r.head.method(), "POST");
        assert_eq!(r.head.target(), "/spans?q=1");
        assert_eq!(r.head.version(), "1.1");
        assert_eq!(r.head.header_count(), 3);
        let pairs: Vec<(&str, &str)> = r.head.headers_iter().collect();
        assert_eq!(pairs, vec![("mixed-case", "Alpha"), ("host", "cz0"), ("empty", "")]);
        // Lookups borrow straight out of the single owned buffer (zero allocation).
        assert!(matches!(r.head.header("MiXeD-Case"), Some(Cow::Borrowed("Alpha"))));
        assert!(matches!(r.head.header("EMPTY"), Some(Cow::Borrowed(""))));
        assert!(r.head.header("absent").is_none());
        // `raw` holds the whole normalized head: upper-cased method + lowercased field names.
        let raw_text = std::str::from_utf8(&r.head.raw).expect("normalized head is valid utf8");
        assert!(raw_text.starts_with("POST /spans?q=1 HTTP/1.1\r\n"), "raw={raw_text:?}");
        assert!(raw_text.contains("mixed-case: Alpha\r\n"), "raw={raw_text:?}");
    }

    #[test]
    fn obs_fold_continuations_normalize_into_a_single_value_span() {
        let r = one_request(
            "GET /fold HTTP/1.1\r\nX-Long: one\r\n\tfolded\r\n  deeper\r\nHost: cz0\r\n\r\n",
        )
        .expect("multi fold parse");
        // Both continuations live INSIDE the previous value span -> one borrowed &str, no join.
        assert_eq!(r.head.header_count(), 2);
        assert!(matches!(r.head.header("x-long"), Some(Cow::Borrowed("one folded deeper"))));
        let vals: Vec<&str> = r.head.header_values("X-LONG").collect();
        assert_eq!(vals, vec!["one folded deeper"]);
    }

    #[test]
    fn cursor_compaction_fires_only_on_full_drain_or_capacity_pressure() {
        // Linear-cursor rule (explicit 32-byte capacity so the outcomes are deterministic):
        // * mid-buffer append that fits  -> bytes stay exactly where they are;
        // * append that cannot fit       -> unread tail memmoves to offset 0;
        // * everything consumed          -> cursor + buffer reset with NO memmove.
        let mut p = RequestParser::new();
        p.buf = Vec::with_capacity(32);
        p.buf.extend_from_slice(b"0123456789");
        p.read_pos = 4;
        p.compact_for(1); // spare capacity available -> untouched
        assert_eq!(p.read_pos, 4);
        assert_eq!(p.readable(), b"456789");
        assert_eq!(&p.buf[..], b"0123456789", "no premature memmove");
        p.compact_for(64); // this append genuinely cannot fit without moving -> compact
        assert_eq!(p.read_pos, 0);
        assert_eq!(&p.buf[..], b"456789");
        // Full drain resets without any memmove at all.
        p.read_pos = p.buf.len();
        p.compact_for(1);
        assert_eq!((p.buf.len(), p.read_pos), (0, 0));
    }

    #[test]
    fn linear_cursor_serves_pipelined_keepalive_requests_across_split_pushes() {
        // Stray leading CRLFs, one request parsed completely while a pipelined follow-up is only
        // partially buffered, then that follow-up fed in small chunks: the cursor skips parsed
        // bytes in place (no drain memmove) and stays coherent across compactions.
        let mut p = RequestParser::new();
        p.push(b"\r\n\r\nGET /a HTTP/1.1\r\nHost: cz0\r\nContent-Length: 3\r\n\r\nabc\r\nPOST /b HTT");
        let first = match p.step() {
            Ok(ParseStep::Request(r)) => r,
            other => panic!("expected first request, got {other:?}"),
        };
        assert_eq!(first.head.method(), "GET");
        assert_eq!(first.body, b"abc".to_vec());
        assert!(first.keep_alive);
        // The pipelined remnant (13 bytes) stays parked AFTER the cursor:
        assert_eq!(p.buffered(), 13, "partial second request still unparsed");
        assert_eq!(p.read_pos, p.buf.len() - 13, "cursor skipped parsed bytes in place");
        assert!(p.read_pos > 0, "parsed prefix is skipped, not drained");

        let rest = "P/1.1\r\nHost: cz0\r\nContent-Length: 4\r\nConnection: close\r\n\r\nxyzw";
        let mut second = None;
        for chunk in rest.as_bytes().chunks(7) {
            p.push(chunk);
            loop {
                match p.step() {
                    Ok(ParseStep::NeedMore) => break,
                    Ok(ParseStep::InterimContinue) => {}
                    Ok(ParseStep::Request(r)) => second = Some(r),
                    Err(e) => panic!("pipelined linear-cursor parse failed: {e}"),
                }
            }
        }
        let r2 = second.expect("second keep-alive request parsed");
        assert_eq!(r2.head.target(), "/b");
        assert_eq!(r2.head.method(), "POST");
        assert_eq!(r2.body, b"xyzw".to_vec());
        assert!(!r2.keep_alive, "Connection: close honored");
        assert_eq!((p.buf.len(), p.read_pos), (0, 0), "full drain resets cursor + buffer");
    }

    #[test]
    fn serialize_response_head_into_is_body_free_reusable_and_serialize_response_delegates() {
        let headers = vec![("Content-Type".into(), "application/json".into())];
        let mut scratch: Vec<u8> = Vec::with_capacity(4096);
        scratch.extend_from_slice(b"STALE GARBAGE FROM A PREVIOUS RESPONSE");
        let cap_before = scratch.capacity();
        serialize_response_head_into(&mut scratch, 201, None, &headers, 11, true);
        let text = String::from_utf8(scratch.clone()).expect("head utf8");
        assert!(text.starts_with("HTTP/1.1 201 Created\r\n"), "{text:?}");
        assert!(text.contains("Content-Type: application/json\r\n"));
        assert!(text.contains("Content-Length: 11\r\n"), "length reflects body_len arg");
        assert!(text.ends_with("Connection: keep-alive\r\n\r\n"), "{text:?}");
        assert!(!text.contains("STALE"), "reused buffer must be cleared first: {text:?}");

        // Reuse across Keep-Alive responses keeps the allocation (no shrink).
        serialize_response_head_into(&mut scratch, 404, Some("Nope"), &[], 0, false);
        assert_eq!(scratch.capacity(), cap_before, "buffer reuse must not reallocate");
        let again = String::from_utf8(scratch.clone()).unwrap();
        assert!(again.starts_with("HTTP/1.1 404 Nope\r\n"));
        assert!(again.contains("Content-Length: 0\r\n"));
        assert!(again.ends_with("Connection: close\r\n\r\n"));

        // The body NEVER appears in the head serialization; serialize_response delegates: exact
        // head bytes + body appended byte-for-byte.
        let full = serialize_response(201, None, &headers, b"{\"ok\":true}", true);
        serialize_response_head_into(&mut scratch, 201, None, &headers, 11, true);
        let mut want = scratch.clone();
        want.extend_from_slice(b"{\"ok\":true}");
        assert_eq!(full, want, "serialize_response must equal head_into + body");
    }

    #[test]
    fn chunked_u64_overflow_is_rejected_without_panic_or_wrap() {
        let err = one_request(
            "POST /x HTTP/1.1\r\nHost: cz0\r\nTransfer-Encoding: chunked\r\n\r\n1\r\na\r\nffffffffffffffff\r\n",
        )
        .expect_err("u64::MAX chunk after body must fail 413");
        assert_eq!(err.status(), 413);
        assert!(matches!(err, HttpError::PayloadTooLarge(_)));
    }

    #[test]
    fn duplicate_or_invalid_content_length_is_bad_request() {
        for bad in [
            "POST /x HTTP/1.1\r\nHost: cz0\r\nContent-Length: 5\r\nContent-Length: 10\r\n\r\nhello",
            "POST /x HTTP/1.1\r\nHost: cz0\r\nContent-Length: 5\r\nContent-Length: 5\r\n\r\nhello",
            "POST /x HTTP/1.1\r\nHost: cz0\r\nContent-Length: abc\r\n\r\nGET /smuggled HTTP/1.1\r\n\r\n",
            "POST /x HTTP/1.1\r\nHost: cz0\r\nContent-Length: +5\r\n\r\nhello",
            "POST /x HTTP/1.1\r\nHost: cz0\r\nContent-Length: -1\r\n\r\n",
        ] {
            let err = one_request(bad).expect_err("ambiguous/invalid CL must return 400");
            assert_eq!(err.status(), 400, "fixture {bad:?} -> {err}");
            assert!(matches!(err, HttpError::BadRequest(_)));
        }
    }

    #[test]
    fn response_head_sanitizes_crlf_reason_and_rejected_content_length() {
        let resp = serialize_response(
            200,
            Some("OK\r\nX-Injected: evil"),
            &[
                ("Content-Length".into(), "5\r\nX-Evil: 1".into()),
                ("".into(), "empty-name".into()),
            ],
            b"abc",
            true,
        );
        let text = String::from_utf8(resp).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "sanitized status line: {text:?}");
        assert!(!text.contains("X-Injected"), "CRLFs in reason must not inject headers");
        assert!(text.contains("Content-Length: 3\r\n"), "auto CL preserved when hostile CL skipped");
        assert!(!text.contains("\r\n: empty-name"), "empty header name skipped");
    }

    #[test]
    fn chunk_data_terminator_line_with_trailing_garbage_is_bad_request() {
        // Phase 15B: after every chunk payload the terminating line MUST be empty. Garbage
        // riding on the SAME line as the chunk data (before its CRLF) used to be swallowed
        // silently; it is now a strict 400 from the DataEol phase itself.
        let err = one_request(
            "POST /x HTTP/1.1\r\nHost: cz0\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nhe junk\r\n0\r\n\r\n",
        )
        .expect_err("non-empty chunk-data terminator must be rejected");
        assert_eq!(err.status(), 400);
        assert!(matches!(err, HttpError::BadRequest(_)));
        assert!(format!("{err}").contains("terminator"), "msg={err}");

        // The garbage terminator arriving split across pushes fails just the same.
        let mut p = RequestParser::new();
        p.push(b"PUT /y HTTP/1.1\r\nHost: cz0\r\nTransfer-Encoding: chunked\r\n\r\n1\r\naX");
        assert_eq!(p.step().unwrap(), ParseStep::NeedMore, "awaiting the terminator newline");
        p.push(b"\r\n");
        let err = p.step().expect_err("split garbage terminator must fail too");
        assert_eq!(err.status(), 400);
        assert!(format!("{err}").contains("terminator"), "msg={err}");

        // Stray text AFTER a proper empty terminator still dies as an invalid size line.
        let err2 = one_request(
            "POST /z HTTP/1.1\r\nHost: cz0\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nhe\r\njunk\r\n0\r\n\r\n",
        )
        .expect_err("junk size line must be rejected");
        assert_eq!(err2.status(), 400);

        // Clean framing (CRLF and bare-LF terminators) still parses exactly as before.
        let ok = one_request(
            "POST /ok HTTP/1.1\r\nHost: cz0\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nhi\r\n0\r\n\r\n",
        )
        .expect("valid chunk framing must stay valid");
        assert_eq!(ok.body, b"hi".to_vec());
        let lf = one_request(
            "POST /lf HTTP/1.1\nHost: cz0\nTransfer-Encoding: chunked\n\n1\na\n0\n\n",
        )
        .expect("bare-LF terminator stays valid");
        assert_eq!(lf.body, b"a".to_vec());
    }

    #[test]
    fn multiple_set_cookie_headers_survive_response_serialization() {
        // Phase 15B (RFC 6265): duplicate Set-Cookie headers must NEVER be collapsed or dropped,
        // while every other duplicated name keeps the historical first-wins dedup rule.
        let headers = vec![
            ("Set-Cookie".into(), "a=1; Path=/".into()),
            ("set-cookie".into(), "b=2; HttpOnly".into()),
            ("X-Dup".into(), "first".into()),
            ("X-Dup".into(), "second-dropped".into()),
            ("Content-Type".into(), "text/plain".into()),
        ];
        let text = String::from_utf8(serialize_response(200, None, &headers, b"ok", true)).unwrap();
        // Header names keep the caller's casing, so count case-insensitively.
        assert_eq!(text.to_lowercase().matches("set-cookie:").count(), 2, "{text:?}");
        assert!(text.contains("Set-Cookie: a=1; Path=/\r\n"), "{text:?}");
        assert!(text.contains("set-cookie: b=2; HttpOnly\r\n"), "{text:?}");
        assert_eq!(text.matches("X-Dup:").count(), 1, "{text:?}");
        assert!(!text.contains("second-dropped"), "{text:?}");

        // The zero-body-copy Keep-Alive head path applies the identical rules.
        let mut scratch: Vec<u8> = Vec::new();
        serialize_response_head_into(&mut scratch, 200, None, &headers, 2, true);
        let head = String::from_utf8(scratch).unwrap();
        assert_eq!(head.to_lowercase().matches("set-cookie:").count(), 2, "{head:?}");

        // Hostile CRLF inside a cookie value is still skipped outright (no injection).
        let hostile = vec![("Set-Cookie".into(), "a=1\r\nX-Evil: 1".into())];
        let htext = String::from_utf8(serialize_response(200, None, &hostile, b"", true)).unwrap();
        assert!(!htext.contains("X-Evil"), "CRLF cookie injection must never reach the wire: {htext:?}");
    }

}
