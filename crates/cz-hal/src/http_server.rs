//! Multi-client TCP HTTP/1.1 engine built on [`crate::http`].
//!
//! Threading model (chosen for a 512MB single-core CardputerZero):
//!
//! - One **accept thread** per listening socket: non-blocking `accept()` parked in `poll()` so a
//!   stop flag is observed within milliseconds and CPU stays idle while nothing arrives.
//! - One **reader thread per TCP connection**: parses requests incrementally, enforces the body
//!   cap, answers protocol errors itself (`400`/`413`) and forwards *complete* requests to the JS
//!   event loop over an `mpsc` channel, carrying a per-request reply channel back so the response
//!   can be flushed onto the socket without any cross-thread JS object sharing.
//! - Connections stay sequential per socket (standard HTTP/1.1 keep-alive ordering), while
//!   different sockets are served concurrently — 8+ simultaneous clients all get answered.
//!
//! No panics: every socket error is handled by closing that connection only.

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::http::{
    error_response, serialize_response, ParsedRequest, RequestParser, ResponseFrame,
};

/// Read size used while pulling bytes off a connection.
const READ_CHUNK: usize = 8 * 1024;
/// Poll slice used by the accept loop (bounded stop latency, low idle CPU).
const ACCEPT_POLL_MS: i32 = 20;
/// Wall-clock budget for JavaScript to answer one request; a hung handler is answered `500` and
/// the socket is closed so clients never stall indefinitely.
pub const DEFAULT_REPLY_TIMEOUT: Duration = Duration::from_secs(30);

static NEXT_CONN_ID: AtomicU64 = AtomicU64::new(1);
static ACTIVE_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);
static EVENTS_QUEUED: AtomicUsize = AtomicUsize::new(0);

/// Count of live connection reader threads (for diagnostics/tests).
pub fn active_connections() -> usize {
    ACTIVE_CONNECTIONS.load(Ordering::SeqCst)
}

/// Total requests enqueued toward the JS event loop since process start. Incremented *before*
/// the send so a consumer can detect queued-but-not-yet-drained work without peeking a channel.
pub fn events_queued() -> usize {
    EVENTS_QUEUED.load(Ordering::SeqCst)
}

/// A fully parsed request handed from a connection thread to the JS event loop.
#[derive(Debug)]
pub struct ConnectionEvent {
    pub server_id: u64,
    pub conn_id: u64,
    pub request: ParsedRequest,
    /// Whether the socket may stay open after the response is written.
    pub keep_alive: bool,
    pub remote_address: String,
    pub remote_port: u16,
    pub local_address: String,
    pub local_port: u16,
    /// Reply channel for THIS request (`mpsc` acts as the oneshot: exactly one frame expected).
    pub reply: Sender<ReplyFrame>,
}

/// The response frame the JS side wants written onto the connection.
#[derive(Debug, Clone)]
pub struct ReplyFrame {
    pub status: u16,
    pub reason: Option<String>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub keep_alive: bool,
}

impl ReplyFrame {
    pub fn new(status: u16, body: Vec<u8>, keep_alive: bool) -> Self {
        Self {
            status,
            reason: None,
            headers: Vec::new(),
            body,
            keep_alive,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        serialize_response(self.status, self.reason.as_deref(), &self.headers, &self.body, self.keep_alive)
    }
}

/// A listening socket plus its parked accept thread.
pub struct Listener {
    pub id: u64,
    pub address: String,
    pub port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Listener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Listener")
            .field("id", &self.id)
            .field("address", &self.address)
            .field("port", &self.port)
            .field("stopped", &self.stop.load(Ordering::SeqCst))
            .finish()
    }
}

/// Bind a TCP listener (with `SO_REUSEADDR`), report the *real* bound address/port (`port: 0`
/// yields an OS-assigned ephemeral port) and park an accept thread forwarding events on `tx`.
pub fn spawn_listener(host: &str, port: u16, server_id: u64, tx: Sender<ConnectionEvent>) -> std::io::Result<Listener> {
    spawn_listener_with_timeout(host, port, server_id, tx, DEFAULT_REPLY_TIMEOUT)
}

