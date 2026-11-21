//! Zero-allocation WHATWG-style URL parser: ONE owned buffer plus `Copy` byte spans.
//!
//! Every field of a parsed URL is an index pair into a single owned `Vec<u8>` href buffer, so a
//! parse never allocates one `String` per component (guidelines §1: *Own Once, Borrow Everywhere*).
//! Normalization streams bytes straight into the caller's buffer: dot-segment removal pops the
//! write cursor in place, default ports are elided while writing, and IPv6 brackets are validated
//! by scanning — no temporary `Vec`s/`String`s on any path. A warm reuse loop therefore measures
//! **0 allocations** (`crates/cz-runner/tests/alloc_audit.rs::alloc_audit_url_parse_reuse_1000`).
//!
//! This single implementation backs BOTH the global `URL` class (`cz-modules/src/url_module.rs`)
//! and the RFC 6455 WebSocket target split (`cz-net/src/ws.rs`), so `ws://` / `wss://` resolution
//! and relative resolution can never drift apart.
//!
//! Documented deviations from WHATWG (deliberate, device-friendly): percent escapes already present
//! are kept (their hex uppercased) instead of decode/re-encode round-tripping; `%2e` dot segments
//! are honored, other escaped separators are not; IDNA/punycode hostnames pass through verbatim.

use std::borrow::Cow;

/// Byte span `[start, end)` inside the single owned href buffer. `Copy` so reads never clone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    const EMPTY: Span = Span { start: 0, end: 0 };
    fn put(start: usize, end: usize) -> Span {
        Span { start, end }
    }
    fn text(self, href: &str) -> &str {
        href.get(self.start..self.end).unwrap_or("")
    }
    fn filled(self) -> bool {
        self.end > self.start
    }
}

// Field slots in `Parsed.fields` (kept as fixed indices so nothing clones or allocates).
const F_SCHEME: usize = 0;
const F_USER: usize = 1;
const F_PASS: usize = 2;
const F_HOST: usize = 3; // brackets included for IPv6, port excluded
const F_PORT: usize = 4; // "" when absent OR equal to the scheme default port
const F_PATH: usize = 5;
const F_QUERY: usize = 6; // includes its leading '?'
const F_FRAG: usize = 7; // includes its leading '#'
const N_FIELDS: usize = 8;

/// The parsed URL carries a `//` authority section (userinfo / host / port).
pub const FLAG_AUTHORITY: u8 = 1 << 0;
/// WHATWG "special" scheme (http/https/ws/wss/ftp/file).
pub const FLAG_SPECIAL: u8 = 1 << 1;
/// Non-special scheme without an authority — a cannot-be-a-base URL (`data:`/`mailto:`).
pub const FLAG_OPAQUE: u8 = 1 << 2;

/// Span table produced by [`parse_url_into`]; owns no heap data of its own.
#[derive(Debug, Clone, Copy)]
pub struct Parsed {
    fields: [Span; N_FIELDS],
    flags: u8,
}

impl Parsed {
    fn new(flags: u8) -> Parsed {
        Parsed {
            fields: [Span::EMPTY; N_FIELDS],
            flags,
        }
    }
}

/// Borrowed accessor view over one normalized href buffer.
#[derive(Debug)]
pub struct Url<'a> {
    href: &'a str,
    p: Parsed,
}

impl<'a> Url<'a> {
    /// Bind a normalized href to the parser's span table.
    pub fn from_parsed(href: &'a str, p: Parsed) -> Url<'a> {
        Url { href, p }
    }

    pub fn href(&self) -> &'a str {
        self.href
    }
    /// `"https:"` — lowercase and always colon-suffixed.
    pub fn protocol(&self) -> &'a str {
        self.p.fields[F_SCHEME].text(self.href)
    }
    pub fn username(&self) -> &'a str {
        self.p.fields[F_USER].text(self.href)
    }
    pub fn password(&self) -> &'a str {
        self.p.fields[F_PASS].text(self.href)
    }
    /// Host without the port; IPv6 literals keep their brackets (`"[::1]"`).
    pub fn hostname(&self) -> &'a str {
        self.p.fields[F_HOST].text(self.href)
    }
    /// `""` when absent or equal to the scheme default port.
    pub fn port(&self) -> &'a str {
        self.p.fields[F_PORT].text(self.href)
    }
    /// `host[:port]`. Written adjacently in the href, so this always borrows (zero alloc): the
    /// span runs from the host start through the port digits (the `:` sits inside it).
    pub fn host(&self) -> Cow<'a, str> {
        let host = self.p.fields[F_HOST];
        let port = self.p.fields[F_PORT];
        if port.filled() {
            Cow::Borrowed(self.href.get(host.start..port.end).unwrap_or(""))
        } else {
            Cow::Borrowed(host.text(self.href))
        }
    }
    /// Absolute normalized path (or the opaque path of a cannot-be-a-base URL).
    pub fn pathname(&self) -> &'a str {
        self.p.fields[F_PATH].text(self.href)
    }
    /// `"?query"` / `""` — borrowed straight from the href (zero alloc).
    pub fn search(&self) -> &'a str {
        self.p.fields[F_QUERY].text(self.href)
    }
    /// `"#fragment"` / `""` — borrowed straight from the href (zero alloc).
    pub fn hash(&self) -> &'a str {
        self.p.fields[F_FRAG].text(self.href)
    }
    pub fn is_special(&self) -> bool {
        self.p.flags & FLAG_SPECIAL != 0
    }
    pub fn has_authority(&self) -> bool {
        self.p.flags & FLAG_AUTHORITY != 0
    }
    pub fn is_opaque(&self) -> bool {
        self.p.flags & FLAG_OPAQUE != 0
    }
    /// `scheme://host[:port]` for special URLs with a host, `"null"` otherwise. With no userinfo
    /// the origin sits contiguously in the href, so it borrows instead of allocating.
    pub fn origin(&self) -> Cow<'a, str> {
        if !self.is_special() || !self.has_authority() || !self.p.fields[F_HOST].filled() {
            return Cow::Borrowed("null");
        }
        let scheme = self.p.fields[F_SCHEME];
        let host = self.p.fields[F_HOST];
        let port = self.p.fields[F_PORT];
        let userinfo_empty = !self.p.fields[F_USER].filled() && !self.p.fields[F_PASS].filled();
        if userinfo_empty && host.start == scheme.end + 2 {
            // "https:" + "//" + host (+ ":port") are adjacent bytes in the href buffer.
            let end = if port.filled() { port.end } else { host.end };
            if let Some(s) = self.href.get(scheme.start..end) {
                return Cow::Borrowed(s);
            }
        }
        let host_text = host.text(self.href);
        if port.filled() {
            Cow::Owned(format!("{}//{}:{}", self.protocol(), host_text, port.text(self.href)))
        } else {
            Cow::Owned(format!("{}//{}", self.protocol(), host_text))
        }
    }

    /// Request target for HTTP/WS upgrades: absolute path + query (`"/ws?q=1"`).
    pub fn request_target(&self) -> Cow<'a, str> {
        let path = self.pathname();
        let search = self.search();
        if search.is_empty() {
            Cow::Borrowed(if path.is_empty() { "/" } else { path })
        } else {
            Cow::Owned(format!("{path}{search}"))
        }
    }
}

