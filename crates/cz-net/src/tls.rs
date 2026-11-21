//! Shared TLS foundation for Cardputer Zero LLRT (Phase 8A/8B/8C).
//!
//! Design rules (spec \u00a72/\u00a73):
//! - `rustls` with the **`ring`** CryptoProvider, never `aws-lc-rs`/OpenSSL.
//! - System root CA only (`rustls-native-certs`, which natively honors `SSL_CERT_FILE` /
//!   `SSL_CERT_DIR`) plus optional `NODE_EXTRA_CA_CERTS`; **no** `webpki-roots`.
//! - Root loading is lazy: a `OnceLock` is initialized on first TLS use so scripts that never
//!   call `fetch()` never touch `/etc/ssl/certs`.
//! - The builders here are reusable by Phase 8B (`node:https` server TLS) and Phase 8C
//!   (`node:tls` raw sockets), which keeps `cz-hal` strictly `libc`-only.

use std::io::Cursor;
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, ServerConfig, SignatureScheme};

/// Error text when no root certificates could be located at all (spec \u00a73.1).
pub const NO_ROOTS_ERROR: &str =
    "TLS error: no root certificates found (install ca-certificates or set SSL_CERT_FILE)";

/// Per-request client TLS options (the JS `fetch(..., { tls: {...} })` extension).
#[derive(Clone, Debug)]
pub struct TlsClientOptions {
    /// PEM-encoded CA certificate(s): replaces the system root store when provided.
    pub ca_pem: Option<Vec<u8>>,
    /// PEM-encoded client certificate chain (mTLS).
    pub cert_pem: Option<Vec<u8>>,
    /// PEM-encoded client private key (mTLS; SEC1 / PKCS#8 / PKCS#1).
    pub key_pem: Option<Vec<u8>>,
    /// When `false`, skip server certificate verification (real TLS is still negotiated).
    pub reject_unauthorized: bool,
    /// ALPN protocol identifiers offered in the ClientHello (Node `ALPNProtocols`).
    /// Empty (the default) offers no protocol.
    pub alpn_protocols: Vec<Vec<u8>>,
}

impl Default for TlsClientOptions {
    fn default() -> Self {
        TlsClientOptions {
            ca_pem: None,
            cert_pem: None,
            key_pem: None,
            // Web-compatible default: verify server certificates.
            reject_unauthorized: true,
            alpn_protocols: Vec::new(),
        }
    }
}

/// Server side TLS options, provided now so Phase 8B/8C can reuse this module without
/// re-implementing PEM plumbing (no stubs anywhere).
#[derive(Clone, Debug)]
pub struct TlsServerOptions {
    /// PEM-encoded server certificate chain (leaf first).
    pub cert_pem: Vec<u8>,
    /// PEM-encoded server private key.
    pub key_pem: Vec<u8>,
    /// Optional PEM-encoded client CA store for mTLS acceptance.
    pub ca_pem: Option<Vec<u8>>,
    /// Require and verify client certificates (requires `ca_pem`).
    pub require_client_auth: bool,
    /// ALPN protocol identifiers the server advertises (Node server `ALPNProtocols`).
    /// Empty (the default) advertises no protocol.
    pub alpn_protocols: Vec<Vec<u8>>,
}

impl Default for TlsServerOptions {
    fn default() -> Self {
        TlsServerOptions {
            cert_pem: Vec::new(),
            key_pem: Vec::new(),
            ca_pem: None,
            require_client_auth: false,
            alpn_protocols: Vec::new(),
        }
    }
}

