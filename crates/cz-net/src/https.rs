//! Phase 8B: TLS acceptor bridging `cz-net`'s rustls stack into `cz-hal`'s HTTP engine
//! [`cz_hal::http_server::StreamAcceptor`] hook — without pulling any TLS code into `cz-hal`
//! (which stays strictly `libc`-only) and without forbidden deps (`tokio`, `openssl`,
//! `native-tls`, `aws-lc-rs`, `webpki-roots`).
//!
//! Each accepted `TcpStream` gets `TCP_NODELAY`, a bounded handshake window
//! ([`HANDSHAKE_TIMEOUT`]), a `rustls::ServerConnection` driven to completion with
//! `complete_io`, and is wrapped in a [`TlsConnStream`] (`rustls::StreamOwned`) implementing
//! [`cz_hal::http_server::ConnStream`] with `is_encrypted() -> true`. Socket timeouts are
//! restored afterwards so the long-lived request/response path behaves exactly like plain HTTP.
//! A failed handshake (plaintext client on the HTTPS port, missing/invalid client cert under
//! mTLS) returns `Err` so the engine's per-connection worker thread exits cleanly — the server
//! itself never crashes or wedges.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::Duration;

use cz_hal::http_server::{ConnStream, StreamAcceptor};
use rustls::{ServerConnection, StreamOwned};

use crate::tls::{build_server_config, TlsServerOptions};

/// Upper bound for one TLS handshake before the connection is dropped (a stalled or hostile
/// client can never pin a worker thread indefinitely).
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// A TLS-wrapped connection: `rustls::StreamOwned<ServerConnection, TcpStream>` presented to the
/// HTTP engine as a plaintext [`ConnStream`] (`is_encrypted() -> true`).
pub struct TlsConnStream {
    stream: StreamOwned<ServerConnection, TcpStream>,
}

impl Read for TlsConnStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.stream.read(buf)
    }
}

impl Write for TlsConnStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.stream.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}

impl ConnStream for TlsConnStream {
    fn set_nodelay(&self) -> std::io::Result<()> {
        self.stream.get_ref().set_nodelay(true)
    }
    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.stream.get_ref().set_read_timeout(timeout)
    }
    fn set_write_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.stream.get_ref().set_write_timeout(timeout)
    }
    fn shutdown_both(&mut self) {
        // Best-effort TLS `close_notify` FIRST (so peers see an orderly session end), drained
        // through the raw socket with a guard, then shut down both TCP directions.
        let rustls::StreamOwned { conn, sock } = &mut self.stream;
        conn.send_close_notify();
        let mut guard = 0;
        while conn.wants_write() && guard < 100 {
            guard += 1;
            if conn.write_tls(sock).is_err() {
                break;
            }
        }
        sock.shutdown(Shutdown::Both).ok();
    }
    fn peer_addr_str(&self) -> String {
        self.stream
            .get_ref()
            .peer_addr()
            .map(|a| a.ip().to_string())
            .unwrap_or_default()
    }
    fn raw_fd(&self) -> std::os::fd::RawFd {
        self.stream.get_ref().as_raw_fd()
    }
    fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.stream.get_ref().local_addr()
    }
    fn is_encrypted(&self) -> bool {
        true
    }
}

/// Build a [`StreamAcceptor`] for an `https.createServer`-style TLS listener. The server config
/// (PEM parse + rustls validation, including mTLS when `require_client_auth`) is built ONCE here,
/// so invalid PEM/key pairs fail fast with a descriptive `String` error. The returned closure
/// runs on the engine's per-connection worker thread.
pub fn make_tls_acceptor(opts: &TlsServerOptions) -> Result<StreamAcceptor, String> {
    let cfg = build_server_config(opts)?;
    Ok(Arc::new(move |mut tcp: TcpStream| -> std::io::Result<Box<dyn ConnStream>> {
        tcp.set_nodelay(true)?;
        // Bound the handshake window BEFORE wrapping: rustls' blocking `complete_io` inherits
        // these socket timeouts, so a silent client fails instead of hanging the worker.
        tcp.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        tcp.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
        let mut conn = ServerConnection::new(cfg.clone()).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("TLS error: {e}"))
        })?;
        if let Err(e) = conn.complete_io(&mut tcp) {
            return Err(e); // plaintext-on-TLS / mTLS rejection: drop THIS socket only
        }
        // Handshake done: restore plain-blocking semantics for the long-lived HTTP path.
        tcp.set_read_timeout(None).ok();
        tcp.set_write_timeout(None).ok();
        Ok(Box::new(TlsConnStream {
            stream: StreamOwned::new(conn, tcp),
        }))
    }))
}