/// Parsed URL that OWNS its single href buffer (spans stay integer indexes: no self-referencing
/// borrow, so the struct can move freely across threads).
#[derive(Debug)]
pub struct OwnedUrl {
    buf: Vec<u8>,
    p: Parsed,
}

impl OwnedUrl {
    pub fn href(&self) -> &str {
        // Parsing only appends ASCII to UTF-8 input bytes, so the buffer stays valid UTF-8.
        std::str::from_utf8(&self.buf).unwrap_or("")
    }
    /// Borrow a full accessor view over the owned buffer.
    pub fn view(&self) -> Url<'_> {
        Url::from_parsed(self.href(), self.p)
    }
    /// Move the normalized href out as an owned `String`.
    pub fn into_string(self) -> String {
        String::from_utf8(self.buf).unwrap_or_default()
    }
}

/// Parse `input` (optionally resolved against an absolute `base_href`) into the CALLER's buffer.
///
/// Warm-buffer contract: once `out`'s capacity covers the result, repeated calls allocate NOTHING —
/// every normalization step writes/truncates bytes in place.
pub fn parse_url_into<'buf>(
    input: &str,
    base_href: Option<&str>,
    out: &'buf mut Vec<u8>,
) -> Result<Url<'buf>, &'static str> {
    out.clear();
    let text = trim_url_chars(input);
    if text.is_empty() && base_href.is_none() {
        // Node rejects an empty string ONLY without a base; `new URL("", base)` resolves to the
        // base itself (minus its fragment), so an empty reference still parses here.
        return Err("Invalid URL: empty input without a base");
    }

    // 1. Scheme detection (`[A-Za-z][A-Za-z0-9+.-]*:`), borrowed without copying.
    let scheme_len = find_scheme(text);
    let relative = scheme_len == 0;
    let rest = &text[scheme_len..];

    // 2. Borrow the base decomposition only when a relative reference needs it.
    let mut base_pieces: Option<Split<'_>> = None;
    if relative {
        let b = match base_href {
            Some(b) => trim_url_chars(b),
            None => return Err("Invalid URL: relative reference without a base"),
        };
        match split_href(b) {
            Some(s) => base_pieces = Some(s),
            None => return Err("Invalid base URL"),
        }
    }

    let effective_scheme: &str = if relative {
        base_pieces.as_ref().expect("base parsed").scheme
    } else {
        &text[..scheme_len]
    };
    if effective_scheme.trim_end_matches(':').is_empty() {
        return Err("Invalid URL: malformed scheme");
    }
    let special = is_special(effective_scheme);
    // Relative resolution against a cannot-be-a-base URL fails (WHATWG "cannot-be-a-base").
    if relative && base_pieces.as_ref().map(|s| s.opaque).unwrap_or(false) {
        return Err("Invalid URL: relative reference against a non-hierarchical base");
    }

    // 3. Write the lowercase scheme NAME, then its ':' (spans keep the canonical "https:" form).
    let scheme_name = effective_scheme.trim_end_matches(':');
    if !valid_scheme_chars(scheme_name) {
        return Err("Invalid URL: malformed scheme");
    }
    let scheme_start = out.len();
    push_lower(out, scheme_name);
    out.push(b':');
    let scheme_end = out.len();

    let mut flags: u8 = if special { FLAG_SPECIAL } else { 0 };
    let mut parsed = Parsed::new(flags);
    parsed.fields[F_SCHEME] = Span::put(scheme_start, scheme_end);

    let input_authority = rest.starts_with("//");
    if !special && !relative && !input_authority {
        // Absolute non-special URL without `//`: opaque ("data:text/x", "mailto:a@b").
        flags |= FLAG_OPAQUE;
        parsed.flags = flags;
        write_opaque_tail(out, rest, &mut parsed);
        return Ok(Url::from_parsed(bytes_to_str(out), parsed));
    }

    // Authority text: from the input after `//`, from the base (relative), or inherited when a
    // special URL is written bare ("http/path") against an identical-scheme base.
    let auth_end = if input_authority {
        // Index is RELATIVE TO THE BODY (`rest[2..]`) — a missing terminator spans the whole body.
        find_first(&rest[2..], &[b'/', b'\\', b'?', b'#']).unwrap_or(rest.len() - 2)
    } else {
        0
    };
    let (auth_text, tail) = if input_authority {
        let body = &rest[2..];
        (&body[..auth_end], &body[auth_end..])
    } else if relative {
        ("", rest)
    } else {
        match base_pieces.as_ref() {
            Some(s) if !s.opaque && eq_ignore_ascii(s.scheme, effective_scheme) => {
                ("", rest)
            }
            _ => {
                if special {
                    return Err("Invalid URL: missing host for a special scheme");
                }
                flags |= FLAG_OPAQUE;
                parsed.flags = flags;
                write_opaque_tail(out, rest, &mut parsed);
                return Ok(Url::from_parsed(bytes_to_str(out), parsed));
            }
        }
    };

    // Base authority inheritance for relative references / bare special URLs.
    let mut authority_used = auth_text;
    let inherited = if !input_authority {
        base_pieces.as_ref().and_then(|s| s.authority).unwrap_or("")
    } else {
        ""
    };
    if authority_used.is_empty() {
        authority_used = inherited;
    }
    if !special && authority_used.is_empty() && !relative {
        // "schemeish:path" with no authority at all: opaque form after all.
        flags |= FLAG_OPAQUE;
        parsed.flags = flags;
        write_opaque_tail(out, tail, &mut parsed);
        return Ok(Url::from_parsed(bytes_to_str(out), parsed));
    }

    // Hierarchical form: "scheme:" + "//" + authority + path (?query) (#fragment).
    out.extend_from_slice(b"//");
    write_authority(out, authority_used, effective_scheme, &mut parsed)?;
    flags |= FLAG_AUTHORITY;
    parsed.flags = flags;

    // 4. Path / query / fragment follow RFC 3986 + Node's WHATWG rules (measured against Node v22):
    //    * ""          -> base path + base query, fragment CLEARED
    //    * "?x"        -> base path + NEW query, fragment cleared
    //    * "#y"        -> base path + base query + NEW fragment
    //    * "g"/"/g"    -> resolved path; query/fragment come from the reference alone
    let (path_src, query_src, frag_src) = split_path_query_frag(tail);
    let tail_has_query = find_byte(tail.as_bytes(), b'?').is_some();
    // Measured against Node: a relative reference with NO path and NO explicit `?` keeps the
    // base's query — even when it carries a fragment ("#y" -> "/p?q=1#y").
    let keep_base_query = relative && !tail_has_query && path_src.is_empty();
    let query_effective: &str = if keep_base_query {
        base_pieces.as_ref().expect("base").query
    } else {
        query_src
    };

    let path_start = out.len();
    if relative && path_src.is_empty() {
        // Empty / query-only / fragment-only references keep the base's absolute path verbatim.
        normalize_path_into(out, base_pieces.as_ref().expect("base").path, special, path_start);
    } else if relative && !starts_sep(path_src, special) {
        // RFC 3986 merge: drop the base's last segment, then resolve the reference remainder.
        let bp = base_pieces.as_ref().expect("base").path;
        match bp.rfind('/') {
            Some(i) => normalize_path_into(out, &bp[..=i], special, path_start),
            None => out.push(b'/'),
        }
        normalize_tail(out, path_src, special, path_start + 1);
    } else {
        normalize_path_into(out, path_src, special, path_start);
    }
    parsed.fields[F_PATH] = Span::put(path_start, out.len());

    if !query_effective.is_empty() {
        let qs = out.len();
        out.push(b'?');
        push_sanitized(out, query_effective.as_bytes(), query_ok);
        parsed.fields[F_QUERY] = Span::put(qs, out.len());
    }
    if !frag_src.is_empty() {
        let fs = out.len();
        out.push(b'#');
        push_sanitized(out, frag_src.as_bytes(), frag_ok);
        parsed.fields[F_FRAG] = Span::put(fs, out.len());
    }

    Ok(Url::from_parsed(bytes_to_str(out), parsed))
}