/// Parse PEM certificates into DER form; errors when the blob contains no certificate.
pub fn parse_certificates(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>, String> {
    let mut certs = Vec::new();
    for item in rustls_pemfile::certs(&mut Cursor::new(pem)) {
        match item {
            Ok(c) => certs.push(c),
            Err(e) => return Err(format!("TLS error: invalid PEM certificate: {e}")),
        }
    }
    if certs.is_empty() {
        return Err("TLS error: no valid PEM certificate found".to_string());
    }
    Ok(certs)
}

/// Parse a PEM private key (SEC1 EC, PKCS#8 or PKCS#1 RSA) into DER form.
pub fn parse_private_key(pem: &[u8]) -> Result<PrivateKeyDer<'static>, String> {
    // `rustls_pemfile::private_key` yields the first key section of any type: PKCS#1, PKCS#8 or
    // SEC1 (the fixtures use SEC1 EC keys).
    match rustls_pemfile::private_key(&mut Cursor::new(pem)) {
        Ok(Some(key)) => Ok(key),
        Ok(None) => {
            Err("TLS error: no valid PEM private key found (expected SEC1, PKCS#8 or PKCS#1)".to_string())
        }
        Err(e) => Err(format!("TLS error: invalid PEM private key: {e}")),
    }
}

/// Load system roots without the process-wide cache (`SSL_CERT_FILE` / `SSL_CERT_DIR` aware).
///
/// Unit tests that vary these env vars use this entry point; production code uses
/// [`system_root_store`] which memoizes the result for the lifetime of the process.
pub fn load_system_roots_uncached() -> Result<RootCertStore, String> {
    let collected = rustls_native_certs::load_native_certs();
    // Individual parse failures are non-fatal (some distros ship oddball PEM entries), but log-free.
    let mut store = RootCertStore::empty();
    let (added, _ignored) = store.add_parsable_certificates(collected.certs);
    // NODE_EXTRA_CA_CERTS: a readable PEM file appended to the system store (Node-compatible).
    if let Some(path) = std::env::var_os("NODE_EXTRA_CA_CERTS") {
        let p = std::path::PathBuf::from(path);
        match std::fs::read(&p) {
            Ok(bytes) => match parse_certificates(&bytes) {
                Ok(certs) => {
                    let _ = store.add_parsable_certificates(certs);
                }
                Err(_) => {
                    // Unparseable extra CA file: keep going with whatever system roots exist,
                    // mirroring Node's "ignore unreadable extra certs" behavior for robustness.
                }
            },
            Err(_) => {}
        }
    }
    if store.is_empty() {
        // Neither system roots nor NODE_EXTRA_CA_CERTS produced anything usable.
        let _ = added;
        return Err(NO_ROOTS_ERROR.to_string());
    }
    Ok(store)
}

/// Process-wide lazily initialized root store (spec \u00a73.1). First TLS use pays the load; each
/// `cz-js` invocation is its own process so a `OnceLock` is safe here.
pub fn system_root_store() -> Result<Arc<RootCertStore>, String> {
    static CACHE: std::sync::OnceLock<Result<Arc<RootCertStore>, String>> = std::sync::OnceLock::new();
    match CACHE.get_or_init(|| load_system_roots_uncached().map(Arc::new)) {
        Ok(store) => Ok(store.clone()),
        Err(msg) => Err(msg.clone()),
    }
}

/// Verifier for explicit `ca_pem` overrides that mirrors Node's `tls.connect({ ca })` semantics:
/// webpki chain verification first, and when the end-entity certificate IS one of the provided
/// anchors (a self-signed leaf handed to the caller as its own CA), accept it directly. rustls'
/// stock webpki verifier rejects such anchors with `CertificateError::CaUsedAsEndEntity`, which
/// would break every self-signed loopback TLS case.
#[derive(Debug)]
struct SelfSignedTolerantVerifier {
    delegate: Arc<rustls::client::WebPkiServerVerifier>,
    anchors: Vec<CertificateDer<'static>>,
}

impl SelfSignedTolerantVerifier {
    fn new(
        delegate: Arc<rustls::client::WebPkiServerVerifier>,
        anchors: Vec<CertificateDer<'static>>,
    ) -> Self {
        Self { delegate, anchors }
    }

    /// webpki surfaces `CaUsedAsEndEntity` as `CertificateError::Other(OtherError(..))` whose
    /// `PartialEq` is always false (rustls error.rs line 1051), so identify it via its Debug text.
    fn is_ca_as_end_entity(err: &rustls::Error) -> bool {
        match err {
            rustls::Error::InvalidCertificate(rustls::CertificateError::Other(other)) => {
                other.to_string().contains("CaUsedAsEndEntity")
                    || format!("{other:?}").contains("CaUsedAsEndEntity")
            }
            _ => false,
        }
    }