#[cfg(test)]
mod https_tests {
    use super::*;
    use cz_hal::http_server::{
        spawn_listener, start_listener_with_acceptor, stop_listener, ConnectionEvent, ReplyFrame,
        ResponseAction,
    };
    use rustls::pki_types::ServerName;
    use std::net::TcpListener;
    use std::sync::mpsc::channel;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("{}/testdata/{name}", env!("CARGO_MANIFEST_DIR")))
            .expect("fixture pem must exist")
    }

    fn server_opts() -> TlsServerOptions {
        TlsServerOptions {
            cert_pem: fixture("server-cert.pem"),
            key_pem: fixture("server-key.pem"),
            ca_pem: None,
            require_client_auth: false,
            alpn_protocols: Vec::new(),
        }
    }

    fn client_cfg(ca: &[u8], client_cert: Option<Vec<u8>>, client_key: Option<Vec<u8>>) -> Arc<rustls::ClientConfig> {
        let mut store = rustls::RootCertStore::empty();
        let certs = crate::tls::parse_certificates(ca).expect("ca parses");
        let (added, _) = store.add_parsable_certificates(certs);
        assert!(added > 0, "CA store must contain at least one anchor");
        let builder = rustls::ClientConfig::builder_with_provider(
            Arc::new(rustls::crypto::ring::default_provider()),
        )
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .expect("protocol versions");
        let cfg = match (client_cert, client_key) {
            (Some(cert), Some(key)) => builder
                .with_root_certificates(store)
                .with_client_auth_cert(
                    crate::tls::parse_certificates(&cert).expect("client cert parses"),
                    crate::tls::parse_private_key(&key).expect("client key parses"),
                )
                .expect("client auth cert accepted"),
            _ => builder.with_root_certificates(store).with_no_client_auth(),
        };
        Arc::new(cfg)
    }

    fn sni() -> ServerName<'static> {
        ServerName::try_from("localhost".to_string()).expect("sni name")
    }

    /// Assert an acceptor build FAILS, without needing `Debug` on the Ok payload.
    fn expect_tls_error(res: Result<StreamAcceptor, String>, what: &str) -> String {
        match res {
            Ok(_) => panic!("{what}: invalid configuration must be rejected"),
            Err(e) => e,
        }
    }

    /// Drive one HTTP/1.1 request through the engine over a TLS acceptor and hand back the
    /// completed [`ConnectionEvent`] plus the raw response bytes the client eventually sees.
    fn tls_roundtrip(
        acceptor: &StreamAcceptor,
        client: Arc<rustls::ClientConfig>,
        request: &[u8],
    ) -> Result<(ConnectionEvent, Vec<u8>), String> {
        let (tx, rx) = channel::<ConnectionEvent>();
        let mut listener =
            start_listener_with_acceptor("127.0.0.1", 0, 91, tx, Some(acceptor.clone()))
                .map_err(|e| format!("bind: {e}"))?;
        let port = listener.port;

        let sock = TcpStream::connect(("127.0.0.1", port)).map_err(|e| format!("connect: {e}"))?;
        sock.set_read_timeout(Some(Duration::from_secs(8)))
            .map_err(|e| format!("timeout: {e}"))?;
        let mut conn = rustls::ClientConnection::new(client, sni()).map_err(|e| format!("client conn: {e}"))?;
        conn.complete_io(&mut sock.try_clone().expect("cli handshake clone"))
            .map_err(|e| format!("client handshake: {e}"))?;
        let mut stream = rustls::StreamOwned::new(conn, sock);
        stream.write_all(request).map_err(|e| format!("write req: {e}"))?;
        stream.flush().map_err(|e| format!("flush req: {e}"))?;

        let ev = rx
            .recv_timeout(Duration::from_secs(8))
            .map_err(|e| format!("no engine event: {e:?}"))?;

        // Answer once (buffered, no keep-alive) so the engine closes the socket afterwards.
        ev.reply
            .send(ResponseAction::Buffered(ReplyFrame {
                status: 200,
                reason: Some("OK".to_string()),
                headers: vec![("Content-Type".to_string(), "text/plain".to_string())],
                body: b"tls-pong".to_vec(),
                keep_alive: false,
            }))
            .map_err(|_| "reply channel lost".to_string())?;

        let mut resp = Vec::new();
        let mut buf = [0u8; 512];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break, // close_notify/EOF from the engine shutdown_both path
                Ok(n) => {
                    resp.extend_from_slice(&buf[..n]);
                    if resp.windows(8).any(|w| w == b"tls-pong") {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(format!("client read: {e}")),
            }
        }
        drop(stream);
        stop_listener(&mut listener);
        Ok((ev, resp))
    }

    fn std_opts() -> TlsServerOptions {
        server_opts()
    }

    #[test]
    fn make_tls_acceptor_rejects_mismatched_cert_key_pair() {
        // server cert signed by the CA but the WRONG private key (client key): must fail fast.
        let opts = TlsServerOptions {
            cert_pem: fixture("server-cert.pem"),
            key_pem: fixture("client-key.pem"),
            ca_pem: None,
            require_client_auth: false,
            alpn_protocols: Vec::new(),
        };
        let err = expect_tls_error(make_tls_acceptor(&opts), "mismatched cert/key");
        assert!(err.contains("TLS error"), "descriptive TLS error expected: {err}");
    }

    #[test]
    fn make_tls_acceptor_rejects_garbage_pem_and_missing_ca_for_mtls() {
        let garbage = TlsServerOptions {
            cert_pem: b"not a pem at all".to_vec(),
            key_pem: fixture("server-key.pem"),
            ca_pem: None,
            require_client_auth: false,
            alpn_protocols: Vec::new(),
        };
        let err = expect_tls_error(make_tls_acceptor(&garbage), "garbage PEM cert");
        assert!(err.contains("TLS error"), "garbage PEM error expected: {err}");

        // require_client_auth WITHOUT a client CA is a configuration error, not a panic.
        let mtls_no_ca = TlsServerOptions {
            cert_pem: fixture("server-cert.pem"),
            key_pem: fixture("server-key.pem"),
            ca_pem: None,
            require_client_auth: true,
            alpn_protocols: Vec::new(),
        };
        let err = expect_tls_error(make_tls_acceptor(&mtls_no_ca), "mTLS without client CA");
        assert!(err.contains("client CA"), "CA-missing config error expected: {err}");
    }

    #[test]
    fn tls_acceptor_engine_roundtrip_marks_event_encrypted() {
        let acceptor = make_tls_acceptor(&std_opts()).expect("valid TLS opts must build an acceptor");
        let client = client_cfg(&fixture("ca.pem"), None, None);
        let (ev, resp) = tls_roundtrip(
            &acceptor,
            client,
            b"GET /hi HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .expect("TLS roundtrip must succeed");
        assert!(ev.encrypted, "TLS connections must report encrypted=true");
        assert_eq!(ev.request.head.method, "GET");
        assert_eq!(ev.request.head.target, "/hi");
        let text = String::from_utf8(resp).expect("response utf-8");
        assert!(text.starts_with("HTTP/1.1 200 OK"), "200 expected: {text:?}");
        assert!(text.contains("tls-pong"), "body expected: {text:?}");
    }

    #[test]
    fn plain_http_events_report_unencrypted_via_trait_path() {
        let (tx, rx) = channel::<ConnectionEvent>();
        let mut listener = spawn_listener("127.0.0.1", 0, 92, tx).expect("plain bind");
        let port = listener.port;
        let mut sock = TcpStream::connect(("127.0.0.1", port)).expect("plain connect");
        sock.set_read_timeout(Some(Duration::from_secs(8))).expect("timeout");
        sock.write_all(b"GET /plain HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .expect("req");
        sock.flush().expect("flush");
        let ev = rx.recv_timeout(Duration::from_secs(8)).expect("plain event");
        assert!(!ev.encrypted, "plain TCP must report encrypted=false");
        ev.reply
            .send(ResponseAction::Buffered(ReplyFrame {
                status: 200,
                reason: Some("OK".to_string()),
                headers: vec![],
                body: b"plain-pong".to_vec(),
                keep_alive: false,
            }))
            .expect("reply");
        let mut buf = Vec::new();
        let mut tmp = [0u8; 256];
        loop {
            match sock.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.windows(10).any(|w| w == b"plain-pong") {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        stop_listener(&mut listener);
    }

    #[test]
    fn tls_acceptor_direct_handshake_timeout_is_bounded() {
        // A silent client must fail within the bounded handshake window, NOT hang forever.
        let acceptor = make_tls_acceptor(&std_opts()).expect("acceptor");
        let l = TcpListener::bind("127.0.0.1:0").expect("bind raw");
        let port = l.local_addr().unwrap().port();
        let stalled = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let (tx, rx) = channel::<std::io::Result<Box<dyn ConnStream>>>();
        let t0 = std::time::Instant::now();
        let worker = std::thread::spawn(move || {
            let (accepted, _) = l.accept().expect("accept");
            tx.send(acceptor(accepted)).expect("send result");
        });
        let result = rx.recv_timeout(Duration::from_secs(8)).expect("acceptor must return");
        let elapsed = t0.elapsed();
        worker.join().expect("worker");
        drop(stalled);
        assert!(result.is_err(), "stalled handshake must end in an io error");
        assert!(
            elapsed >= Duration::from_millis(4500) && elapsed < Duration::from_secs(8),
            "bounded ~{HANDSHAKE_TIMEOUT:?} window expected, took {elapsed:?}"
        );
    }

    #[test]
    fn plaintext_client_on_tls_port_is_isolated_and_server_survives() {
        let acceptor = make_tls_acceptor(&std_opts()).expect("acceptor");
        let (tx, rx) = channel::<ConnectionEvent>();
        let mut listener =
            start_listener_with_acceptor("127.0.0.1", 0, 93, tx, Some(acceptor.clone()))
                .expect("bind tls engine");
        let port = listener.port;

        // Attacker-style plaintext client: sends raw HTTP at a TLS port and hangs up.
        {
            let mut plain = TcpStream::connect(("127.0.0.1", port)).expect("plain connect");
            plain.set_read_timeout(Some(Duration::from_secs(4))).expect("t");
            plain
                .write_all(b"GET /evil HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .expect("evil req");
            let mut tmp = [0u8; 64];
            // The server-side worker will fail the handshake and drop THIS socket only:
            // read either errors or sees EOF quickly; no event must reach JS for it.
            match plain.read(&mut tmp) {
                Ok(0) | Err(_) => {}
                Ok(_) => {} // rustls alert bytes: also acceptable, connection is dead anyway
            }
            assert!(
                rx.recv_timeout(Duration::from_millis(400)).is_err(),
                "plaintext garbage must NEVER be forwarded to the JS event loop"
            );
        }

        // The live server still serves a genuine TLS client afterwards.
        let client = client_cfg(&fixture("ca.pem"), None, None);
        let sock = TcpStream::connect(("127.0.0.1", port)).expect("second connect");
        sock.set_read_timeout(Some(Duration::from_secs(8))).expect("t2");
        let mut conn = rustls::ClientConnection::new(client, sni()).expect("client conn");
        conn.complete_io(&mut sock.try_clone().expect("second handshake clone"))
            .expect("second handshake");
        let mut stream = rustls::StreamOwned::new(conn, sock);
        stream
            .write_all(b"GET /after HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .expect("req2");
        stream.flush().expect("flush2");
        let ev = rx.recv_timeout(Duration::from_secs(8)).expect("event after attack");
        assert!(ev.encrypted, "surviving connection must be encrypted");
        ev.reply
            .send(ResponseAction::Buffered(ReplyFrame {
                status: 200,
                reason: Some("OK".to_string()),
                headers: vec![],
                body: b"alive".to_vec(),
                keep_alive: false,
            }))
            .expect("reply2");
        let mut resp = Vec::new();
        let mut tmp = [0u8; 512];
        loop {
            match stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    resp.extend_from_slice(&tmp[..n]);
                    if resp.windows(5).any(|w| w == b"alive") {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => panic!("post-attack read: {e}"),
            }
        }
        drop(stream);
        stop_listener(&mut listener);
    }

    #[test]
    fn mtls_acceptor_rejects_missing_client_cert_and_accepts_valid_one() {
        let opts = TlsServerOptions {
            cert_pem: fixture("server-cert.pem"),
            key_pem: fixture("server-key.pem"),
            ca_pem: Some(fixture("ca.pem")),
            require_client_auth: true,
            alpn_protocols: Vec::new(),
        };
        let acceptor = make_tls_acceptor(&opts).expect("mTLS acceptor");
        let (tx, rx) = channel::<ConnectionEvent>();
        let mut listener =
            start_listener_with_acceptor("127.0.0.1", 0, 94, tx, Some(acceptor.clone()))
                .expect("bind mtls");
        let port = listener.port;

        // (1) No client certificate: rustls TLS1.3 lets the client's handshake complete locally
        // BEFORE the server's async denial alert arrives, so rejection is proven by its EFFECTS:
        // the connection must never deliver an application response and no event may reach JS.
        {
            let sock = TcpStream::connect(("127.0.0.1", port)).expect("connect no-cert");
            sock.set_read_timeout(Some(Duration::from_secs(6))).expect("t");
            let mut conn =
                rustls::ClientConnection::new(client_cfg(&fixture("ca.pem"), None, None), sni())
                    .expect("plain-client cfg");
            let handshook = conn.complete_io(&mut sock.try_clone().expect("probe clone"));
            let mut rejected = handshook.is_err();
            if !rejected {
                // Server-side `AnonymousClientPolicy::Deny`: try to use the session anyway.
                let mut stream = rustls::StreamOwned::new(conn, sock);
                stream
                    .write_all(b"GET /should-not-work HTTP/1.1\r\nHost: localhost\r\n\r\n")
                    .map_err(|e| format!("no-cert write: {e}"))
                    .ok();
                stream.flush().ok();
                let mut observed = Vec::new();
                let mut tmp = [0u8; 256];
                loop {
                    match stream.read(&mut tmp) {
                        Ok(0) => {
                            rejected = true; // EOF without any response: denied
                            break;
                        }
                        Ok(n) => {
                            observed.extend_from_slice(&tmp[..n]);
                            if observed.windows(8).any(|w| w == b"mtls-ok")
                                || observed.windows(8).any(|w| w == b"HTTP/1.1")
                            {
                                panic!(
                                    "mTLS server must NEVER serve a certificate-less client: {:?}",
                                    String::from_utf8_lossy(&observed)
                                );
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => {
                            rejected = true; // alert/reset from the denying server
                            break;
                        }
                    }
                }
            }
            assert!(rejected, "mTLS MUST reject a client without a certificate");
            assert!(
                rx.recv_timeout(Duration::from_millis(400)).is_err(),
                "a rejected mTLS client must never reach the JS event loop"
            );
        }

        // (2) Valid client cert: full engine roundtrip works and reports encrypted.
        let certified = client_cfg(
            &fixture("ca.pem"),
            Some(fixture("client-cert.pem")),
            Some(fixture("client-key.pem")),
        );
        let sock = TcpStream::connect(("127.0.0.1", port)).expect("connect cert");
        sock.set_read_timeout(Some(Duration::from_secs(8))).expect("t2");
        let mut conn = rustls::ClientConnection::new(certified, sni()).expect("cert client");
        conn.complete_io(&mut sock.try_clone().expect("mtls handshake clone"))
            .expect("mTLS handshake with valid client cert");
        let mut stream = rustls::StreamOwned::new(conn, sock);
        stream
            .write_all(b"GET /mtls HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .expect("req");
        stream.flush().expect("flush");
        let ev = rx.recv_timeout(Duration::from_secs(8)).expect("mTLS event");
        assert!(ev.encrypted && ev.request.head.target == "/mtls");
        ev.reply
            .send(ResponseAction::Buffered(ReplyFrame {
                status: 200,
                reason: Some("OK".to_string()),
                headers: vec![],
                body: b"mtls-ok".to_vec(),
                keep_alive: false,
            }))
            .expect("reply");
        let mut resp = Vec::new();
        let mut tmp = [0u8; 512];
        loop {
            match stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    resp.extend_from_slice(&tmp[..n]);
                    if resp.windows(7).any(|w| w == b"mtls-ok") {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => panic!("mTLS read: {e}"),
            }
        }
        drop(stream);
        stop_listener(&mut listener);
    }
}