/// Convenience owned parse (allocates exactly ONE buffer for the normalized href).
pub fn parse_url(input: &str, base_href: Option<&str>) -> Result<OwnedUrl, &'static str> {
    let mut buf = Vec::new();
    let view = parse_url_into(input, base_href, &mut buf)?;
    let p = view.p; // `Parsed` is `Copy`, so the spans survive moving the buffer.
    Ok(OwnedUrl { buf, p })
}

// ---------------------------------------------------------------------------
// Borrowed splitting helpers (zero allocation)
// ---------------------------------------------------------------------------

/// Borrowed decomposition of an absolute url text used for relative resolution.
struct Split<'a> {
    scheme: &'a str,
    authority: Option<&'a str>,
    path: &'a str,
    /// Base query WITHOUT its '?' marker (empty when the base has none).
    query: &'a str,
    opaque: bool,
}

/// Path component separator test ('/' always, plus '\' for special schemes like WHATWG).
fn starts_sep(s: &str, special: bool) -> bool {
    s.starts_with('/') || (special && s.starts_with('\\'))
}

fn split_href(text: &str) -> Option<Split<'_>> {
    let sl = find_scheme(text);
    if sl == 0 || !valid_scheme_chars(text[..sl - 1].trim_end_matches(':')) {
        return None;
    }
    let scheme = &text[..sl];
    let rest = &text[sl..];
    if !rest.starts_with("//") {
        return Some(Split {
            scheme,
            authority: None,
            path: "",
            query: "",
            opaque: true,
        });
    }
    let body = &rest[2..];
    let auth_end = find_first(body, &[b'/', b'\\', b'?', b'#']).unwrap_or(body.len());
    let tail = &body[auth_end..];
    let (path, query, _f) = split_path_query_frag(tail);
    Some(Split {
        scheme,
        authority: Some(&body[..auth_end]),
        // A bare "scheme://host" still has a hierarchical root path.
        path: if path.is_empty() { "/" } else { path },
        query,
        opaque: false,
    })
}