    fn anchor_matches(&self, end_entity: &CertificateDer<'_>) -> bool {
        self.anchors.iter().any(|a| a.as_ref() == end_entity.as_ref())
    }
}

impl ServerCertVerifier for SelfSignedTolerantVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match self.delegate.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now) {
            Ok(v) => Ok(v),
            Err(e) => {
                // A self-signed leaf presented as its own trust anchor is the intentional Node
                // `ca:` pattern, but webpki hits `CaUsedAsEndEntity` before hostname/time checks.
                // Enforce self-issued, key purpose, validity window, and SAN/CN matching explicitly.
                if Self::is_ca_as_end_entity(&e) && self.anchor_matches(end_entity) {
                    verify_self_signed_leaf(end_entity, server_name, now)?;
                    Ok(ServerCertVerified::assertion())
                } else {
                    Err(e)
                }
            }
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.delegate.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.delegate.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.delegate.supported_verify_schemes()
    }
}

fn bad_cert_enc() -> rustls::Error {
    rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding)
}

fn der_tlv(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let &tag = input.first()?;
    let &len_byte = input.get(1)?;
    let (len, hdr_len) = if len_byte < 0x80 {
        (len_byte as usize, 2usize)
    } else {
        let n = (len_byte & 0x7f) as usize;
        if n == 0 || n > std::mem::size_of::<usize>() || input.len() < 2 + n {
            return None;
        }
        let mut len = 0usize;
        for &b in &input[2..2 + n] {
            len = (len << 8) | (b as usize);
        }
        if len < 0x80 || (n > 1 && input[2] == 0) {
            return None;
        }
        (len, 2 + n)
    };
    let end = hdr_len.checked_add(len)?;
    if end > input.len() {
        return None;
    }
    Some((tag, &input[hdr_len..end], &input[end..]))
}

fn der_expect<'a>(input: &'a [u8], expected_tag: u8) -> Option<(&'a [u8], &'a [u8])> {
    let (tag, body, rest) = der_tlv(input)?;
    if tag == expected_tag {
        Some((body, rest))
    } else {
        None
    }
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = y - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m_adj = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * m_adj as i64 + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn is_leap_year(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0)
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => if is_leap_year(y) { 29 } else { 28 },
        _ => 0,
    }
}

fn parse_dec2(b: &[u8]) -> Option<u32> {
    if b.len() < 2 || !b[0].is_ascii_digit() || !b[1].is_ascii_digit() {
        return None;
    }
    Some(((b[0] - b'0') as u32) * 10 + ((b[1] - b'0') as u32))
}

fn parse_x509_time(tag: u8, body: &[u8]) -> Option<i64> {
    let (year, off) = match tag {
        0x17 => {
            // UTCTime: YYMMDDHHMMSSZ
            if body.len() != 13 || body[12] != b'Z' {
                return None;
            }
            let yy = parse_dec2(&body[0..2])? as i64;
            let y = if yy >= 50 { 1900 + yy } else { 2000 + yy };
            (y, 2usize)
        }
        0x18 => {
            // GeneralizedTime: YYYYMMDDHHMMSSZ
            if body.len() != 15 || body[14] != b'Z' {
                return None;
            }
            let hi = parse_dec2(&body[0..2])? as i64;
            let lo = parse_dec2(&body[2..4])? as i64;
            (hi * 100 + lo, 4usize)
        }
        _ => return None,
    };
    let month = parse_dec2(&body[off..off + 2])?;
    let day = parse_dec2(&body[off + 2..off + 4])?;
    let hour = parse_dec2(&body[off + 4..off + 6])?;
    let minute = parse_dec2(&body[off + 6..off + 8])?;
    let second = parse_dec2(&body[off + 8..off + 10])?;
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + (hour as i64) * 3600 + (minute as i64) * 60 + (second.min(59) as i64))
}

