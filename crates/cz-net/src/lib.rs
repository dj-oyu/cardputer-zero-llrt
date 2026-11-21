//! `cz-net` — shared TLS foundation and synchronous outbound HTTP engine for Cardputer Zero LLRT.
//!
//! Phase 8A scope: [`tls`] (rustls + `ring`, lazy system CA roots via `rustls-native-certs`,
//! PEM parsing, per-request `TlsClientOptions`) and [`fetch`] (ureq v3 worker threads with a thin
//! custom `Transport` that holds the `TcpStream` so `AbortController.abort()` interrupts idle SSE
//! streams immediately). Phases 8B (`node:https` server TLS) and 8C (`node:tls` raw sockets) reuse
//! [`tls`] without pulling any TLS C library into `cz-hal`, which stays strictly `libc`-only.
//!
//! Phase 11 adds [`url`] (zero-allocation WHATWG-style URL parsing into ONE owned buffer plus
//! `Copy` byte spans) and [`ws`] (an RFC 6455 WebSocket client for `ws://` and `wss://` written
//! directly on top of [`tls`] + `ring`, with stack frame headers, mandatory client masking and
//! automatic `Pong` replies) — no external WebSocket crate anywhere.
//!
//! Forbidden-by-spec dependencies are absent: no `tokio`, `openssl`/`native-tls`, `aws-lc-rs`,
//! `webpki-roots`, `tungstenite`, or any other WebSocket crate.

pub mod fetch;
pub mod https;
pub mod tls;
pub mod tls_socket;
pub mod url;
pub mod ws;

pub use fetch::{
    active_fetch_count, cancel_fetch, fetch_has_pending, is_redirect_status, live_ids, next_event,
    parse_redirect_mode, release_slot, start_fetch, switch_buffered, switch_streaming, sync_agent,
    AbortHandle, EngineEvent, FailKind, FetchSpec, RedirectMode, CHUNK_QUEUE_DEPTH, CHUNK_SIZE,
    DEFAULT_CONNECT_TIMEOUT, MAX_BUFFERED_BODY, MAX_CONCURRENT_FETCHES, WORKER_STACK_BYTES,
};
pub use https::{make_tls_acceptor, TlsConnStream, HANDSHAKE_TIMEOUT};
pub use tls::{
    build_client_config, build_client_config_uncached, build_server_config, is_default_client_options,
    load_system_roots_uncached, parse_certificates, parse_private_key, status_text,
    system_root_store, TlsClientOptions, TlsServerOptions, NO_ROOTS_ERROR,
};
pub use tls_socket::{
    close_tls_server, connect_tls, create_tls_server, next_tls_event, tls_active_count,
    tls_destroy, tls_end, tls_has_pending, tls_write, TlsEngineEvent, SERVER_HANDSHAKE_TIMEOUT,
    TLS_WORKER_STACK_BYTES,
};
pub use url::{
    default_port, is_special, parse_url, parse_url_into, trim_url_chars, OwnedUrl, Parsed, Span,
    Url, FLAG_AUTHORITY, FLAG_OPAQUE, FLAG_SPECIAL,
};
pub use ws::{
    apply_mask_in_place, close_ws, encode_frame_header, next_ws_event, send_ws, terminate_ws,
    ws_active_count, ws_buffered_amount, ws_connect, ws_has_pending, WsEngineEvent, WS_GUID,
    WS_MAX_FRAME, WS_WORKER_STACK_BYTES,
};

#[cfg(test)]
mod tests {
    use super::fetch::*;
    use super::tls::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::{Duration, Instant};

    fn fixture(name: &str) -> Vec<u8> {
        // Resolve fixtures relative to the crate so workspace tests are self-contained.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata")
            .join(name);
        std::fs::read(&path).expect("fixture")
    }