/// Split `path?query#fragment` into its three parts (both markers excluded).
fn split_path_query_frag(text: &str) -> (&str, &str, &str) {
    let frag_at = find_byte(text.as_bytes(), b'#');
    let head = &text[..frag_at.unwrap_or(text.len())];
    let frag = match frag_at {
        Some(i) => &text[i + 1..],
        None => "",
    };
    let q_at = find_byte(head.as_bytes(), b'?');
    match q_at {
        Some(i) => (&head[..i], &head[i + 1..], frag),
        None => (head, "", frag),
    }
}

fn write_opaque_tail(out: &mut Vec<u8>, rest: &str, parsed: &mut Parsed) {
    let (path, query, frag) = split_path_query_frag(rest);
    let path_start = out.len();
    push_sanitized(out, path.as_bytes(), opaque_ok);
    parsed.fields[F_PATH] = Span::put(path_start, out.len());
    if !query.is_empty() {
        let qs = out.len();
        out.push(b'?');
        push_sanitized(out, query.as_bytes(), query_ok);
        parsed.fields[F_QUERY] = Span::put(qs, out.len());
    }
    if !frag.is_empty() {
        let fs = out.len();
        out.push(b'#');
        push_sanitized(out, frag.as_bytes(), frag_ok);
        parsed.fields[F_FRAG] = Span::put(fs, out.len());
    }
}

/// Write canonical `user[:pass@]host[:port]`, filling the userinfo/host/port spans.
fn write_authority(
    out: &mut Vec<u8>,
    auth: &str,
    scheme: &str,
    parsed: &mut Parsed,
) -> Result<(), &'static str> {
    // Userinfo ends at the LAST '@' (passwords may legitimately contain escaped '@').
    let at_at = rfind_byte(auth.as_bytes(), b'@');
    let (userinfo, hostport) = match at_at {
        Some(i) => (&auth[..i], &auth[i + 1..]),
        None => ("", auth),
    };

    let (host_text, port_text) = if hostport.starts_with('[') {
        let close = match rfind_byte(hostport.as_bytes(), b']') {
            Some(i) => i,
            None => return Err("Invalid URL: unterminated IPv6 host literal"),
        };
        let after = &hostport[close + 1..];
        if after.is_empty() {
            (hostport, "")
        } else if after.starts_with(':') && after.len() > 1 {
            (&hostport[..close + 1], &after[1..])
        } else {
            return Err("Invalid URL: unexpected data after the IPv6 host literal");
        }
    } else {
        // A colon INSIDE brackets never splits; a bare host splits on its last ':'.
        match hostport.rfind(':') {
            Some(i) => (&hostport[..i], &hostport[i + 1..]),
            None => (hostport, ""),
        }
    };

    // Only `file:` may carry an empty host (Node: `file:///tmp/x` stays valid).
    let allow_empty_host = scheme.trim_end_matches(':').eq_ignore_ascii_case("file");
    if host_text.is_empty() && !allow_empty_host {
        return Err("Invalid URL: empty host");
    }

    if !userinfo.is_empty() {
        // Node keeps the `user:pass` separator exactly when the caller wrote one; `user@host`
        // stays without a colon, and an empty password still prints as `user:@host`? measured:
        // Node turns "u:@h" into "u@h", so a colon appears ONLY alongside a non-empty password.
        let (user, pass) = match userinfo.find(':') {
            Some(i) => (&userinfo[..i], &userinfo[i + 1..]),
            None => (userinfo, ""),
        };
        if user.contains('@') {
            return Err("Invalid URL: unencoded '@' in userinfo");
        }
        if !user.is_empty() {
            let us = out.len();
            push_sanitized(out, user.as_bytes(), userinfo_ok);
            parsed.fields[F_USER] = Span::put(us, out.len());
        }
        if !pass.is_empty() {
            out.push(b':');
        }
        let ps = out.len();
        push_sanitized(out, pass.as_bytes(), userinfo_ok);
        parsed.fields[F_PASS] = Span::put(ps, out.len());
        out.push(b'@');
    }

    if !host_text.is_empty() {
        let hs = out.len();
        if host_text.starts_with('[') {
            write_ipv6_host(out, host_text)?;
        } else {
            push_host_lower(out, host_text)?;
        }
        parsed.fields[F_HOST] = Span::put(hs, out.len());
    }

    if port_text.is_empty() {
        let ps = out.len();
        parsed.fields[F_PORT] = Span::put(ps, ps);
    } else {
        if !port_text.bytes().all(|b| b.is_ascii_digit()) {
            return Err("Invalid URL: invalid port");
        }
        let value: u32 = port_text
            .parse::<u32>()
            .map_err(|_| "Invalid URL: port out of range")?;
        if value > 65_535 {
            return Err("Invalid URL: port out of range");
        }
        // Elide the scheme's default port from `.port`, `.host` and `.href` exactly like Node.
        if Some(value as u16) == default_port(scheme) {
            let ps = out.len();
            parsed.fields[F_PORT] = Span::put(ps, ps);
        } else {
            out.push(b':');
            let ps = out.len(); // `.port` is DIGITS ONLY (no leading ':')
            push_decimal(out, value);
            parsed.fields[F_PORT] = Span::put(ps, out.len());
        }
    }
    Ok(())
}