fn extract_cns(name_body: &[u8]) -> Vec<String> {
    const OID_CN: &[u8] = &[0x55, 0x04, 0x03];
    let mut out = Vec::new();
    let mut cur = name_body;
    while !cur.is_empty() {
        let Some((set_body, next_rdn)) = der_expect(cur, 0x31) else {
            break;
        };
        cur = next_rdn;
        let mut rdn_cur = set_body;
        while !rdn_cur.is_empty() {
            let Some((atv_body, next_atv)) = der_expect(rdn_cur, 0x30) else {
                break;
            };
            rdn_cur = next_atv;
            let Some((oid, val_tlv)) = der_expect(atv_body, 0x06) else {
                continue;
            };
            if oid == OID_CN {
                if let Some((vtag, vbytes, _)) = der_tlv(val_tlv) {
                    if matches!(vtag, 0x0C | 0x13 | 0x16 | 0x14) {
                        if let Ok(s) = std::str::from_utf8(vbytes) {
                            out.push(s.to_string());
                        }
                    }
                }
            }
        }
    }
    out
}

fn dns_pattern_matches(pattern: &str, host: &str) -> bool {
    if !pattern.is_ascii() || pattern.is_empty() || host.is_empty() {
        return false;
    }
    if pattern.eq_ignore_ascii_case(host) {
        return true;
    }
    if let Some(rest) = pattern.strip_prefix("*.") {
        // Single-label wildcard only (e.g. *.example.com), never *.tld or multi-wildcard.
        if rest.is_empty() || rest.contains('*') || !rest.contains('.') {
            return false;
        }
        if host.len() <= rest.len() + 1 {
            return false;
        }
        let split = host.len() - rest.len();
        let (prefix, tail) = host.split_at(split);
        if let Some(label) = prefix.strip_suffix('.') {
            return !label.is_empty() && !label.contains('.') && tail.eq_ignore_ascii_case(rest);
        }
    }
    false
}