    /// Start a one-shot HTTP responder thread that writes `bytes` then closes.
    fn spawn_http_once(bytes: Vec<u8>, hold: Option<Duration>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for res in listener.incoming().take(1) {
                let mut stream = match res {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let mut req = [0u8; 4096];
                let _ = stream.read(&mut req);
                if let Some(h) = hold {
                    std::thread::sleep(h);
                }
                let _ = stream.write_all(&bytes);
                let _ = stream.shutdown(std::net::Shutdown::Both);
                break;
            }
        });
        port
    }

    /// Drain any slots left behind by earlier engine tests (process-wide registry hygiene).
    fn clear_registry() {
        for id in live_ids() {
            cancel_fetch(id);
            release_slot(id);
        }
        let started = Instant::now();
        while fetch_has_pending() && started.elapsed() < Duration::from_millis(3000) {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Drain events from a slot until `pred` matches or the deadline expires.
    fn drain_until(id: u64, pred: impl Fn(&EngineEvent) -> bool, ms: u64) -> Option<EngineEvent> {
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(ms) {
            match next_event(id) {
                Some(ev) => {
                    if pred(&ev) {
                        return Some(ev);
                    }
                }
                None => std::thread::sleep(Duration::from_millis(2)),
            }
        }
        None
    }

    // ------------------------------------------------------------------ TLS
    #[test]
    fn tls_client_options_default_rejects_unauthorized() {
        let opts = TlsClientOptions::default();
        assert!(opts.reject_unauthorized);
        assert!(opts.ca_pem.is_none());
    }

    #[test]
    fn parse_certificates_accepts_pem_chain_and_rejects_garbage() {
        let ca = fixture("ca.pem");
        let certs = parse_certificates(&ca).expect("valid ca pem");
        assert_eq!(certs.len(), 1);
        let err = parse_certificates(b"not a pem at all").expect_err("garbage must error");
        assert!(err.contains("no valid PEM certificate"), "was: {err}");
    }

    #[test]
    fn parse_private_key_reads_sec1_ec_and_rejects_missing_key() {
        let key = parse_private_key(&fixture("server-key.pem")).expect("sec1 ec key");
        match key {
            rustls::pki_types::PrivateKeyDer::Sec1(_) => {}
            other => panic!("expected SEC1 key, got {other:?}"),
        }
        let err = parse_private_key(b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----")
            .expect_err("cert is not a key");
        assert!(err.contains("no valid PEM private key"), "was: {err}");
    }

    #[test]
    fn build_client_config_with_ca_pem_and_mtls_pair() {
        let opts = TlsClientOptions {
            ca_pem: Some(fixture("ca.pem")),
            cert_pem: Some(fixture("client-cert.pem")),
            key_pem: Some(fixture("client-key.pem")),
            reject_unauthorized: true,
            alpn_protocols: Vec::new(),
        };
        let cfg = build_client_config(&opts).expect("mtls client config");
        // mTLS is exercised end-to-end by mtls_server_requires_and_accepts_client_certificate.
        let _ = cfg;
    }

    #[test]
    fn build_client_config_rejects_invalid_ca_pem() {
        let opts = TlsClientOptions {
            ca_pem: Some(b"junk".to_vec()),
            ..Default::default()
        };
        let err = build_client_config(&opts).expect_err("invalid ca");
        assert!(err.contains("no valid PEM certificate"), "was: {err}");
    }

    /// CA env vars are *process-global* and the root store is a process-wide `OnceLock`, so tests
    /// that mutate `SSL_CERT_FILE` / `SSL_CERT_DIR` / `NODE_EXTRA_CA_CERTS` (or latch the cache)
    /// must never interleave — they run under this shared serial guard regardless of harness threads.
    fn ca_env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn system_roots_honor_ssl_cert_file_and_extra_certs_env() {
        let _lock = ca_env_lock();
        let dir = std::env::temp_dir().join(format!("cz-net-ca-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmpdir");
        let ca = dir.join("ca.pem");
        std::fs::write(&ca, fixture("ca.pem")).expect("write ca");
        let empty_dir = dir.join("empty");
        std::fs::create_dir_all(&empty_dir).expect("mk empty");

        std::env::set_var("SSL_CERT_FILE", &ca);
        std::env::remove_var("NODE_EXTRA_CA_CERTS");
        let store = load_system_roots_uncached().expect("roots from SSL_CERT_FILE");
        assert!(store.len() >= 1, "expected the fixture CA to load");

        // NODE_EXTRA_CA_CERTS adds the wrong CA on top of the system selection.
        std::env::set_var("NODE_EXTRA_CA_CERTS", dir.join("extra.pem"));
        std::fs::write(dir.join("extra.pem"), fixture("wrong-ca.pem")).expect("write extra");
        let with_extra = load_system_roots_uncached().expect("roots + extra");
        assert_eq!(with_extra.len(), store.len() + 1, "extra CA must be appended");

        // Nothing loadable -> the spec-mandated error string.
        std::env::set_var("SSL_CERT_FILE", dir.join("missing.pem"));
        std::env::set_var("SSL_CERT_DIR", &empty_dir);
        std::env::remove_var("NODE_EXTRA_CA_CERTS");
        let err = load_system_roots_uncached().expect_err("no roots expected");
        assert_eq!(err, NO_ROOTS_ERROR, "was: {err}");

        std::env::remove_var("SSL_CERT_FILE");
        std::env::remove_var("SSL_CERT_DIR");
        std::env::remove_var("NODE_EXTRA_CA_CERTS");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn system_root_store_is_memoized_per_process() {
        // Latching the cache while another test mutates CA env would make the first load depend on
        // scheduling; the guard keeps env mutation and cache latching mutually exclusive.
        let _lock = ca_env_lock();
        let a = system_root_store().expect("system roots present in sandbox");
        let b = system_root_store().expect("cached system roots");
        assert!(Arc::ptr_eq(&a, &b), "OnceLock must return the same Arc");
    }

    #[test]
    fn default_client_config_is_memoized_while_non_default_options_build_fresh() {
        // Phase 10C: standard HTTPS fetches share ONE Arc<ClientConfig> instead of rebuilding the
        // config + WebPkiServerVerifier per request; anything custom still builds a fresh config,
        // and `build_client_config_uncached` keeps the CA-env-sensitive path observable.
        let _lock = ca_env_lock();
        assert!(is_default_client_options(&TlsClientOptions::default()));
        let first = build_client_config(&TlsClientOptions::default());
        let second = build_client_config(&TlsClientOptions::default());
        match (&first, &second) {
            (Ok(a), Ok(b)) => assert!(Arc::ptr_eq(a, b), "default ClientConfig must be cached"),
            (Err(a), Err(b)) => assert_eq!(a, b, "both attempts report the same root-store failure"),
            _ => panic!("cached client config outcome must be deterministic per process"),
        }

        // The uncached helper always rebuilds (used by CA-env unit tests), so its Arc differs.
        let fresh = build_client_config_uncached(&TlsClientOptions::default()).expect("uncached default");
        if let Ok(cached) = &first {
            assert!(!Arc::ptr_eq(cached, &fresh), "uncached build must not reuse the cache");
        }

        // Non-default options are never served from the cache.
        let ca_opts = TlsClientOptions {
            ca_pem: Some(fixture("ca.pem")),
            ..Default::default()
        };
        assert!(!is_default_client_options(&ca_opts));
        let ca1 = build_client_config(&ca_opts).expect("explicit CA");
        let ca2 = build_client_config(&ca_opts).expect("explicit CA again");
        assert!(!Arc::ptr_eq(&ca1, &ca2), "explicit CA configs must be built fresh each call");
        let unchecked = build_client_config(&TlsClientOptions {
            reject_unauthorized: false,
            ..Default::default()
        })
        .expect("accept-any config");
        if let Ok(cached) = &first {
            assert!(!Arc::ptr_eq(cached, &unchecked), "reject_unauthorized:false is not the default");
        }
        let alpn = build_client_config(&TlsClientOptions {
            alpn_protocols: vec![b"h2".to_vec()],
            ..Default::default()
        })
        .expect("alpn config");
        assert_eq!(alpn.alpn_protocols.len(), 1);
    }

        /// Ground-truth TLS harness: real handshake + encrypted byte roundtrip over loopback with
    /// blocking sockets (`ConnectionCommon::complete_io` plus `rustls::StreamOwned`). Returns the
    /// echoed plaintext on success, or the client-side error text when verification rejects.
    fn tls_handshake(
        client_cfg: Arc<rustls::ClientConfig>,
        server_cfg: Arc<rustls::ServerConfig>,
    ) -> Result<Vec<u8>, String> {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();

        // Server thread (its own blocking socket so neither side deadlocks).
        let server_thread = std::thread::spawn(move || -> Result<(), String> {
            let (sock, _) = listener.accept().expect("accept");
            // Bound the handshake so a rejecting client cannot hang this thread forever.
            let _ = sock.set_read_timeout(Some(Duration::from_secs(8)));
            let _ = sock.set_write_timeout(Some(Duration::from_secs(8)));
            let mut conn = rustls::ServerConnection::new(server_cfg).expect("server conn");
            match conn.complete_io(&mut sock.try_clone().expect("srv clone")) {
                Ok(_) => {}
                Err(e) => return Err(format!("server handshake: {e}")),
            }
            // Echo whatever plaintext arrives over the encrypted session.
            let mut stream = rustls::StreamOwned::new(conn, sock.try_clone().expect("srv stream"));
            let mut buf = [0u8; 16];
            match stream.read(&mut buf) {
                Ok(n) if n > 0 => {
                    stream.write_all(&buf[..n]).map_err(|e| format!("echo write: {e}"))?;
                    stream.flush().map_err(|e| format!("echo flush: {e}"))?;
                    // Wait (bounded) for the client's orderly close_notify so we can close
                    // gracefully too; an abrupt client close is tolerated here.
                    let wait_started = Instant::now();
                    while wait_started.elapsed() < Duration::from_millis(700) {
                        let rustls::StreamOwned { conn, sock } = &mut stream;
                        match conn.read_tls(sock) {
                            Ok(0) => break,
                            Ok(_) => {
                                let _ = conn.process_new_packets();
                                break;
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => break,
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::sleep(Duration::from_millis(5));
                            }
                            Err(_) => break,
                        }
                    }
                    {
                        let rustls::StreamOwned { conn, sock } = &mut stream;
                        conn.send_close_notify();
                        let mut guard = 0;
                        while conn.wants_write() && guard < 100 {
                            guard += 1;
                            match conn.write_tls(sock) {
                                Ok(_) => {}
                                Err(_) => break,
                            }
                        }
                    }
                    drop(stream);
                    Ok(())
                }
                other => Err(format!("server read: {other:?}")),
            }
        });

        let sock = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let _ = sock.set_read_timeout(Some(Duration::from_secs(8)));
        let name = rustls::pki_types::ServerName::try_from("localhost".to_string()).expect("sni");
        let mut conn = rustls::ClientConnection::new(client_cfg, name).expect("client conn");
        let handshake = conn.complete_io(&mut sock.try_clone().expect("cli clone"));
        if let Err(e) = handshake {
            // Verification failure (wrong CA / missing client cert): real TLS refuses to
            // negotiate. Close our side so the server thread stops blocking, then join it.
            let _ = sock.shutdown(std::net::Shutdown::Both);
            let server_result = server_thread.join().expect("server thread");
            let _ = server_result;
            return Err(format!("{e}"));
        }
        let mut stream = rustls::StreamOwned::new(conn, sock);
        stream.write_all(b"cz-ping").map_err(|e| format!("client write: {e}"))?;
        stream.flush().map_err(|e| format!("client flush: {e}"))?;
        let mut echoed = [0u8; 16];
        let n = stream.read(&mut echoed).map_err(|e| format!("client read: {e}"))?;
        if n == 0 {
            return Err("empty echo".into());
        }
        // Orderly client shutdown first, THEN join the server (it echoes and waits for our
        // close_notify before closing its side).
        {
            let rustls::StreamOwned { conn, sock } = &mut stream;
            conn.send_close_notify();
            let mut guard = 0;
            while conn.wants_write() && guard < 100 {
                guard += 1;
                match conn.write_tls(sock) {
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        }
        drop(stream);
        let server_result = server_thread.join().expect("server thread");
        match server_result {
            Ok(()) => Ok(echoed[..n].to_vec()),
            Err(msg) => Err(format!("server side: {msg}")),
        }
    }

    #[test]
    fn tls_handshake_succeeds_with_correct_ca_and_echoes_plaintext() {
        let server = build_server_config(&TlsServerOptions {
            cert_pem: fixture("server-cert.pem"),
            key_pem: fixture("server-key.pem"),
            ca_pem: None,
            require_client_auth: false,
            alpn_protocols: Vec::new(),
        })
        .expect("server cfg");
        let client = build_client_config(&TlsClientOptions {
            ca_pem: Some(fixture("ca.pem")),
            ..Default::default()
        })
        .expect("client cfg");
        let echoed = tls_handshake(client, server).expect("correct CA must verify");
        assert_eq!(&echoed[..], b"cz-ping", "encrypted roundtrip must echo exactly");
    }

    /// Node-compatible `ca:` semantics: a self-signed leaf handed over as its own CA must verify
    /// (rustls' stock webpki verifier rejects that shape with `CaUsedAsEndEntity`), while an
    /// unrelated CA still fails closed.
    #[test]
    fn tls_self_signed_leaf_as_ca_is_accepted_and_unrelated_ca_rejects() {
        let server = build_server_config(&TlsServerOptions {
            cert_pem: fixture("self-cert.pem"),
            key_pem: fixture("self-key.pem"),
            ca_pem: None,
            require_client_auth: false,
            alpn_protocols: Vec::new(),
        })
        .expect("self signed server cfg");

        let tolerant = build_client_config(&TlsClientOptions {
            ca_pem: Some(fixture("self-cert.pem")),
            ..Default::default()
        })
        .expect("tolerant client cfg");
        let echoed = tls_handshake(tolerant, server.clone()).expect("self-signed leaf as CA must verify");
        assert_eq!(&echoed[..], b"cz-ping", "encrypted roundtrip must echo exactly");

        let unrelated = build_client_config(&TlsClientOptions {
            ca_pem: Some(fixture("ca.pem")),
            ..Default::default()
        })
        .expect("unrelated ca cfg");
        let err = tls_handshake(unrelated, server).expect_err("unrelated CA MUST reject");
        assert!(
            err.contains("certificate") || err.contains("Issuer") || err.contains("invalid"),
            "rejection text was: {err}"
        );
    }

    #[test]
    fn tls_handshake_rejects_wrong_ca_but_passes_when_unverified() {
        let server = build_server_config(&TlsServerOptions {
            cert_pem: fixture("server-cert.pem"),
            key_pem: fixture("server-key.pem"),
            ca_pem: None,
            require_client_auth: false,
            alpn_protocols: Vec::new(),
        })
        .expect("server cfg");

        let wrong = build_client_config(&TlsClientOptions {
            ca_pem: Some(fixture("wrong-ca.pem")),
            ..Default::default()
        })
        .expect("wrong ca cfg");
        let err = tls_handshake(wrong, server.clone()).expect_err("wrong CA MUST reject");
        assert!(
            err.contains("certificate") || err.contains("alert") || err.contains("Issuer")
                || err.contains("Signature") || err.contains("invalid"),
            "rejection text was: {err}"
        );

        // reject_unauthorized: false still negotiates real TLS (encryption intact).
        let unchecked = build_client_config(&TlsClientOptions {
            ca_pem: Some(fixture("wrong-ca.pem")),
            reject_unauthorized: false,
            alpn_protocols: Vec::new(),
            ..Default::default()
        })
        .expect("danger cfg builds");
        let echoed = tls_handshake(unchecked, server).expect("unverified mode negotiates");
        assert_eq!(&echoed[..], b"cz-ping");
    }

    #[test]
    fn mtls_server_requires_and_accepts_client_certificate() {
        let server = build_server_config(&TlsServerOptions {
            cert_pem: fixture("server-cert.pem"),
            key_pem: fixture("server-key.pem"),
            ca_pem: Some(fixture("ca.pem")),
            require_client_auth: true,
            alpn_protocols: Vec::new(),
        })
        .expect("mtls server cfg");

        let with_client = build_client_config(&TlsClientOptions {
            ca_pem: Some(fixture("ca.pem")),
            cert_pem: Some(fixture("client-cert.pem")),
            key_pem: Some(fixture("client-key.pem")),
            reject_unauthorized: true,
            alpn_protocols: Vec::new(),
        })
        .expect("mtls client");
        let echoed = tls_handshake(with_client, server.clone())
            .expect("mTLS handshake with a valid client cert");
        assert_eq!(&echoed[..], b"cz-ping");

        let without_client = build_client_config(&TlsClientOptions {
            ca_pem: Some(fixture("ca.pem")),
            ..Default::default()
        })
        .expect("client without cert");
        tls_handshake(without_client, server).expect_err("server requiring client auth must reject");
    }

    #[test]
    fn build_server_config_rejects_bad_key() {
        let err = build_server_config(&TlsServerOptions {
            cert_pem: fixture("server-cert.pem"),
            key_pem: b"nope".to_vec(),
            ca_pem: None,
            require_client_auth: false,
            alpn_protocols: Vec::new(),
        })
        .expect_err("bad key rejected");
        assert!(err.contains("private key") || err.contains("PEM"), "was: {err}");
    }

    #[test]
    fn redirect_mode_parsing_and_status_table() {
        assert_eq!(parse_redirect_mode("manual"), RedirectMode::Manual);
        assert_eq!(parse_redirect_mode("ERROR"), RedirectMode::Error);
        assert_eq!(parse_redirect_mode("follow"), RedirectMode::Follow);
        assert_eq!(parse_redirect_mode(""), RedirectMode::Follow);
        assert!(is_redirect_status(302) && is_redirect_status(308));
        assert!(!is_redirect_status(200) && !is_redirect_status(404));
        assert_eq!(status_text(404), "Not Found");
        assert_eq!(status_text(200), "OK");
    }

    #[test]
    fn spec_constants_match_the_phase8a_contract() {
        assert_eq!(CHUNK_SIZE, 16 * 1024);
        assert_eq!(CHUNK_QUEUE_DEPTH, 8);
        assert_eq!(MAX_BUFFERED_BODY, 16 * 1024 * 1024);
        assert_eq!(MAX_CONCURRENT_FETCHES, 16);
        assert_eq!(WORKER_STACK_BYTES, 256 * 1024);
    }

    #[test]
    fn fetch_delivers_headers_before_body_and_buffers_result() {
        let _lock = registry_test_lock();
        clear_registry();
        let body = b"hello-cz-net";
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nX-Custom: CardputerZero\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        let mut full = resp;
        full.extend_from_slice(body);
        let port = spawn_http_once(full, None);

        let id = start_fetch(FetchSpec {
            url: format!("http://127.0.0.1:{port}/get"),
            method: "GET".to_string(),
            headers: Vec::new(),
            body: None,
            redirect: RedirectMode::Follow,
            tls: TlsClientOptions::default(),
        })
        .expect("start");

        let headers = drain_until(id, |e| matches!(e, EngineEvent::Headers { .. }), 3000)
            .expect("headers event");
        match headers {
            EngineEvent::Headers { status, status_text, headers: hs, .. } => {
                assert_eq!(status, 200);
                assert_eq!(status_text, "OK");
                assert!(hs.iter().any(|(k, v)| k == "x-custom" && v == "CardputerZero"));
            }
            _ => unreachable!(),
        }
        let body_ev = drain_until(id, |e| matches!(e, EngineEvent::Body(_)), 3000)
            .expect("buffered body event");
        match body_ev {
            EngineEvent::Body(b) => {
                assert_eq!(&b[..], b"hello-cz-net");
            }
            _ => unreachable!(),
        }
        assert!(release_slot(id));
    }

    #[test]
    fn fetch_resolves_404_without_failing() {
        clear_registry();
        let resp = b"HTTP/1.1 404 Not Found\r\nContent-Length: 3\r\nConnection: close\r\n\r\nno!";
        let port = spawn_http_once(resp.to_vec(), None);
        let id = start_fetch(FetchSpec {
            url: format!("http://127.0.0.1:{port}/missing"),
            method: "GET".to_string(),
            headers: Vec::new(),
            body: None,
            redirect: RedirectMode::Follow,
            tls: TlsClientOptions::default(),
        })
        .expect("start");
        let h = drain_until(id, |e| matches!(e, EngineEvent::Headers { .. }), 3000).expect("headers");
        match h {
            EngineEvent::Headers { status, .. } => assert_eq!(status, 404),
            _ => unreachable!(),
        }
        // 4xx must never surface as Failed.
        let failed = drain_until(id, |e| matches!(e, EngineEvent::Failed { .. }), 300);
        assert!(failed.is_none(), "404 must resolve, not fail");
        release_slot(id);
    }

    #[test]
    fn streaming_mode_replays_prefix_then_emits_chunks_and_done() {
        let _lock = registry_test_lock();
        clear_registry();
        let body = b"partA-";
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
        );
        let framed = format!("{head}\x00").replace("\x00", "");
        let mut bytes = framed.into_bytes();
        bytes.extend_from_slice(format!("{:x}\r\n", body.len()).as_bytes());
        bytes.extend_from_slice(body);
        bytes.extend_from_slice(b"\r\n0\r\n\r\n");
        let port = spawn_http_once(bytes, None);

        let id = start_fetch(FetchSpec {
            url: format!("http://127.0.0.1:{port}/chunked"),
            method: "GET".to_string(),
            headers: Vec::new(),
            body: None,
            redirect: RedirectMode::Follow,
            tls: TlsClientOptions::default(),
        })
        .expect("start");
        drain_until(id, |e| matches!(e, EngineEvent::Headers { .. }), 3000).expect("headers");
        assert!(switch_streaming(id), "switch to streaming accepted");

        let mut combined = Vec::new();
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(3000) {
            match next_event(id) {
                Some(EngineEvent::Chunk(c)) => combined.extend_from_slice(&c),
                Some(EngineEvent::Done) => break,
                Some(EngineEvent::Failed { message, .. }) => panic!("stream failed: {message}"),
                _ => std::thread::sleep(Duration::from_millis(2)),
            }
        }
        assert_eq!(&combined[..], b"partA-");
        release_slot(id);
    }

    #[test]
    fn cancel_fetch_aborts_a_hanging_stream_quickly() {
        let _lock = registry_test_lock();
        clear_registry();
        // Server sends headers then never the body; the read blocks until the socket is shut down.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let holder = std::thread::spawn(move || {
            for res in listener.incoming().take(1) {
                let mut stream = match res {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let mut req = [0u8; 4096];
                let _ = stream.read(&mut req);
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\ndata: first\n\n",
                );
                // Idle SSE: hold briefly (bounded) so the test's join cannot hang the suite.
                std::thread::sleep(Duration::from_secs(2));
                let _ = stream.shutdown(std::net::Shutdown::Both);
                break;
            }
        });

        let id = start_fetch(FetchSpec {
            url: format!("http://127.0.0.1:{port}/sse"),
            method: "GET".to_string(),
            headers: Vec::new(),
            body: None,
            redirect: RedirectMode::Follow,
            tls: TlsClientOptions::default(),
        })
        .expect("start");
        drain_until(id, |e| matches!(e, EngineEvent::Headers { .. }), 3000).expect("headers");
        assert!(switch_streaming(id));
        let first = drain_until(id, |e| matches!(e, EngineEvent::Chunk(_)), 3000).expect("first chunk");
        match first {
            EngineEvent::Chunk(c) => assert_eq!(&c[..], b"data: first\n\n"),
            _ => unreachable!(),
        }
        let started = Instant::now();
        assert!(cancel_fetch(id), "cancel live slot");
        let fail = drain_until(id, |e| matches!(e, EngineEvent::Failed { kind, .. } if *kind == FailKind::Abort), 2500)
            .expect("abort failure within ms");
        match fail {
            EngineEvent::Failed { kind, message } => {
                assert_eq!(kind, FailKind::Abort);
                assert!(message.contains("aborted"), "was: {message}");
            }
            other => panic!("expected Failed(Abort), got {other:?}"),
        }
        assert!(started.elapsed() < Duration::from_millis(1500), "abort took {:?}", started.elapsed());
        release_slot(id);
        holder.join().expect("sse holder thread");
    }

    #[test]
    fn buffered_body_cap_rejects_oversize_response() {
        let _lock = registry_test_lock();
        clear_registry();
        // 17 MiB of 64 KiB blocks > MAX_BUFFERED_BODY (16 MiB).
        let block = vec![0x41u8; 64 * 1024];
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let sender = std::thread::spawn(move || {
            for res in listener.incoming().take(1) {
                let mut stream = match res {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let mut req = [0u8; 4096];
                let _ = stream.read(&mut req);
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                );
                for _ in 0..(17 * 16) {
                    let hdr = format!("{:x}\r\n", block.len());
                    if stream.write_all(hdr.as_bytes()).is_err() || stream.write_all(&block).is_err() || stream.write_all(b"\r\n").is_err() {
                        break;
                    }
                }
                let _ = stream.write_all(b"0\r\n\r\n");
                let _ = stream.shutdown(std::net::Shutdown::Both);
                break;
            }
        });

        let id = start_fetch(FetchSpec {
            url: format!("http://127.0.0.1:{port}/oversize"),
            method: "GET".to_string(),
            headers: Vec::new(),
            body: None,
            redirect: RedirectMode::Follow,
            tls: TlsClientOptions::default(),
        })
        .expect("start");
        drain_until(id, |e| matches!(e, EngineEvent::Headers { .. }), 5000).expect("headers");
        let fail = drain_until(
            id,
            |e| matches!(e, EngineEvent::Failed { kind, .. } if *kind == FailKind::BodyTooLarge),
            12000,
        )
        .expect(">16MiB must reject with BodyTooLarge");
        match fail {
            EngineEvent::Failed { message, .. } => assert!(message.contains("MiB"), "was: {message}"),
            _ => unreachable!(),
        }
        release_slot(id);
        sender.join().expect("oversize sender thread");
    }

    #[test]
    fn redirects_follow_manual_and_error_modes() {
        let _lock = registry_test_lock();
        clear_registry();
        // Two-hop redirect chain served on one port: /redirect-1 -> /redirect-2.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let router = std::thread::spawn(move || {
            // Exactly four connections are expected: follow (302 + landed), manual, error mode.
            for res in listener.incoming().take(4) {
                let mut stream = match res {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let mut req = [0u8; 4096];
                let n = stream.read(&mut req).expect("read request");
                let text = String::from_utf8_lossy(&req[..n]).into_owned();
                if text.starts_with("GET /redirect-1") {
                    let _ = stream.write_all(b"HTTP/1.1 302 Found\r\nLocation: /redirect-2\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                } else {
                    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 15\r\nConnection: close\r\n\r\nredirect-landedX");
                }
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        });

        // follow: lands on /redirect-2 and reports redirected=true
        let id = start_fetch(FetchSpec {
            url: format!("http://127.0.0.1:{port}/redirect-1"),
            method: "GET".to_string(),
            headers: Vec::new(),
            body: None,
            redirect: RedirectMode::Follow,
            tls: TlsClientOptions::default(),
        })
        .expect("follow");
        let h = drain_until(id, |e| matches!(e, EngineEvent::Headers { .. }), 3000).expect("headers");
        match h {
            EngineEvent::Headers { status, url, redirected, .. } => {
                assert_eq!(status, 200);
                assert!(redirected, "must report redirected");
                assert!(url.ends_with("/redirect-2"), "final url: {url}");
            }
            _ => unreachable!(),
        }
        release_slot(id);

        // manual: returns the raw 302 with redirected=false
        let id = start_fetch(FetchSpec {
            url: format!("http://127.0.0.1:{port}/redirect-1"),
            method: "GET".to_string(),
            headers: Vec::new(),
            body: None,
            redirect: RedirectMode::Manual,
            tls: TlsClientOptions::default(),
        })
        .expect("manual");
        let h = drain_until(id, |e| matches!(e, EngineEvent::Headers { .. }), 3000).expect("headers");
        match h {
            EngineEvent::Headers { status, redirected, .. } => {
                assert_eq!(status, 302);
                assert!(!redirected, "manual must not claim redirection");
            }
            _ => unreachable!(),
        }
        release_slot(id);

        // error: rejects with a Redirect failure event
        let id = start_fetch(FetchSpec {
            url: format!("http://127.0.0.1:{port}/redirect-1"),
            method: "GET".to_string(),
            headers: Vec::new(),
            body: None,
            redirect: RedirectMode::Error,
            tls: TlsClientOptions::default(),
        })
        .expect("error mode");
        let f = drain_until(id, |e| matches!(e, EngineEvent::Failed { kind, .. } if *kind == FailKind::Redirect), 3000)
            .expect("redirect error mode rejects");
        match f {
            EngineEvent::Failed { message, .. } => assert!(message.contains("redirect"), "was: {message}"),
            _ => unreachable!(),
        }
        release_slot(id);
        router.join().expect("redirect router thread");
    }

    #[test]
    fn concurrency_cap_rejects_the_17th_inflight_fetch() {
        let _lock = registry_test_lock();
        clear_registry();
        // A listener that accepts connections but never responds -> workers stay in-flight.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        let sink = std::thread::spawn(move || {
            // Non-blocking accept polling so shutdown does not hang on a blocking accept().
            listener.set_nonblocking(true).expect("nonblocking sink");
            while !stop2.load(std::sync::atomic::Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // Hold the socket open so the worker blocks awaiting response headers.
                        std::thread::spawn(move || {
                            let mut buf = [0u8; 64];
                            let _ = stream.read(&mut buf);
                            std::thread::sleep(Duration::from_secs(3));
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        let mut ids = Vec::new();
        for _ in 0..MAX_CONCURRENT_FETCHES {
            let id = start_fetch(FetchSpec {
                url: format!("http://127.0.0.1:{port}/hang"),
                method: "GET".to_string(),
                headers: Vec::new(),
                body: None,
                redirect: RedirectMode::Follow,
                tls: TlsClientOptions::default(),
            })
            .expect("first 16 allowed");
            ids.push(id);
        }
        assert!(fetch_has_pending(), "in-flight fetches report pending");
        let rejected = start_fetch(FetchSpec {
            url: format!("http://127.0.0.1:{port}/hang"),
            method: "GET".to_string(),
            headers: Vec::new(),
            body: None,
            redirect: RedirectMode::Follow,
            tls: TlsClientOptions::default(),
        });
        let err = rejected.expect_err("17th must be rejected");
        assert!(err.contains("resource limit") || err.contains("too many"), "was: {err}");

        for id in &ids {
            cancel_fetch(*id);
        }
        let cleanup_started = Instant::now();
        let mut arrived = 0usize;
        for id in &ids {
            if drain_until(*id, |e| matches!(e, EngineEvent::Failed { .. }), 4000).is_some() {
                arrived += 1;
            }
            release_slot(*id);
        }
        // Every cancelled worker must report an Abort failure promptly and leave the registry.
        assert_eq!(arrived, ids.len(), "all cancelled workers reported failures");
        assert!(
            cleanup_started.elapsed() < Duration::from_millis(3000),
            "cancelled workers must clear quickly"
        );
        assert_eq!(active_fetch_count(), 0, "released slots leave the registry");
        let clear_started = Instant::now();
        while fetch_has_pending() && clear_started.elapsed() < Duration::from_millis(6000) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!fetch_has_pending(), "inflight workers must exit after cancel");
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        sink.join().expect("sink thread");
    }


    #[test]
    fn post_body_is_sent_to_the_server() {
        let _lock = registry_test_lock();
        clear_registry();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let echo = std::thread::spawn(move || {
            for res in listener.incoming().take(1) {
                let mut stream = match res {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let mut acc = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    let n = match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(_) => break,
                    };
                    acc.extend_from_slice(&buf[..n]);
                    // Stop once the header block plus the declared Content-Length body have arrived.
                    let text = String::from_utf8_lossy(&acc).into_owned();
                    if let Some(head_end) = text.find("\r\n\r\n") {
                        let declared: usize = text[..head_end]
                            .lines()
                            .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                            .and_then(|l| l.split(':').nth(1))
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                        if acc.len() >= head_end + 4 + declared {
                            break;
                        }
                    }
                }
                let text = String::from_utf8_lossy(&acc).into_owned();
                let head_end = text.find("\r\n\r\n").map(|p| p + 4).unwrap_or(text.len());
                let payload = &text[head_end..];
                let json = format!("{{\"echo\":\"{payload}\"}}");
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{json}",
                    json.len()
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        });

        let id = start_fetch(FetchSpec {
            url: format!("http://127.0.0.1:{port}/echo"),
            method: "POST".to_string(),
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: Some(b"{\"k\":1}".to_vec()),
            redirect: RedirectMode::Follow,
            tls: TlsClientOptions::default(),
        })
        .expect("start");
        drain_until(id, |e| matches!(e, EngineEvent::Headers { .. }), 3000).expect("headers");
        let b = drain_until(id, |e| matches!(e, EngineEvent::Body(_)), 3000).expect("body");
        match b {
            EngineEvent::Body(bytes) => {
                let txt = String::from_utf8_lossy(&bytes).into_owned();
                assert!(txt.contains("{\"k\":1}"), "server echoed: {txt}");
            }
            _ => unreachable!(),
        }
        release_slot(id);
        echo.join().expect("echo thread");
    }
}