/// Lowercase a registered hostname, keeping valid percent escapes uppercase.
fn push_host_lower(out: &mut Vec<u8>, host: &str) -> Result<(), &'static str> {
    let bytes = host.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' {
            if i + 2 < bytes.len() && hex_val(bytes[i + 1]).is_some() && hex_val(bytes[i + 2]).is_some() {
                out.push(b'%');
                out.push(HEX_UPPER[hex_val(bytes[i + 1]).unwrap_or(0) as usize]);
                out.push(HEX_UPPER[hex_val(bytes[i + 2]).unwrap_or(0) as usize]);
                i += 3;
                continue;
            }
            out.push(b'%');
            out.push(b'2');
            out.push(b'5');
            i += 1;
            continue;
        }
        if !host_ok(b) {
            return Err("Invalid URL: forbidden host character");
        }
        out.push(b.to_ascii_lowercase());
        i += 1;
    }
    Ok(())
}

/// Validate and copy a bracketed IPv6 literal (lowercased, brackets preserved).
fn write_ipv6_host(out: &mut Vec<u8>, host: &str) -> Result<(), &'static str> {
    if host.len() < 4 || !host.ends_with(']') {
        return Err("Invalid URL: invalid IPv6 host literal");
    }
    let inner = &host[1..host.len() - 1];
    if !ipv6_ok(inner) {
        return Err("Invalid URL: invalid IPv6 address");
    }
    out.push(b'[');
    for b in inner.as_bytes() {
        out.push(b.to_ascii_lowercase());
    }
    out.push(b']');
    Ok(())
}

/// RFC 4291-ish IPv6 shape check (`::` compression at most once, optional IPv4 tail, zone ids).
fn ipv6_ok(s: &str) -> bool {
    // Drop a `%zone` / `%25zone` interface identifier before structural validation.
    let body = match find_byte(s.as_bytes(), b'%') {
        Some(i) => &s[..i],
        None => s,
    };
    if body.is_empty() {
        return false;
    }
    let bytes = body.as_bytes();
    let mut i = 0usize;
    let mut groups = 0usize;
    let mut compressions = 0usize;
    // A lone ':' is legal ONLY between two groups; `::` marks at most one compression run.
    let mut expect_group = true;
    while i < bytes.len() {
        if bytes[i] == b':' {
            if i + 1 < bytes.len() && bytes[i + 1] == b':' {
                if compressions > 0 {
                    return false; // a second `::` is malformed
                }
                compressions += 1;
                i += 2;
                expect_group = true;
                continue;
            }
            if expect_group {
                return false; // leading ':', doubled ':' without compression, or trailing ':'
            }
            i += 1;
            expect_group = true;
            continue;
        }
        // One group of 1-4 hex digits, or an IPv4-mapped dotted quad tail (worth 2 groups).
        let start = i;
        while i < bytes.len() && bytes[i] != b':' {
            if bytes[i] == b'.' {
                if !ipv4_ok(&body[start..]) {
                    return false;
                }
                groups += 2;
                return compressions <= 1 && groups <= 8;
            }
            if !bytes[i].is_ascii_hexdigit() {
                return false;
            }
            i += 1;
            if i - start > 4 {
                return false;
            }
        }
        groups += 1;
        if groups > 8 {
            return false;
        }
        expect_group = false;
    }
    if compressions == 0 && groups != 8 {
        return false; // uncompressed forms must spell all eight groups
    }
    compressions <= 1 && groups <= 8
}

/// Dotted-quad IPv4 tail inside an IPv6 literal.
fn ipv4_ok(s: &str) -> bool {
    let mut octets = 0usize;
    for part in s.split('.') {
        if part.is_empty() || part.len() > 3 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        match part.parse::<u16>() {
            Ok(v) if v <= 255 => {}
            _ => return false,
        }
        octets += 1;
    }
    octets == 4
}

/// Write a fully normalized ABSOLUTE path starting at `/` (dot segments removed in place).
/// Write a fully normalized ABSOLUTE path starting at `/`. `floor` is the href offset where the
/// path region begins — `..` pops may NEVER cut into the scheme/authority that precedes it.
fn normalize_path_into(out: &mut Vec<u8>, src: &str, special: bool, floor: usize) {
    out.push(b'/'); // root anchor: a `..` pop can never make the path relative again
    normalize_tail(out, src, special, floor);
}