fn verify_self_signed_leaf(
    end_entity: &CertificateDer<'_>,
    server_name: &ServerName<'_>,
    now: UnixTime,
) -> Result<(), rustls::Error> {
    const OID_KEY_USAGE: &[u8] = &[0x55, 0x1D, 0x0F];
    const OID_SAN: &[u8] = &[0x55, 0x1D, 0x11];
    const OID_EKU: &[u8] = &[0x55, 0x1D, 0x25];
    const OID_SERVER_AUTH: &[u8] = &[0x2B, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01];
    const OID_ANY_EKU: &[u8] = &[0x55, 0x1D, 0x25, 0x00];

    let (cert_seq, cert_rest) = der_expect(end_entity.as_ref(), 0x30).ok_or_else(bad_cert_enc)?;
    if !cert_rest.is_empty() {
        return Err(bad_cert_enc());
    }
    let (tbs, _) = der_expect(cert_seq, 0x30).ok_or_else(bad_cert_enc)?;
    let mut cur = tbs;
    // Optional [0] explicit version
    if cur.first() == Some(&0xA0) {
        let (_, _, rest) = der_tlv(cur).ok_or_else(bad_cert_enc)?;
        cur = rest;
    }
    // serialNumber (INTEGER) & signature AlgorithmIdentifier (SEQUENCE)
    let (_, cur) = der_expect(cur, 0x02).ok_or_else(bad_cert_enc)?;
    let (_, cur) = der_expect(cur, 0x30).ok_or_else(bad_cert_enc)?;
    // issuer
    let (issuer_body, cur) = der_expect(cur, 0x30).ok_or_else(bad_cert_enc)?;
    // validity
    let (validity_body, cur) = der_expect(cur, 0x30).ok_or_else(bad_cert_enc)?;
    let (nb_tag, nb_body, val_rest) = der_tlv(validity_body).ok_or_else(bad_cert_enc)?;
    let (na_tag, na_body, val_tail) = der_tlv(val_rest).ok_or_else(bad_cert_enc)?;
    if !val_tail.is_empty() {
        return Err(bad_cert_enc());
    }
    let not_before = parse_x509_time(nb_tag, nb_body).ok_or_else(bad_cert_enc)?;
    let not_after = parse_x509_time(na_tag, na_body).ok_or_else(bad_cert_enc)?;
    let now_secs = now.as_secs() as i64;
    if now_secs < not_before {
        return Err(rustls::Error::InvalidCertificate(rustls::CertificateError::NotValidYet));
    }
    if now_secs > not_after {
        return Err(rustls::Error::InvalidCertificate(rustls::CertificateError::Expired));
    }
    // subject + subjectPublicKeyInfo
    let (subject_body, cur) = der_expect(cur, 0x30).ok_or_else(bad_cert_enc)?;
    if issuer_body != subject_body {
        return Err(rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer));
    }
    let (_, mut cur) = der_expect(cur, 0x30).ok_or_else(bad_cert_enc)?;

    // Optional [1] issuerUniqueID, [2] subjectUniqueID, [3] extensions
    let mut san_dns: Vec<String> = Vec::new();
    let mut san_ips: Vec<std::net::IpAddr> = Vec::new();
    let mut saw_san = false;
    while !cur.is_empty() {
        let (tag, body, rest) = der_tlv(cur).ok_or_else(bad_cert_enc)?;
        cur = rest;
        if tag != 0xA3 {
            continue;
        }
        let (exts_seq, exts_tail) = der_expect(body, 0x30).ok_or_else(bad_cert_enc)?;
        if !exts_tail.is_empty() {
            return Err(bad_cert_enc());
        }
        let mut exts = exts_seq;
        while !exts.is_empty() {
            let (ext_body, next_ext) = der_expect(exts, 0x30).ok_or_else(bad_cert_enc)?;
            exts = next_ext;
            let (oid, ext_rest) = der_expect(ext_body, 0x06).ok_or_else(bad_cert_enc)?;
            let ext_rest = if ext_rest.first() == Some(&0x01) {
                let (_, r) = der_expect(ext_rest, 0x01).ok_or_else(bad_cert_enc)?;
                r
            } else {
                ext_rest
            };
            let (ext_val, ext_tail) = der_expect(ext_rest, 0x04).ok_or_else(bad_cert_enc)?;
            if !ext_tail.is_empty() {
                return Err(bad_cert_enc());
            }
            if oid == OID_KEY_USAGE {
                let (bits, bits_tail) = der_expect(ext_val, 0x03).ok_or_else(bad_cert_enc)?;
                if !bits_tail.is_empty() || bits.len() < 2 {
                    return Err(bad_cert_enc());
                }
                let ku = bits[1];
                // Must allow TLS server authentication usage (digitalSignature | keyEncipherment | keyAgreement);
                // pure CA certs (only keyCertSign / cRLSign) must fail even if present in ca_pem.
                if (ku & (0x80 | 0x20 | 0x08)) == 0 {
                    return Err(rustls::Error::InvalidCertificate(rustls::CertificateError::InvalidPurpose));
                }
            } else if oid == OID_EKU {
                let (eku_seq, eku_tail) = der_expect(ext_val, 0x30).ok_or_else(bad_cert_enc)?;
                if !eku_tail.is_empty() {
                    return Err(bad_cert_enc());
                }
                let mut ecur = eku_seq;
                let mut server_auth_ok = false;
                while !ecur.is_empty() {
                    let (eoid, erest) = der_expect(ecur, 0x06).ok_or_else(bad_cert_enc)?;
                    ecur = erest;
                    if eoid == OID_SERVER_AUTH || eoid == OID_ANY_EKU {
                        server_auth_ok = true;
                    }
                }
                if !server_auth_ok {
                    return Err(rustls::Error::InvalidCertificate(rustls::CertificateError::InvalidPurpose));
                }
            } else if oid == OID_SAN {
                saw_san = true;
                let (san_seq, san_tail) = der_expect(ext_val, 0x30).ok_or_else(bad_cert_enc)?;
                if !san_tail.is_empty() {
                    return Err(bad_cert_enc());
                }
                let mut scur = san_seq;
                while !scur.is_empty() {
                    let (gtag, gbody, grest) = der_tlv(scur).ok_or_else(bad_cert_enc)?;
                    scur = grest;
                    if gtag == 0x82 {
                        if let Ok(name) = std::str::from_utf8(gbody) {
                            san_dns.push(name.to_string());
                        }
                    } else if gtag == 0x87 {
                        if gbody.len() == 4 {
                            san_ips.push(std::net::IpAddr::V4(std::net::Ipv4Addr::new(
                                gbody[0], gbody[1], gbody[2], gbody[3],
                            )));
                        } else if gbody.len() == 16 {
                            let mut oct = [0u8; 16];
                            oct.copy_from_slice(gbody);
                            san_ips.push(std::net::IpAddr::V6(std::net::Ipv6Addr::from(oct)));
                        }
                    }
                }
            }
        }
    }

    let name_ok = if saw_san {
        match server_name {
            ServerName::DnsName(dns) => san_dns.iter().any(|p| dns_pattern_matches(p, dns.as_ref())),
            ServerName::IpAddress(ip) => {
                let want = std::net::IpAddr::from(*ip);
                san_ips.iter().any(|a| *a == want)
            }
            _ => false,
        }
    } else {
        let cns = extract_cns(subject_body);
        match server_name {
            ServerName::DnsName(dns) => cns.iter().any(|cn| dns_pattern_matches(cn, dns.as_ref())),
            ServerName::IpAddress(ip) => {
                let want = std::net::IpAddr::from(*ip);
                cns.iter().any(|cn| cn.parse::<std::net::IpAddr>().ok() == Some(want))
            }
            _ => false,
        }
    };
    if !name_ok {
        return Err(rustls::Error::InvalidCertificate(rustls::CertificateError::NotValidForName));
    }
    Ok(())
}

