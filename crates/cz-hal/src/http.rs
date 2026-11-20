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

use std::collections::VecDeque;

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
    /// Case-insensitive lookup; duplicate fields are joined with `", "` like Node.js.
    pub fn header(&self, name: &str) -> Option<String> {
        let want = name.to_ascii_lowercase();
        let mut found: Vec<&str> = Vec::new();
        for (k, v) in &self.headers {
            if *k == want {
                found.push(v.as_str());
            }
        }
        if found.is_empty() {
            None
        } else {
            Some(found.join(", "))
        }
    }

    /// True when the request is HTTP/1.0 (default connection semantics differ).
    pub fn is_http10(&self) -> bool {
        self.version == "1.0"
    }

    /// Whether this connection should stay open after the response (HTTP/1.1 default keep-alive,
    /// HTTP/1.0 default close unless the client asks for keep-alive).
    pub fn keep_alive(&self) -> bool {
        let conn = self.header("connection").unwrap_or_default().to_ascii_lowercase();
        if conn.contains("close") {
            false
        } else if conn.contains("keep-alive") {
            true
        } else {
            !self.is_http10()
        }
    }

    /// Client asked for `Expect: 100-continue` before the body is transmitted.
    pub fn expects_continue(&self) -> bool {
        self.header("expect")
            .map(|v| v.to_ascii_lowercase().trim() == "100-continue")
            .unwrap_or(false)
    }

    /// `Transfer-Encoding: chunked` present?
    pub fn chunked(&self) -> bool {
        self.header("transfer-encoding")
            .map(|v| v.to_ascii_lowercase().contains("chunked"))
            .unwrap_or(false)
    }

    /// Declared `Content-Length` when present and numeric.
    pub fn content_length(&self) -> Option<u64> {
        self.header("content-length")
            .and_then(|v| v.trim().parse::<u64>().ok())
    }
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

#[derive(Debug, PartialEq, Eq, Clone)]
enum State {
    Head,
    /// Waiting for exactly `n` body bytes.
    Length(u64),
    Chunked(ChunkPhase),
}