/// Dot-segment resolution over one path source, writing sanitized segments into `out`.
///
/// The write cursor IS the accumulator (`pop_last_segment` truncates it), so no scratch buffer,
/// component `Vec` or memmove ever happens.
fn normalize_tail(out: &mut Vec<u8>, src: &str, special: bool, floor: usize) {
    let bytes = src.as_bytes();
    let mut i = 0usize;
    if !bytes.is_empty() && (bytes[0] == b'/' || (special && bytes[0] == b'\\')) {
        i = 1; // the root anchor already supplies this separator
    }
    let mut last_dot = false;
    while i < bytes.len() {
        let sep = find_byte_from(bytes, i, special);
        let comp_end = sep.unwrap_or(bytes.len());
        let comp = &bytes[i..comp_end];
        let followed = sep.is_some();
        let matches_dot2 = is_dot(comp, 2);
        if is_dot(comp, 1) {
            last_dot = true;
        } else if matches_dot2 {
            pop_last_segment(out, floor);
            last_dot = true;
        } else if comp.is_empty() {
            // An empty segment contributes only its separator (keeps `//b` shapes).
            last_dot = false;
        } else {
            push_sanitized(out, comp, path_ok);
            last_dot = false;
        }
        // Dot components (`.` / `..`) never ADD a separator when the accumulator already ends
        // with one (the anchor they leave behind IS that separator). Empty segments still emit
        // theirs so `//b` shapes survive exactly like Node's.
        if followed {
            let skip_sep = out.ends_with(b"/") && (matches_dot2 || is_dot(comp, 1));
            if skip_sep {
                // the pop's anchor separator IS this segment's separator
            } else {
                out.push(b'/');
            }
        }
        i = comp_end + if followed { 1 } else { 0 };
    }
    // Trailing separators / trailing dots leave a directory-like tail (WHATWG path state).
    let trailing_sep = match bytes.last() {
        Some(b'/') => true,
        Some(b'\\') if special => true,
        _ => false,
    };
    let ends_at_sep = out.ends_with(b"/");
    if (trailing_sep || last_dot) && !ends_at_sep {
        out.push(b'/');
    }
}

/// Remove the last path segment plus its preceding `/`; the root anchor always survives.
/// Pop the last written PATH segment (and its separator), never cutting below `floor` — the offset
/// where the path region starts inside the shared href buffer.
fn pop_last_segment(out: &mut Vec<u8>, floor: usize) {
    if out.ends_with(b"/") && out.len() > floor + 1 {
        out.pop();
    }
    match out[floor..].iter().rposition(|b| *b == b'/') {
        Some(rel) => out.truncate(floor + rel + 1),
        None => out.truncate(floor.max(1)),
    }
}

/// `.` / `..` component test that also accepts `%2e`-escaped dot segments (WHATWG honors them).
fn is_dot(comp: &[u8], dots: usize) -> bool {
    let mut got = 0usize;
    let mut i = 0usize;
    while i < comp.len() {
        let w = if comp[i] == b'.' {
            if got + 1 > dots {
                return false;
            }
            got += 1;
            1
        } else if percent_is(&comp[i..], b'.') {
            if got + 1 > dots {
                return false;
            }
            got += 1;
            3
        } else {
            return false;
        };
        i += w;
    }
    got == dots && i == comp.len()
}

/// Recognize a `%xx` escape for one target ASCII byte (case-insensitive hex).
fn percent_is(s: &[u8], want: u8) -> bool {
    if s.len() < 3 || s[0] != b'%' {
        return false;
    }
    match (hex_val(s[1]), hex_val(s[2])) {
        (Some(h), Some(l)) => ((h << 4) | l) == want,
        _ => false,
    }
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(10 + (b - b'a')),
        b'A'..=b'F' => Some(10 + (b - b'A')),
        _ => None,
    }
}

const HEX_UPPER: &[u8; 16] = b"0123456789ABCDEF";

/// Percent-encode a byte range into `out`. Existing `%XX` escapes survive VERBATIM (Node/WHATWG
/// keeps the caller's own hex case, e.g. `/a%2fb`), and a stray `%` is only escaped when the two
/// bytes after it are not hex digits.
fn push_sanitized(out: &mut Vec<u8>, src: &[u8], ok: fn(u8) -> bool) {
    let mut i = 0usize;
    while i < src.len() {
        let b = src[i];
        if b == b'%' {
            if i + 2 < src.len() && hex_val(src[i + 1]).is_some() && hex_val(src[i + 2]).is_some() {
                out.extend_from_slice(&src[i..i + 3]); // keep the escape exactly as given
                i += 3;
                continue;
            }
            // Node keeps malformed/lone percent sequences EXACTLY (`%zz`, `%7g`, `%`) — measured.
            out.push(b'%');
            i += 1;
            continue;
        }
        if ok(b) {
            out.push(b);
            i += 1;
        } else {
            out.push(b'%');
            out.push(HEX_UPPER[(b >> 4) as usize]);
            out.push(HEX_UPPER[(b & 0x0F) as usize]);
            i += 1;
        }
    }
}

fn push_lower(out: &mut Vec<u8>, s: &str) {
    for b in s.as_bytes() {
        out.push(b.to_ascii_lowercase());
    }
}

/// Decimal port digits from a fixed stack buffer (never allocates).
fn push_decimal(out: &mut Vec<u8>, mut v: u32) {
    if v == 0 {
        out.push(b'0');
        return;
    }
    let mut tmp = [0u8; 10];
    let mut n = tmp.len();
    while v > 0 {
        n -= 1;
        tmp[n] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    out.extend_from_slice(&tmp[n..]);
}

// ---------------------------------------------------------------------------
// Character classes and scanning primitives (all stack-side, zero alloc)
// ---------------------------------------------------------------------------

fn unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}
fn sub_delims(b: u8) -> bool {
    // RFC 3986 sub-delims — '&' matters: Node keeps `?a=1&b=2` query strings verbatim.
    matches!(b, b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b'+' | b',' | b';' | b'=')
}
fn path_ok(b: u8) -> bool {
    unreserved(b) || sub_delims(b) || matches!(b, b':' | b'@' | b'/') || b == b'%'
}
fn opaque_ok(b: u8) -> bool {
    unreserved(b) || sub_delims(b) || matches!(b, b':' | b'@' | b'/' | b'?') || b == b'%'
}
fn query_ok(b: u8) -> bool {
    unreserved(b) || sub_delims(b) || matches!(b, b':' | b'@' | b'/' | b'?' | b'#') || b == b'%'
}
fn frag_ok(b: u8) -> bool {
    unreserved(b) || sub_delims(b) || matches!(b, b':' | b'@' | b'/' | b'?' | b'#') || b == b'%'
}
fn userinfo_ok(b: u8) -> bool {
    unreserved(b) || sub_delims(b) || b == b':' || b == b'%'
}
fn host_ok(b: u8) -> bool {
    // '%' is handled separately (valid escapes kept, others escaped as %25).
    unreserved(b) || matches!(b, b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b'+' | b',' | b';' | b'=' | b'-' | b'.' | b'_' | b'~')
}