/// Verifier used when `reject_unauthorized: false`: accepts any chain but still performs a real
/// TLS 1.2/1.3 handshake (encryption remains genuine; only certificate trust is bypassed).
#[derive(Debug)]
struct AcceptAnyVerifier;

impl ServerCertVerifier for AcceptAnyVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA1,
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
            SignatureScheme::ED448,
        ]
    }
}

/// True when the options describe the plain default HTTPS client (system roots, verification on,
/// no mTLS, no ALPN, no CA override). Exactly those configs are process-stable, so they can be
/// memoized and shared by every standard HTTPS `fetch()`.
pub fn is_default_client_options(opts: &TlsClientOptions) -> bool {
    opts.ca_pem.is_none()
        && opts.cert_pem.is_none()
        && opts.key_pem.is_none()
        && opts.reject_unauthorized
        && opts.alpn_protocols.is_empty()
}

/// Build a fresh `rustls::ClientConfig` from [`TlsClientOptions`] on every call (always the `ring`
/// provider). Unit tests that vary `SSL_CERT_FILE` / `SSL_CERT_DIR` / `NODE_EXTRA_CA_CERTS` must use
/// this entry point so their environment changes are actually observed; production HTTPS reuses the
/// memoized default through [`build_client_config`].
pub fn build_client_config_uncached(opts: &TlsClientOptions) -> Result<Arc<ClientConfig>, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    // Clone cheaply: the CA-verifier branch also needs the same ring provider.
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|e| format!("TLS error: unsupported protocol versions: {e}"))?;

    let builder = if !opts.reject_unauthorized {
        builder.dangerous().with_custom_certificate_verifier(Arc::new(AcceptAnyVerifier))
    } else {
        match &opts.ca_pem {
            Some(pem) => {
                // Explicit CA override: webpki verifier over the provided anchors, wrapped so a
                // self-signed end-entity that equals an anchor is accepted (Node `ca:` semantics).
                let certs = parse_certificates(pem)?;
                let mut store = RootCertStore::empty();
                let (added, ignored) = store.add_parsable_certificates(certs.clone());
                if added == 0 {
                    return Err(format!(
                        "TLS error: ca_pem contained no trusted certificate ({ignored} ignored)"
                    ));
                }
                let delegate = rustls::client::WebPkiServerVerifier::builder_with_provider(
                    Arc::new(store),
                    provider.clone(),
                )
                .build()
                .map_err(|e| format!("TLS error: cannot build CA verifier: {e}"))?;
                builder
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(SelfSignedTolerantVerifier::new(
                        delegate,
                        certs,
                    )))
            }
            None => {
                // Share the cached Arc<RootCertStore> directly (avoids cloning every TrustAnchor).
                let cached = system_root_store()?;
                builder.with_root_certificates(cached)
            }
        }
    };

    let mut config = match (&opts.cert_pem, &opts.key_pem) {
        (Some(cert_pem), Some(key_pem)) => {
            let chain: Vec<CertificateDer<'static>> = parse_certificates(cert_pem)?;
            let key = parse_private_key(key_pem)?;
            builder
                .with_client_auth_cert(chain, key)
                .map_err(|e| format!("TLS error: invalid client certificate/key pair: {e}"))?
        }
        (Some(_), None) | (None, Some(_)) => {
            return Err("TLS error: cert_pem and key_pem must both be provided for client authentication".to_string());
        }
        (None, None) => builder.with_no_client_auth(),
    };


    // SNI on by default so virtual-hosted servers route correctly.
    if !opts.alpn_protocols.is_empty() {
        config.alpn_protocols = opts.alpn_protocols.clone();
    }
    Ok(Arc::new(config))
}