/// Same as [`spawn_listener`] but with an explicit per-request JS reply timeout.
pub fn spawn_listener_with_timeout(
    host: &str,
    port: u16,
    server_id: u64,
    tx: Sender<ConnectionEvent>,
    reply_timeout: Duration,
) -> std::io::Result<Listener> {
    let listener = bind_socket(host, port)?;
    let bound = listener.local_addr()?;
    listener.set_nonblocking(true)?;
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let handle = std::thread::Builder::new()
        .name(format!("cz-http-accept-{server_id}"))
        .spawn(move || accept_loop(listener, stop_thread, tx, server_id, reply_timeout))?;
    Ok(Listener {
        id: server_id,
        address: bound.ip().to_string(),
        port: bound.port(),
        stop,
        handle: Some(handle),
    })
}

/// Bind helper split out so tests can bind without spawning a thread.
pub fn bind_socket(host: &str, port: u16) -> std::io::Result<TcpListener> {
    // Normalize bare hosts / localhost-ish aliases Node users pass to listen().
    let normalized = match host.trim() {
        "" | "*" | "any" => "0.0.0.0".to_string(),
        other => other.to_string(),
    };
    let listener = TcpListener::bind((normalized.as_str(), port))?;
    // SO_REUSEADDR avoids TIME_WAIT bind failures when tests restart servers on a fixed port.
    set_reuseaddr(&listener);
    Ok(listener)
}

fn set_reuseaddr(listener: &TcpListener) {
    use std::ffi::c_int;
    let opt: c_int = 1;
    unsafe {
        libc::setsockopt(
            listener.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            &opt as *const c_int as *const libc::c_void,
            std::mem::size_of::<c_int>() as libc::socklen_t,
        )
    };
}

/// Stop accepting new connections: flag the thread, join it (bounded by one poll slice) so the
/// bound socket is fully released and returned so its ephemeral port becomes free again.
pub fn stop_listener(listener: &mut Listener) -> bool {
    listener.stop.store(true, Ordering::SeqCst);
    match listener.handle.take() {
        Some(h) => h.join().is_ok(),
        None => false,
    }
}

fn accept_loop(
    listener: TcpListener,
    stop: Arc<AtomicBool>,
    tx: Sender<ConnectionEvent>,
    server_id: u64,
    reply_timeout: Duration,
) {
    while !stop.load(Ordering::SeqCst) {
        let mut pfd = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, ACCEPT_POLL_MS) };
        if rc < 0 {
            // EINTR-ish: keep polling unless we were asked to stop.
            continue;
        }
        loop {
            match listener.accept() {
                Ok((stream, peer)) => {
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    let conn_id = NEXT_CONN_ID.fetch_add(1, Ordering::SeqCst);
                    spawn_connection(stream, peer, server_id, conn_id, tx.clone(), reply_timeout);
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => break,
                Err(_) => return,
            }
        }
    }
}