fn valid_scheme_chars(s: &str) -> bool {
    let mut first = true;
    for b in s.as_bytes() {
        if first {
            if !b.is_ascii_alphabetic() {
                return false;
            }
            first = false;
        } else if !(b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.')) {
            return false;
        }
    }
    !s.is_empty()
}

/// WHATWG special schemes (file included — it permits an empty host).
pub fn is_special(scheme: &str) -> bool {
    let l = scheme.trim_end_matches(':');
    eq_ignore_ascii(l, "http")
        || eq_ignore_ascii(l, "https")
        || eq_ignore_ascii(l, "ws")
        || eq_ignore_ascii(l, "wss")
        || eq_ignore_ascii(l, "ftp")
        || eq_ignore_ascii(l, "file")
}

/// Default ports whose explicit form is elided from `.port`, `.host` and `.href`.
pub fn default_port(scheme: &str) -> Option<u16> {
    let l = scheme.trim_end_matches(':');
    if eq_ignore_ascii(l, "http") || eq_ignore_ascii(l, "ws") {
        Some(80)
    } else if eq_ignore_ascii(l, "https") || eq_ignore_ascii(l, "wss") {
        Some(443)
    } else if eq_ignore_ascii(l, "ftp") {
        Some(21)
    } else {
        None
    }
}

fn eq_ignore_ascii(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).all(|(x, y)| x.eq_ignore_ascii_case(&y))
}

/// Length INCLUDING the `:` of a leading scheme (`0` when absent or malformed).
fn find_scheme(text: &str) -> usize {
    let bytes = text.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_alphabetic() {
        return 0;
    }
    for (i, b) in bytes.iter().enumerate() {
        if *b == b':' {
            return i + 1;
        }
        if !(b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.')) {
            return 0;
        }
    }
    0
}

/// Strip leading/trailing C0 controls, spaces and DEL without copying anything.
pub fn trim_url_chars(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut a = 0usize;
    while a < bytes.len() && (bytes[a] <= 0x20 || bytes[a] == 0x7F) {
        a += 1;
    }
    let mut z = bytes.len();
    while z > a && (bytes[z - 1] <= 0x20 || bytes[z - 1] == 0x7F) {
        z -= 1;
    }
    &s[a..z]
}

fn find_byte_from(bytes: &[u8], from: usize, backslash_too: bool) -> Option<usize> {
    for i in from..bytes.len() {
        if bytes[i] == b'/' || (backslash_too && bytes[i] == b'\\') {
            return Some(i);
        }
    }
    None
}

fn find_byte(bytes: &[u8], needle: u8) -> Option<usize> {
    bytes.iter().position(|b| *b == needle)
}

fn find_first(s: &str, needles: &[u8]) -> Option<usize> {
    s.as_bytes().iter().position(|b| needles.contains(b))
}

fn rfind_byte(bytes: &[u8], needle: u8) -> Option<usize> {
    bytes.iter().rposition(|b| *b == needle)
}

/// Re-interpret the parse buffer as UTF-8 (always valid: input was UTF-8 and everything appended
/// here is ASCII).
fn bytes_to_str(buf: &Vec<u8>) -> &str {
    std::str::from_utf8(buf).unwrap_or("")
}

#[cfg(test)]
mod url_tests {
    use super::*;

    fn parse(input: &str, base: Option<&str>) -> OwnedUrl {
        parse_url(input, base).expect("parse")
    }

    #[test]
    fn absolute_normalization_lowercases_host_and_elides_default_ports() {
        let u = parse("https://user:pass@Example.COM:443/v1/old", None);
        let v = u.view();
        assert_eq!(v.protocol(), "https:");
        assert_eq!(v.hostname(), "example.com");
        assert_eq!(v.port(), "");
        assert_eq!(v.host(), "example.com");
        assert_eq!(v.pathname(), "/v1/old");
        assert_eq!(v.username(), "user");
        assert_eq!(v.password(), "pass");
        assert_eq!(v.origin(), "https://example.com");
        assert_eq!(v.href(), "https://user:pass@example.com/v1/old");
    }

    #[test]
    fn non_default_ports_survive_everywhere() {
        let u = parse("https://example.com:8443/a/c", None);
        assert_eq!(u.view().port(), "8443");
        assert_eq!(u.view().host(), "example.com:8443");
        assert_eq!(u.view().origin(), "https://example.com:8443");
        assert_eq!(parse("http://a.test:80/x", None).view().href(), "http://a.test/x");
        assert_eq!(parse("ws://a.test:80/ws", None).view().port(), "");
        assert_eq!(parse("wss://a.test:443/s", None).view().port(), "");
    }