/// Build a `rustls::ClientConfig` from [`TlsClientOptions`], always using the `ring` provider.
///
/// Phase 10C: the DEFAULT configuration (system roots + verification on, no mTLS, no ALPN, no CA
/// override) is memoized in a process-wide `OnceLock`, so standard HTTPS `fetch()` calls share one
/// `Arc<ClientConfig>` instead of rebuilding the config and its `WebPkiServerVerifier` on every
/// request (that config also keeps holding the shared cached `Arc<RootCertStore>`). Anything that
/// differs from the default — explicit `ca`, mTLS pairs, `reject_unauthorized: false`, ALPN — still
/// builds fresh through [`build_client_config_uncached`].
pub fn build_client_config(opts: &TlsClientOptions) -> Result<Arc<ClientConfig>, String> {
    if !is_default_client_options(opts) {
        return build_client_config_uncached(opts);
    }
    static CACHE: std::sync::OnceLock<Result<Arc<ClientConfig>, String>> = std::sync::OnceLock::new();
    match CACHE.get_or_init(|| build_client_config_uncached(opts)) {
        Ok(cfg) => Ok(cfg.clone()),
        Err(msg) => Err(msg.clone()),
    }
}

/// Build a `rustls::ServerConfig` (used by Phase 8B/8C; complete and tested already).
pub fn build_server_config(opts: &TlsServerOptions) -> Result<Arc<ServerConfig>, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let certs = parse_certificates(&opts.cert_pem)?;
    let key = parse_private_key(&opts.key_pem)?;

    let builder = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|e| format!("TLS error: unsupported protocol versions: {e}"))?;

    let mut config = if opts.require_client_auth {
        let ca_pem = opts
            .ca_pem
            .as_ref()
            .ok_or("TLS error: require_client_auth needs a client CA (ca_pem)")?;
        let client_certs = parse_certificates(ca_pem)?;
        let mut client_store = RootCertStore::empty();
        let (added, _) = client_store.add_parsable_certificates(client_certs);
        if added == 0 {
            return Err("TLS error: client CA contained no certificate".to_string());
        }
        let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(client_store))
            .build()
            .map_err(|e| format!("TLS error: invalid client verifier: {e}"))?;
        builder
            .with_client_cert_verifier(verifier)
            .with_single_cert(certs, key)
            .map_err(|e| format!("TLS error: invalid server certificate/key: {e}"))?
    } else {
        builder
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| format!("TLS error: invalid server certificate/key: {e}"))?
    };
    if !opts.alpn_protocols.is_empty() {
        config.alpn_protocols = opts.alpn_protocols.clone();
    }
    Ok(Arc::new(config))
}

/// Convenience: canonical TLS reason phrase for an HTTP status (ureq 3 does not surface the
/// remote's reason phrase, so the IANA canonical text is used like browsers do).
pub fn status_text(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        101 => "Switching Protocols",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        413 => "Content Too Large",
        414 => "URI Too Long",
        415 => "Unsupported Media Type",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "",
    }
}