/// Detached per-connection reader thread: parse -> (error answer | forward + await reply).
fn spawn_connection(
    mut stream: TcpStream,
    peer: std::net::SocketAddr,
    server_id: u64,
    conn_id: u64,
    tx: Sender<ConnectionEvent>,
    reply_timeout: Duration,
) {
    ACTIVE_CONNECTIONS.fetch_add(1, Ordering::SeqCst);
    stream.set_nodelay(true).ok();
    let local = stream.local_addr().unwrap_or(peer);
    std::thread::Builder::new()
        .name(format!("cz-http-conn-{conn_id}"))
        // Intentionally detached: `server.close()` may leave clients mid-flight, and the process
        // exits once every server closed; stray threads must not block shutdown.
        .spawn(move || {
            // Decrement ACTIVE_CONNECTIONS when this thread ends (also on early `return`s).
            let _guard = ConnectionGuard;
            let remote_ip = peer.ip().to_string();
            let local_ip = local.ip().to_string();
            let mut parser = RequestParser::new();
            let mut buf = [0u8; READ_CHUNK];
            loop {
                // Drive the parser until one complete request (or a protocol failure) is reached.
                let req: ParsedRequest = loop {
                    match parser.step() {
                        Ok(crate::http::ParseStep::Request(r)) => break r,
                        Ok(crate::http::ParseStep::InterimContinue) => {
                            if write_all_eof_free(&mut stream, crate::http::continue_frame()).is_err() {
                                return;
                            }
                        }
                        Ok(crate::http::ParseStep::NeedMore) => {
                            match stream.read(&mut buf) {
                                Ok(0) => return, // client closed the connection
                                Ok(n) => parser.push(&buf[..n]),
                                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                                Err(_) => return,
                            }
                        }
                        Err(err) => {
                            // Malformed (400) / oversized (413): answer, then drop only THIS socket.
                            let _ = write_all_eof_free(&mut stream, error_response(&err));
                            shutdown(&mut stream);
                            return;
                        }
                    }
                };

                let keep_alive = req.keep_alive;
                let (reply_tx, reply_rx) = channel::<ReplyFrame>();
                EVENTS_QUEUED.fetch_add(1, Ordering::SeqCst);
                if tx
                    .send(ConnectionEvent {
                        server_id,
                        conn_id,
                        request: req,
                        keep_alive,
                        remote_address: remote_ip.clone(),
                        remote_port: peer.port(),
                        local_address: local_ip.clone(),
                        local_port: local.port(),
                        reply: reply_tx,
                    })
                    .is_err()
                {
                    // Consumer gone (event loop exited): close quietly.
                    shutdown(&mut stream);
                    return;
                }

                match reply_rx.recv_timeout(reply_timeout) {
                    Ok(frame) => {
                        let bytes = frame.encode();
                        if write_all_eof_free(&mut stream, bytes).is_err() {
                            shutdown(&mut stream);
                            return;
                        }
                        if !frame.keep_alive {
                            shutdown(&mut stream);
                            return;
                        }
                    }
                    Err(_) => {
                        // Handler timed out OR dropped the response object without `res.end()`:
                        // answer 500 (never stall the client) and close only this socket.
                        let body = ResponseFrame::error(500, "request handler did not respond", false);
                        let _ = write_all_eof_free(&mut stream, body.encode());
                        shutdown(&mut stream);
                        return;
                    }
                }
            }
        })
        .ok(); // thread creation failure merely drops (closes) the socket
}

struct ConnectionGuard;

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        ACTIVE_CONNECTIONS.fetch_sub(1, Ordering::SeqCst);
    }
}

