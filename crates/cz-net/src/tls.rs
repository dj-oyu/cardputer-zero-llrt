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
            Ok(c) => certs.push(CertificateDer::from(c.to_vec())),
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
                // `ca:` pattern; everything else still fails closed.
                if Self::is_ca_as_end_entity(&e) && self.anchor_matches(end_entity) {
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

/// Build a `rustls::ClientConfig` from [`TlsClientOptions`], always using the `ring` provider.
pub fn build_client_config(opts: &TlsClientOptions) -> Result<Arc<ClientConfig>, String> {
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
                // Clone the cached system roots into a fresh store (RootCertStore::roots is public).
                let cached = system_root_store()?;
                builder.with_root_certificates(RootCertStore { roots: cached.roots.clone() })
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
        _ => builder.with_no_client_auth(),
    };


    // SNI on by default so virtual-hosted servers route correctly.
    if !opts.alpn_protocols.is_empty() {
        config.alpn_protocols = opts.alpn_protocols.clone();
    }
    Ok(Arc::new(config))
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