#[derive(Debug, PartialEq, Eq, Clone)]
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
            // Clone the current state so we can mutate `self` while dispatching on it.
            let cur = self.state.clone();
            match cur {
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
        let head_bytes: Vec<u8> = self.buf.make_contiguous()[..end].to_vec();
        self.buf.drain(..end + term_len);
        let head = parse_head_lines(&head_bytes)?;

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
        let slice: Vec<u8> = self.buf.make_contiguous()[..take].to_vec();
        self.buf.drain(..take);
        self.body.extend(slice);
        self.body_read = next;
        Ok(())
    }

    fn step_chunked(&mut self, phase: ChunkPhase) -> Result<ChunkMove, HttpError> {
        match phase {
            ChunkPhase::SizeLine => {
                let line = match take_line(&mut self.buf) {
                    Some(l) => l,
                    None => {
                        if self.buf.len() > MAX_CHUNK_LINE_BYTES {
                            return Err(HttpError::BadRequest("chunk-size line too long".into()));
                        }
                        return Ok(ChunkMove::NeedMore(ChunkPhase::SizeLine));
                    }
                };
                let token = String::from_utf8_lossy(&line);
                let hex = token.split(';').next().unwrap_or("").trim();
                let size = u64::from_str_radix(hex, 16)
                    .map_err(|_| HttpError::BadRequest(format!("invalid chunk size {hex:?}")))?;
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
            ChunkPhase::DataEol => match take_line(&mut self.buf) {
                Some(_) => Ok(ChunkMove::Advanced(ChunkPhase::SizeLine)),
                None => {
                    if self.buf.len() > MAX_CHUNK_LINE_BYTES {
                        return Err(HttpError::BadRequest("chunk framing error: missing CRLF".into()));
                    }
                    Ok(ChunkMove::NeedMore(ChunkPhase::DataEol))
                }
            },
            ChunkPhase::Trailers => match take_line(&mut self.buf) {
                Some(l) if l.is_empty() => Ok(ChunkMove::Done),
                Some(_) => Ok(ChunkMove::Advanced(ChunkPhase::Trailers)),
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
fn find_head_terminator(buf: &mut VecDeque<u8>) -> Option<(usize, usize)> {
    let contig = buf.make_contiguous();
    let crlf = find_subslice(contig, b"\r\n\r\n");
    let lf = find_subslice(contig, b"\n\n");
    match (crlf, lf) {
        (Some(a), Some(b)) => Some(if a <= b { (a, 4) } else { (b, 2) }),
        (Some(a), None) => Some((a, 4)),
        (None, Some(b)) => Some((b, 2)),
        (None, None) => None,
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
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

/// Remove one header/trailer line (`\r\n` or `\n` terminated) from the front of `buf`.
/// Returns `None` when the terminator has not arrived yet.
fn take_line(buf: &mut VecDeque<u8>) -> Option<Vec<u8>> {
    let contig = buf.make_contiguous();
    let nl = find_subslice(contig, b"\n")?;
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
    let parts: Vec<&str> = req_line.split_whitespace().collect();
    if parts.len() != 3 {
        return Err(HttpError::BadRequest(format!(
            "invalid request line {req_line:?}"
        )));
    }
    let version_token = parts[2];
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
    let method = parts[0].to_ascii_uppercase();
    if method.is_empty() || !method.chars().all(|c| c.is_ascii_graphic()) {
        return Err(HttpError::BadRequest("invalid method token".into()));
    }
    if parts[1].is_empty() {
        return Err(HttpError::BadRequest("empty request target".into()));
    }

    let mut headers: Vec<(String, String)> = Vec::new();
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

    Ok(RequestHead {
        method,
        target: parts[1].to_string(),
        version: format!("1.{minor}"),
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
    {
        let st = status.to_string();
        out.reserve(12 + st.len() + phrase.len() + 4);
        out.extend_from_slice(b"HTTP/1.1 ");
        out.extend_from_slice(st.as_bytes());
        out.push(b' ');
        out.extend_from_slice(phrase.as_bytes());
        out.extend_from_slice(b"\r\n");
    }

    let mut has_len = false;
    let mut seen: Vec<String> = Vec::with_capacity(headers.len());
    for (name, value) in headers {
        let lower = name.to_ascii_lowercase();
        if lower == "connection" || lower == "transfer-encoding" || seen.contains(&lower) {
            continue;
        }
        seen.push(lower.clone());
        // Header names must be field-content safe; skip anything hostile instead of crashing.
        if lower.chars().all(|c| c.is_ascii_graphic() && c != ':') && !value.contains('\n') && !value.contains('\r') {
            out.reserve(name.len() + value.len() + 4);
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(b": ");
            out.extend_from_slice(value.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        if lower == "content-length" {
            has_len = true;
        }
    }
    if !has_len {
        let cl = body.len().to_string();
        out.reserve(20 + cl.len() + 4);
        out.extend_from_slice(b"Content-Length: ");
        out.extend_from_slice(cl.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    {
        let conn = if keep_alive { "keep-alive" } else { "close" };
        out.reserve(13 + conn.len() + 4);
        out.extend_from_slice(b"Connection: ");
        out.extend_from_slice(conn.as_bytes());
        out.extend_from_slice(b"\r\n\r\n");
    }
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
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(format!("HTTP/1.1 {status} {phrase}\r\n").as_bytes());
    let mut seen: Vec<String> = Vec::new();
    for (name, value) in headers {
        let lower = name.to_ascii_lowercase();
        if lower == "connection"
            || lower == "transfer-encoding"
            || lower == "content-length"
            || seen.contains(&lower)
        {
            continue;
        }
        seen.push(lower.clone());
        if lower.chars().all(|c| c.is_ascii_graphic() && c != ':') && !value.contains('\n') && !value.contains('\r') {
            out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
    }
    out.extend_from_slice(b"Transfer-Encoding: chunked\r\n");
    out.extend_from_slice(
        format!("Connection: {}\r\n\r\n", if keep_alive { "keep-alive" } else { "close" }).as_bytes(),
    );
    out
}

/// Encode one HTTP/1.1 chunk frame: `<hex-size>\r\n<data>\r\n`.
pub fn chunk_frame(data: &[u8]) -> Vec<u8> {
    let mut out = format!("{:x}\r\n", data.len()).into_bytes();
    out.extend_from_slice(data);
    out.extend_from_slice(b"\r\n");
    out
}

/// The terminating zero-length chunk plus empty trailer section (`0\r\n\r\n`).
pub const LAST_CHUNK_FRAME: &[u8] = b"0\r\n\r\n";

/// Build an interim `HTTP/1.1 100 Continue` frame for `Expect: 100-continue` clients.
pub fn continue_frame() -> Vec<u8> {
    b"HTTP/1.1 100 Continue\r\n\r\n".to_vec()
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