fn write_all_eof_free(stream: &mut TcpStream, mut bytes: Vec<u8>) -> std::io::Result<()> {
    while !bytes.is_empty() {
        match stream.write(&bytes) {
            Ok(0) => return Err(std::io::Error::new(ErrorKind::BrokenPipe, "socket closed")),
            Ok(n) => {
                // Advance past the accepted prefix (partial writes are fully handled).
                let _ = bytes.drain(..n);
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    stream.flush()
}

fn shutdown(stream: &mut TcpStream) {
    stream.shutdown(std::net::Shutdown::Both).ok();
}

/// Convenience error frames (also used directly by tests).
pub fn bad_request(detail: &str) -> ReplyFrame {
    ResponseFrame::error(400, detail, false).into_frame()
}

/// Small extension so a [`ResponseFrame`] can travel as a [`ReplyFrame`].
pub trait IntoReply {
    fn into_frame(self) -> ReplyFrame;
}

impl IntoReply for ResponseFrame {
    fn into_frame(self) -> ReplyFrame {
        ReplyFrame {
            status: self.status,
            reason: self.reason,
            headers: self.headers,
            body: self.body,
            keep_alive: self.keep_alive,
        }
    }
}

#[cfg(test)]
mod server_tests {
    use super::*;
    use std::io::Read;
    use std::time::Instant;

    fn read_http_response(stream: &mut TcpStream) -> String {
        // Read a status line + headers + Content-Length body.
        let mut acc = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            match stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    acc.extend_from_slice(&tmp[..n]);
                    if let Some(head_end) = find_sub(&acc, b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&acc[..head_end]).to_string();
                        if let Some(cl) = header_of(&head, "content-length") {
                            let want: usize = cl.trim().parse().unwrap_or(0);
                            while acc.len() < head_end + 4 + want {
                                match stream.read(&mut tmp) {
                                    Ok(0) => break,
                                    Ok(m) => acc.extend_from_slice(&tmp[..m]),
                                    Err(_) => break,
                                }
                            }
                        }
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&acc).to_string()
    }

    fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
        if needle.len() > hay.len() {
            return None;
        }
        (0..=(hay.len() - needle.len())).find(|&i| &hay[i..i + needle.len()] == needle)
    }

    fn header_of(head: &str, name: &str) -> Option<String> {
        for line in head.split("\r\n").skip(1) {
            if let Some((k, v)) = line.split_once(':') {
                if k.trim().eq_ignore_ascii_case(name) {
                    return Some(v.trim().to_string());
                }
            }
        }
        None
    }

    #[test]
    fn ephemeral_bind_reports_real_address_and_port_plus_answered_request() {
        let (tx, rx) = channel::<ConnectionEvent>();
        let mut listener = spawn_listener("0.0.0.0", 0, 7, tx).expect("bind");
        assert_eq!(listener.address, "0.0.0.0", "wildcard bind must report 0.0.0.0");
        assert!(listener.port > 0, "ephemeral port must be resolved");

        let consumer = std::thread::spawn(move || {
            let ev = rx.recv_timeout(Duration::from_secs(5)).expect("event");
            assert_eq!(ev.server_id, 7);
            assert_eq!(ev.request.head.method, "GET");
            assert_eq!(ev.request.head.target, "/tail");
            assert_eq!(ev.request.head.header("x-test").as_deref(), Some("phone"));
            assert!(!ev.remote_address.is_empty());
            assert!(ev.remote_port > 0);
            assert!(ev.local_port > 0);
            ev.reply
                .send(ReplyFrame {
                    status: 200,
                    reason: None,
                    headers: vec![("content-type".into(), "application/json".into())],
                    body: b"{\"ok\":true}".to_vec(),
                    keep_alive: ev.keep_alive,
                })
                .expect("reply");
            ev.conn_id
        });

        let mut sock = TcpStream::connect(("127.0.0.1", listener.port)).expect("connect");
        sock.set_nodelay(true).ok();
        sock.write_all(b"GET /tail HTTP/1.1\r\nHost: x\r\nX-Test: phone\r\nConnection: close\r\n\r\n")
            .unwrap();
        let resp = read_http_response(&mut sock);
        assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"), "got {resp:?}");
        // HTTP field names are case-insensitive; the engine emits caller-provided names verbatim
        // (like Node.js), so compare the parsed value rather than the raw capitalization.
        assert_eq!(header_of(&resp, "content-type").as_deref(), Some("application/json"));
        assert_eq!(header_of(&resp, "content-length").as_deref(), Some("11"));
        assert!(resp.contains("Connection: close\r\n"));
        assert!(resp.ends_with("{\"ok\":true}"));
        consumer.join().unwrap();
        assert!(stop_listener(&mut listener));
    }

    #[test]
    fn keep_alive_serves_multiple_sequential_requests_on_one_socket() {
        let (tx, rx) = channel::<ConnectionEvent>();
        let mut listener = spawn_listener("0.0.0.0", 0, 9, tx).expect("bind");
        let consumer = std::thread::spawn(move || {
            let mut ids = Vec::new();
            for i in 0..2 {
                let ev = rx.recv_timeout(Duration::from_secs(5)).expect("event");
                ids.push(ev.conn_id);
                assert_eq!(ev.conn_id, ids[0], "same socket reused -> same conn id");
                let body = format!("route-{}", ev.request.head.target);
                ev.reply
                    .send(ReplyFrame::new(if i == 0 { 200 } else { 201 }, body.into_bytes(), ev.keep_alive))
                    .expect("reply");
            }
            ids[0]
        });

        let mut sock = TcpStream::connect(("127.0.0.1", listener.port)).expect("connect");
        sock.set_nodelay(true).ok();
        sock.write_all(b"GET /one HTTP/1.1\r\nHost: x\r\nConnection: keep-alive\r\n\r\n").unwrap();
        let first = read_http_response(&mut sock);
        assert!(first.starts_with("HTTP/1.1 200 OK\r\n"), "first={first:?}");
        assert!(first.contains("Connection: keep-alive\r\n"));
        assert!(first.ends_with("route-/one"));
        sock.write_all(b"POST /two HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi")
            .unwrap();
        let second = read_http_response(&mut sock);
        assert!(second.starts_with("HTTP/1.1 201 Created\r\n"), "second={second:?}");
        assert!(second.ends_with("route-/two"));
        consumer.join().unwrap();
        assert!(stop_listener(&mut listener));
    }

    #[test]
    fn malformed_request_gets_400_and_server_survives_for_next_client() {
        let (tx, rx) = channel::<ConnectionEvent>();
        let mut listener = spawn_listener("0.0.0.0", 0, 11, tx).expect("bind");
        // No consumer thread exists on purpose: a garbage request must be answered by the engine.
        let mut junk = TcpStream::connect(("127.0.0.1", listener.port)).expect("connect junk");
        junk.set_nodelay(true).ok();
        junk.write_all(b"GARBAGE_NOT_HTTP\r\n\r\n").unwrap();
        let resp = read_http_response(&mut junk);
        assert!(resp.starts_with("HTTP/1.1 400 Bad Request\r\n"), "got {resp:?}");
        assert!(resp.contains("Connection: close\r\n"));

        // The listener is still healthy: a well-formed request reaches the channel afterwards.
        let good = std::thread::spawn(move || rx.recv_timeout(Duration::from_secs(5)).expect("good request"));
        let mut oksock = TcpStream::connect(("127.0.0.1", listener.port)).expect("connect good");
        oksock.set_nodelay(true).ok();
        oksock
            .write_all(b"GET /alive HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .unwrap();
        let ev = good.join().unwrap();
        assert_eq!(ev.request.head.target, "/alive");
        ev.reply.send(ReplyFrame::new(200, b"still-up".to_vec(), false)).unwrap();
        let mut buf = String::new();
        oksock.read_to_string(&mut buf).unwrap();
        assert!(buf.starts_with("HTTP/1.1 200 OK\r\n"), "post-400 response {buf:?}");
        assert!(stop_listener(&mut listener));
    }

    #[test]
    fn oversized_body_gets_413_and_closes_only_that_connection() {
        let (tx, rx) = channel::<ConnectionEvent>();
        let mut listener = spawn_listener("0.0.0.0", 0, 13, tx).expect("bind");
        let mut big = TcpStream::connect(("127.0.0.1", listener.port)).expect("connect big");
        big.set_nodelay(true).ok();
        big.write_all(b"POST /huge HTTP/1.1\r\nHost: x\r\nContent-Length: 2097152\r\n\r\nx")
            .unwrap();
        let resp = read_http_response(&mut big);
        assert!(resp.starts_with("HTTP/1.1 413 Payload Too Large\r\n"), "got {resp:?}");
        assert!(resp.contains("Connection: close\r\n"));

        let second = std::thread::spawn(move || rx.recv_timeout(Duration::from_secs(5)).expect("next request"));
        let mut fine = TcpStream::connect(("127.0.0.1", listener.port)).expect("connect fine");
        fine.set_nodelay(true).ok();
        fine.write_all(b"GET /fine HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
        let ev = second.join().unwrap();
        assert_eq!(ev.request.head.target, "/fine");
        ev.reply.send(ReplyFrame::new(200, b"fine".to_vec(), false)).unwrap();
        let mut text = String::new();
        fine.read_to_string(&mut text).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "{text:?}");
        assert!(stop_listener(&mut listener));
    }

    #[test]
    fn eight_concurrent_clients_are_all_served_without_dropping() {
        let (tx, rx) = channel::<ConnectionEvent>();
        let mut listener = spawn_listener("0.0.0.0", 0, 21, tx).expect("bind");
        // Consumer mimics the JS event loop: drain with a short timeout and answer each request.
        let consumer = std::thread::spawn(move || {
            let mut answered = 0;
            loop {
                match rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(ev) => {
                        let body = format!("hello {}", ev.request.head.target);
                        ev.reply
                            .send(ReplyFrame::new(200, body.into_bytes(), ev.keep_alive))
                            .expect("send frame");
                        answered += 1;
                    }
                    Err(_) => break,
                }
            }
            answered
        });

        let mut conns = Vec::new();
        for i in 0..8 {
            let mut s = TcpStream::connect(("127.0.0.1", listener.port)).expect("connect concurrent");
            s.set_nodelay(true).ok();
            s.write_all(format!("GET /c{i} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes())
                .unwrap();
            conns.push(s);
        }
        let mut bodies = Vec::new();
        for (i, mut s) in conns.drain(..).enumerate() {
            let text = read_http_response(&mut s);
            assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "conn {i} got {text:?}");
            assert!(text.ends_with(&format!("hello /c{i}")), "conn {i} body {text:?}");
            bodies.push(text);
        }
        assert_eq!(bodies.len(), 8);
        assert_eq!(consumer.join().unwrap(), 8, "all 8 requests reached the engine consumer");
        assert!(stop_listener(&mut listener));
    }

    #[test]
    fn chunked_request_body_reaches_consumer_then_reply_roundtrips() {
        let (tx, rx) = channel::<ConnectionEvent>();
        let mut listener = spawn_listener("0.0.0.0", 0, 31, tx).expect("bind");
        let consumer = std::thread::spawn(move || {
            let ev = rx.recv_timeout(Duration::from_secs(5)).expect("chunked event");
            assert_eq!(ev.request.head.header("transfer-encoding").as_deref(), Some("chunked"));
            assert_eq!(ev.request.body, b"{\"brightness\":65}".to_vec());
            ev.reply
                .send(ReplyFrame {
                    status: 200,
                    reason: None,
                    headers: vec![("content-type".into(), "application/json".into())],
                    body: b"{\"brightness\":65}".to_vec(),
                    keep_alive: false,
                })
                .expect("reply");
        });
        let mut sock = TcpStream::connect(("127.0.0.1", listener.port)).expect("connect chunked");
        sock.set_nodelay(true).ok();
        // Mirrors a chunked POST split like the reference E2E: 8 + 9 bytes == {"brightness":65}
        sock.write_all(concat!(
            "POST /api/backlight HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            "8\r\n{\"bright\r\n",
            "9\r\nness\":65}\r\n",
            "0\r\n\r\n",
        ).as_bytes())
        .unwrap();
        // Wait for the consumer first: a truncated/garbled chunk cannot masquerade as a 200.
        consumer.join().unwrap();
        let text = read_http_response(&mut sock);
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "got {text:?}");
        assert!(text.ends_with("{\"brightness\":65}"));
        assert!(stop_listener(&mut listener));
    }

    #[test]
    fn stopping_listener_frees_port_and_refuses_new_connections() {
        // Bind, stop, then re-bind the SAME port (proves the socket was really released).
        let (tx, rx) = channel::<ConnectionEvent>();
        let mut listener = spawn_listener("127.0.0.1", 0, 41, tx).expect("bind first");
        let port = listener.port;
        let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect before stop");
        s.write_all(b"GET /pre HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
        let ev = rx.recv_timeout(Duration::from_secs(5)).expect("pre-stop event");
        ev.reply.send(ReplyFrame::new(200, b"pre".to_vec(), false)).unwrap();
        let mut text = String::new();
        s.read_to_string(&mut text).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK"));
        assert!(stop_listener(&mut listener), "accept thread joined cleanly");
        let again = bind_socket("127.0.0.1", port).expect("rebind same ephemeral port after stop");
        assert_eq!(again.local_addr().unwrap().port(), port);
        drop(again);
    }

    #[test]
    fn unanswered_request_times_out_with_500_and_connection_closes() {
        // Tiny JS budget: nobody ever reads/replies, so the reader thread self-recovers fast.
        let (tx, rx) = channel::<ConnectionEvent>();
        let mut listener =
            spawn_listener_with_timeout("127.0.0.1", 0, 43, tx, Duration::from_millis(150)).expect("bind");
        let port = listener.port;
        let mut sock = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        sock.write_all(b"GET /hang HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
        let text = read_http_response(&mut sock);
        assert!(text.starts_with("HTTP/1.1 500 Internal Server Error\r\n"), "timeout frame {text:?}");
        assert!(text.contains("Connection: close\r\n"));
        // The parked event is still queued; dropping the consumer frees the engine.
        drop(rx);
        assert!(stop_listener(&mut listener));
    }

    #[test]
    fn dropped_reply_channel_answers_500_promptly() {
        // Simulates JS garbage-collecting a ServerResponse without res.end(): the per-request
        // reply sender is dropped -> Disconnected wakes immediately (no timeout wait).
        let (tx, rx) = channel::<ConnectionEvent>();
        let mut listener =
            spawn_listener_with_timeout("127.0.0.1", 0, 45, tx, Duration::from_secs(30)).expect("bind");
        let port = listener.port;
        let dropper = std::thread::spawn(move || {
            let ev = rx.recv_timeout(Duration::from_secs(5)).expect("event arrives");
            drop(ev.reply); // never send a frame
        });
        let mut sock = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        sock.write_all(b"GET /orphan HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
        let t0 = Instant::now();
        let text = read_http_response(&mut sock);
        assert!(text.starts_with("HTTP/1.1 500 Internal Server Error\r\n"), "dropped-reply frame {text:?}");
        assert!(t0.elapsed() < Duration::from_secs(2), "Disconnected wake must be prompt, took {t0:?}");
        dropper.join().unwrap();
        assert!(stop_listener(&mut listener));
    }

    #[test]
    fn interim_100_continue_is_emitted_before_body_and_request_still_parses() {
        let (tx, rx) = channel::<ConnectionEvent>();
        let mut listener = spawn_listener("127.0.0.1", 0, 47, tx).expect("bind");
        let consumer = std::thread::spawn(move || {
            let ev = rx.recv_timeout(Duration::from_secs(5)).expect("expect-continue event");
            assert_eq!(ev.request.head.method, "PUT");
            assert_eq!(ev.request.body, b"payload-here".to_vec());
            ev.reply.send(ReplyFrame::new(201, b"stored".to_vec(), false)).unwrap();
        });
        let mut sock = TcpStream::connect(("127.0.0.1", listener.port)).expect("connect");
        sock.set_nodelay(true).ok();
        sock.write_all(b"PUT /up HTTP/1.1\r\nHost: x\r\nExpect: 100-continue\r\nContent-Length: 12\r\nConnection: close\r\n\r\n")
            .unwrap();
        // Read the interim 100 line first, then send the body, then read the final response.
        let mut interim = [0u8; 64];
        let n = sock.read(&mut interim).unwrap();
        let head = String::from_utf8_lossy(&interim[..n]);
        assert!(head.starts_with("HTTP/1.1 100 Continue"), "interim={head:?}");
        sock.write_all(b"payload-here").unwrap();
        consumer.join().unwrap();
        let rest = read_http_response(&mut sock);
        assert!(rest.starts_with("HTTP/1.1 201 Created\r\n"), "final={rest:?}");
        assert!(rest.ends_with("stored"));
        assert!(stop_listener(&mut listener));
    }

    #[test]
    fn reply_frames_encode_reserved_headers_correctly() {
        let frame = ReplyFrame {
            status: 204,
            reason: Some("Bye".into()),
            headers: vec![("connection".into(), "keep-alive".into())],
            body: Vec::new(),
            keep_alive: false,
        };
        let text = String::from_utf8(frame.encode()).unwrap();
        assert!(text.starts_with("HTTP/1.1 204 Bye\r\n"));
        // Engine decides Connection, user-supplied copies are dropped; length auto-added.
        assert_eq!(text.matches("Connection:").count(), 1);
        assert!(text.contains("Connection: close\r\n"));
        assert!(text.contains("Content-Length: 0\r\n"));
        assert_eq!(bad_request("x").status, 400);
    }
}