#[cfg(test)]
mod tls_tests {
    use super::*;

    fn fixture_cert(name: &str) -> CertificateDer<'static> {
        let pem = std::fs::read(format!("{}/testdata/{name}", env!("CARGO_MANIFEST_DIR")))
            .expect("fixture pem must exist");
        parse_certificates(&pem).expect("parse cert").into_iter().next().unwrap()
    }

    fn build_tolerant_verifier(cert: CertificateDer<'static>) -> SelfSignedTolerantVerifier {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut store = RootCertStore::empty();
        store.add(cert.clone()).expect("add anchor");
        let delegate = rustls::client::WebPkiServerVerifier::builder_with_provider(
            Arc::new(store),
            provider,
        )
        .build()
        .expect("build verifier");
        SelfSignedTolerantVerifier::new(delegate, vec![cert])
    }

    #[test]
    fn self_signed_verifier_checks_hostname_and_validity_and_rejects_ca_only_anchor() {
        let self_cert = fixture_cert("self-cert.pem");
        let verifier = build_tolerant_verifier(self_cert.clone());

        // Fixture validity is 2026-11-21..2036-11-18; pick 2028-01-01 as valid "now".
        let valid_now = UnixTime::since_unix_epoch(std::time::Duration::from_secs(1_830_297_600));
        let localhost = ServerName::try_from("localhost").unwrap();
        assert!(verifier
            .verify_server_cert(&self_cert, &[], &localhost, &[], valid_now)
            .is_ok());

        // Wrong DNS host must fail closed.
        let evil = ServerName::try_from("evil.example").unwrap();
        assert!(matches!(
            verifier.verify_server_cert(&self_cert, &[], &evil, &[], valid_now),
            Err(rustls::Error::InvalidCertificate(rustls::CertificateError::NotValidForName))
        ));

        // IP without IP SAN must fail closed.
        let ip = ServerName::from(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
        assert!(matches!(
            verifier.verify_server_cert(&self_cert, &[], &ip, &[], valid_now),
            Err(rustls::Error::InvalidCertificate(rustls::CertificateError::NotValidForName))
        ));

        // Expired timestamp (2040-01-01) and pre-notBefore timestamp (2020-01-01) must fail.
        let expired_now = UnixTime::since_unix_epoch(std::time::Duration::from_secs(2_208_988_800));
        assert!(matches!(
            verifier.verify_server_cert(&self_cert, &[], &localhost, &[], expired_now),
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::Expired | rustls::CertificateError::ExpiredContext { .. }
            ))
        ));
        assert!(matches!(
            verify_self_signed_leaf(&self_cert, &localhost, expired_now),
            Err(rustls::Error::InvalidCertificate(rustls::CertificateError::Expired))
        ));
        let early_now = UnixTime::since_unix_epoch(std::time::Duration::from_secs(1_577_836_800));
        assert!(matches!(
            verifier.verify_server_cert(&self_cert, &[], &localhost, &[], early_now),
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::NotValidYet | rustls::CertificateError::NotValidYetContext { .. }
            ))
        ));
        assert!(matches!(
            verify_self_signed_leaf(&self_cert, &localhost, early_now),
            Err(rustls::Error::InvalidCertificate(rustls::CertificateError::NotValidYet))
        ));

        // CA-only anchor (KeyUsage = keyCertSign, cRLSign; no SAN) must never pass as end-entity.
        let ca_cert = fixture_cert("ca.pem");
        let ca_verifier = build_tolerant_verifier(ca_cert.clone());
        assert!(ca_verifier
            .verify_server_cert(&ca_cert, &[], &localhost, &[], valid_now)
            .is_err());

        // Partial mTLS (cert without key or key without cert) must fail rather than downgrade.
        assert!(build_client_config_uncached(&TlsClientOptions {
            cert_pem: Some(b"dummy".to_vec()),
            ..Default::default()
        })
        .is_err());
        assert!(build_client_config_uncached(&TlsClientOptions {
            key_pem: Some(b"dummy".to_vec()),
            ..Default::default()
        })
        .is_err());
    }
}