    #[test]
    fn relative_resolution_merges_and_pops_dot_segments() {
        let up = parse("../b?x=1#h", Some("https://example.com:8443/a/c"));
        assert_eq!(up.view().pathname(), "/b");
        assert_eq!(up.view().search(), "?x=1");
        assert_eq!(up.view().hash(), "#h");
        assert_eq!(up.view().href(), "https://example.com:8443/b?x=1#h");

        // The Phase 11 gate case: userinfo is inherited from the base too.
        let deep = parse("../api/items?a=1&b=2#sec", Some("https://user:pass@Example.COM:443/v1/old"));
        assert_eq!(deep.view().href(), "https://user:pass@example.com/api/items?a=1&b=2#sec");

        assert_eq!(parse("./z", Some("http://a.test/p/q")).view().pathname(), "/p/z");
        assert_eq!(parse("../../z", Some("http://a.test/p/q/r")).view().pathname(), "/z");
        assert_eq!(parse("http://a.test/./b/../c", None).view().pathname(), "/c");
        assert_eq!(parse("http://a.test", None).view().pathname(), "/");
        assert_eq!(parse("http://a.test/a/b/..", None).view().pathname(), "/a/");
        // Node ground truth: empty segment survives (`//b`); empty reference KEEPS base query/fragment;
        // a query-only reference replaces the query and clears the fragment; fragment-only keeps query.
        assert_eq!(parse("http://a.test//b", None).view().pathname(), "//b"); // Node keeps the empty first segment
        assert_eq!(parse("/x", Some("http://a.test")).view().href(), "http://a.test/x");
        assert_eq!(parse("", Some("http://a.test/p?q=1")).view().href(), "http://a.test/p?q=1");
        assert_eq!(parse("?only", Some("http://a.test/p?q=1")).view().href(), "http://a.test/p?only");
        assert_eq!(
            parse("#frag", Some("http://a.test/p?q=1")).view().href(),
            "http://a.test/p?q=1#frag"
        );
        assert_eq!(parse("//host/q", Some("https://base.test/p")).view().href(), "https://host/q");
    }

    #[test]
    fn percent_escapes_normalize_and_forbidden_bytes_escape() {
        // Node ground truth: hrefs keep caller escapes exactly (`/a%2fb%20c`, `#f%7g`).
        assert_eq!(parse("http://a.test/a%2fb%20c", None).view().pathname(), "/a%2fb%20c");
        assert_eq!(parse("http://a.test/p#f%7g", None).view().hash(), "#f%7g");
        let space = parse("http://a.test/a b?q r", None);
        assert_eq!(space.view().pathname(), "/a%20b");
        assert_eq!(space.view().search(), "?q%20r");
    }

    #[test]
    fn ipv6_brackets_survive_and_ports_parse() {
        let u = parse("http://[::1]:8080/p", None);
        assert_eq!(u.view().hostname(), "[::1]");
        assert_eq!(u.view().port(), "8080");
        assert_eq!(u.view().host(), "[::1]:8080");
        assert_eq!(u.view().href(), "http://[::1]:8080/p");
        assert_eq!(
            parse("ws://[2001:DB8::ff00:42:8329]/x", None).view().hostname(),
            "[2001:db8::ff00:42:8329]"
        );
        assert_eq!(parse("http://[::ffff:1.2.3.4]/p", None).view().hostname(), "[::ffff:1.2.3.4]");
        assert!(parse_url("http://[::1", None).is_err());
        assert!(parse_url("http://[:::1]/p", None).is_err());
        assert!(parse_url("http://[1.2.3.4.5]/p", None).is_err());
    }

    #[test]
    fn ws_wss_and_opaque_schemes() {
        let u = parse("wss://example.com/secure?a=1", None);
        assert_eq!(u.view().protocol(), "wss:");
        assert!(u.view().is_special());
        assert_eq!(u.view().request_target(), "/secure?a=1");
        assert_eq!(u.view().origin(), "wss://example.com");

        let d = parse("data:text/plain,a%20b", None);
        assert!(d.view().is_opaque());
        assert!(!d.view().has_authority());
        assert_eq!(d.view().hostname(), "");
        assert_eq!(d.view().origin(), "null");
        assert_eq!(d.view().href(), "data:text/plain,a%20b");

        let m = parse("mailto:someone@example.com", None);
        assert!(m.view().is_opaque());
        assert_eq!(m.view().pathname(), "someone@example.com");

        let custom = parse("myco://host.test/path", None);
        assert!(custom.view().has_authority());
        assert_eq!(custom.view().hostname(), "host.test");
        assert_eq!(custom.view().pathname(), "/path");
    }

    #[test]
    fn invalid_inputs_and_invalid_bases_reject() {
        assert!(parse_url("not a url", None).is_err());
        assert!(parse_url("/x", None).is_err());
        assert!(parse_url("/x", Some("nonsense")).is_err());
        assert!(parse_url("http://", None).is_err());
        assert!(parse_url("", None).is_err());
        assert!(parse_url("https://a.test:99999/p", None).is_err());
        assert!(parse_url("ht tp://a.test/", None).is_err());
        assert!(parse_url("http://a test/p", None).is_err()); // bare space in host -> forbidden
    }

    #[test]
    fn file_scheme_allows_the_empty_host() {
        let u = parse("file:///tmp/x", None);
        assert_eq!(u.view().hostname(), "");
        assert_eq!(u.view().pathname(), "/tmp/x");
        assert_eq!(u.view().href(), "file:///tmp/x");
    }

    #[test]
    fn reuse_of_a_warm_buffer_produces_identical_views() {
        let mut buf: Vec<u8> = Vec::new();
        buf.reserve(64);
        let first = parse_url_into("https://a.test/p?x=1#f", None, &mut buf).unwrap();
        assert_eq!(first.href(), "https://a.test/p?x=1#f");
        assert_eq!(first.search(), "?x=1");
        let second = parse_url_into("../b", Some("https://a.test/p?x=1#f"), &mut buf).unwrap();
        assert_eq!(second.href(), "https://a.test/b");
        assert_eq!(second.origin(), "https://a.test");
    }
}

