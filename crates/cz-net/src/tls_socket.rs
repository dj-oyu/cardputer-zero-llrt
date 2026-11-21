//! Phase 8C: thread-based full-duplex **raw TLS sockets** (`node:tls` engine) on the shared
//! `cz-net` rustls foundation — zero Tokio, zero C TLS libraries, `cz-hal` strictly `libc`-only.
//!
//! Architecture mirrors the `fetch`/`http` layers: every socket/server runs on its own detached
//! worker thread (256 KiB stacks via `std::thread::Builder`), and every event (`SecureConnect`,
//! `ServerConnection`, `Data`, `End`, `Close`, `Error`) lands in a process-wide queue that calls
//! `cz_hal::http_server::notify_event_loop()` — so `cz-runner`'s Condvar idle wait wakes
//! immediately and there are NO polling loops on the JS side.
//!
//! I/O model: blocking `TcpStream`s with a 20 ms read slice let each worker interleave
//! outbound writes (`write()` / `end()` control messages) with reads instead of deadlocking on a
//! bare `read()`. Orderly shutdown sends TLS `close_notify` then `Shutdown::Write`; `destroy()`
//! best-effort `close_notify` + `TcpStream::shutdown(Shutdown::Both)` (a stored socket clone lets
//! an external destroy wake even a handshaking worker). Server handshakes are bounded by
//! [`SERVER_HANDSHAKE_TIMEOUT`] so malformed/plaintext clients can never stall the listener.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cz_hal::http_server::notify_event_loop;
use rustls::pki_types::ServerName;
use rustls::{ClientConnection, ConnectionCommon, ServerConnection, StreamOwned, SideData};

use crate::tls::{build_client_config, build_server_config, TlsClientOptions, TlsServerOptions};

/// Worker thread stack budget (spec §1.2): 256 KiB, matching the Phase 8A fetch workers.
pub const TLS_WORKER_STACK_BYTES: usize = 256 * 1024;

/// Read slice that interleaves control-channel writes with reads on blocking sockets.
const IO_SLICE: Duration = Duration::from_millis(20);

/// Bounded server-side handshake window: plaintext / stalled clients fail within 5 s and only
/// drop THEIR socket — the listener and all other connections keep serving.
pub const SERVER_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Accept-thread poll slice: `close()` stops accepting and joins within one slice.
const ACCEPT_POLL_MS: i32 = 50;

/// Application read buffer per socket worker.
const READ_CHUNK: usize = 16 * 1024;

/// One engine event queued toward the JavaScript drain (`__czTlsDrainLoop`).
#[derive(Debug, Clone)]
pub enum TlsEngineEvent {
    /// Client handshake completed (negotiated ALPN + trust decision available).
    SecureConnect {
        socket_id: u64,
        alpn_protocol: Option<String>,
        authorized: bool,
        remote_address: String,
        remote_port: u16,
    },
    /// Server accepted a connection whose TLS handshake completed.
    ServerConnection {
        server_id: u64,
        socket_id: u64,
        alpn_protocol: Option<String>,
        authorized: bool,
        remote_address: String,
        remote_port: u16,
    },
    /// Decrypted bytes arrived from the peer.
    Data { socket_id: u64, bytes: Vec<u8> },
    /// Peer closed the TLS session cleanly (EOF / `close_notify`) or we did an orderly `end()`.
    End { socket_id: u64 },
    /// Socket is fully gone (deregistered; no more events will follow for this id).
    Close { socket_id: u64 },
    /// Socket-level TLS failure (handshake rejection, post-handshake alert, reset).
    Error { socket_id: u64, message: String },
    /// Server-level TLS failure (a client handshake that never became a socket).
    ServerError { server_id: u64, message: String },
}

/// Control-channel messages JS sends into a live worker thread.
enum Control {
    Write(Vec<u8>),
    End(Vec<u8>),
    Destroy,
}

/// Shared destroy latch: an external `destroy()` flips it even before the worker owns a socket.
type DestroyFlag = Arc<AtomicBool>;

struct SocketEntry {
    ctrl_tx: Sender<Control>,
    /// Second handle on the same connection so `destroy()`/`close()` can wake blocked reads via
    /// `shutdown(Shutdown::Both)` (filled by the worker once `connect()` returns).
    wake_slot: Arc<Mutex<Option<TcpStream>>>,
    destroyed: DestroyFlag,
    /// Owning TLS server id for accepted connections (`None` for outbound client sockets).
    server_id: Option<u64>,
}

struct ServerEntry {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

fn sockets() -> &'static Mutex<HashMap<u64, SocketEntry>> {
    static S: OnceLock<Mutex<HashMap<u64, SocketEntry>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

fn events() -> &'static Mutex<VecDeque<TlsEngineEvent>> {
    static E: OnceLock<Mutex<VecDeque<TlsEngineEvent>>> = OnceLock::new();
    E.get_or_init(|| Mutex::new(VecDeque::new()))
}

fn servers() -> &'static Mutex<HashMap<u64, ServerEntry>> {
    static SR: OnceLock<Mutex<HashMap<u64, ServerEntry>>> = OnceLock::new();
    SR.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_socket_id() -> u64 {
    static N: AtomicU64 = AtomicU64::new(1);
    N.fetch_add(1, Ordering::SeqCst)
}

fn next_server_id() -> u64 {
    static N: AtomicU64 = AtomicU64::new(100_001);
    N.fetch_add(1, Ordering::SeqCst)
}

/// Push one event and wake the JS event loop instantly (Condvar `Waker` in `cz-hal`).
fn push_event(ev: TlsEngineEvent) {
    events().lock().expect("tls event queue lock").push_back(ev);
    notify_event_loop();
}

/// Pop the next queued event (`None` when the drain queue is empty).
pub fn next_tls_event() -> Option<TlsEngineEvent> {
    events().lock().expect("tls event queue lock").pop_front()
}

/// True while engine events await the JavaScript drain (event-loop liveness).
pub fn tls_has_pending() -> bool {
    events().lock().expect("tls event queue lock").len() > 0
}

/// Open client sockets + open server-side sockets + listening TLS servers (liveness).
pub fn tls_active_count() -> usize {
    let socks = sockets().lock().expect("sockets lock").len();
    let srvs = servers().lock().expect("servers lock").len();
    socks + srvs
}

fn deregister_socket(socket_id: u64) {
    sockets().lock().expect("sockets lock").remove(&socket_id);
}

/// Queue a control message to a live socket; false when the id is gone.
fn send_control(socket_id: u64, msg: Control) -> bool {
    // Do NOT hold the registry mutex across `send`: a dead worker channel must not wedge JS.
    let tx = match sockets().lock().expect("sockets lock").get(&socket_id) {
        Some(entry) => entry.ctrl_tx.clone(),
        None => return false,
    };
    tx.send(msg).is_ok()
}

// ---------------------------------------------------------------------------
// Connection boxing: one duplex I/O loop for client and server rustls sessions
// ---------------------------------------------------------------------------

/// Either rustls session shape behind one shared full-duplex loop (no logic duplication).
enum ConnBox {
    Client(StreamOwned<ClientConnection, TcpStream>),
    Server(StreamOwned<ServerConnection, TcpStream>),
}

impl ConnBox {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            ConnBox::Client(s) => s.read(buf),
            ConnBox::Server(s) => s.read(buf),
        }
    }
    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match self {
            ConnBox::Client(s) => s.write_all(bytes),
            ConnBox::Server(s) => s.write_all(bytes),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            ConnBox::Client(s) => s.flush(),
            ConnBox::Server(s) => s.flush(),
        }
    }
    /// Send TLS `close_notify` and hand every buffered TLS record to the socket (bounded drain).
    fn send_close_notify(&mut self) {
        match self {
            ConnBox::Client(stream) => {
                stream.conn.send_close_notify();
                let StreamOwned { conn, sock } = stream;
                drain_outbound(conn, sock);
            }
            ConnBox::Server(stream) => {
                stream.conn.send_close_notify();
                let StreamOwned { conn, sock } = stream;
                drain_outbound(conn, sock);
            }
        }
    }
    fn socket(&self) -> &TcpStream {
        match self {
            ConnBox::Client(s) => &s.sock,
            ConnBox::Server(s) => &s.sock,
        }
    }
}

/// Flush rustls' outbound queue onto the raw socket (guarded against infinite loops).
fn drain_outbound(conn: &mut ConnectionCommon<impl SideData>, sock: &mut TcpStream) {
    let mut guard = 0usize;
    while conn.wants_write() && guard < 200 {
        guard += 1;
        match conn.write_tls(sock) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    sock.flush().ok();
}

// ---------------------------------------------------------------------------
// Client sockets
// ---------------------------------------------------------------------------

/// Start an outbound TLS connection. Returns the socket id immediately; `secureConnect`/`error`
/// arrive through the event queue (Node connect is asynchronous, never blocking JS).
pub fn connect_tls(
    opts: TlsClientOptions,
    host: String,
    port: u16,
    servername: Option<String>,
) -> Result<u64, String> {
    if host.trim().is_empty() {
        return Err("tls.connect requires a non-empty 'host'".to_string());
    }
    let socket_id = next_socket_id();
    let (ctrl_tx, ctrl_rx) = channel::<Control>();
    let destroyed: DestroyFlag = Arc::new(AtomicBool::new(false));
    let wake_slot: Arc<Mutex<Option<TcpStream>>> = Arc::new(Mutex::new(None));
    sockets().lock().expect("sockets lock").insert(
        socket_id,
        SocketEntry {
            ctrl_tx,
            wake_slot: wake_slot.clone(),
            destroyed: destroyed.clone(),
            server_id: None,
        },
    );
    if let Err(e) = std::thread::Builder::new()
        .name(format!("cz-tls-client-{socket_id}"))
        .stack_size(TLS_WORKER_STACK_BYTES)
        .spawn(move || {
            client_worker(socket_id, host, port, servername, opts, ctrl_rx, destroyed, wake_slot)
        })
    {
        deregister_socket(socket_id);
        return Err(format!("cannot spawn TLS worker: {e}"));
    }
    Ok(socket_id)
}

/// Queue plaintext onto a live socket (`write()`); false once the socket is gone.
pub fn tls_write(socket_id: u64, bytes: Vec<u8>) -> bool {
    send_control(socket_id, Control::Write(bytes))
}

/// Orderly `end()`: flush pending plaintext, send TLS `close_notify`, half-close the TCP write
/// side, then keep reading until the peer closes too (Node full close semantics).
pub fn tls_end(socket_id: u64, last_bytes: Vec<u8>) -> bool {
    send_control(socket_id, Control::End(last_bytes))
}

/// Hard `destroy()`: latch, best-effort `close_notify` via the control channel, and shut the TCP
/// socket down both directions so even a handshaking worker exits promptly.
pub fn tls_destroy(socket_id: u64) -> bool {
    // Snapshot handles WITHOUT holding the registry mutex across `send`/`shutdown`.
    let (destroyed, wake, tx) = {
        let reg = sockets().lock().expect("sockets lock");
        match reg.get(&socket_id) {
            Some(entry) => (
                entry.destroyed.clone(),
                entry.wake_slot.clone(),
                entry.ctrl_tx.clone(),
            ),
            None => return false,
        }
    };
    destroyed.store(true, Ordering::SeqCst);
    if let Some(sock) = wake.lock().expect("wake slot lock").as_ref() {
        sock.shutdown(Shutdown::Both).ok();
    }
    tx.send(Control::Destroy).is_ok()
}

fn install_wake_handle(wake: &Arc<Mutex<Option<TcpStream>>>, sock: TcpStream) {
    *wake.lock().expect("wake slot lock") = Some(sock);
}

fn peer_text(tcp: &TcpStream) -> (String, u16) {
    match tcp.peer_addr() {
        Ok(a) => (a.ip().to_string(), a.port()),
        Err(_) => (String::new(), 0),
    }
}

fn client_worker(
    socket_id: u64,
    host: String,
    port: u16,
    servername: Option<String>,
    opts: TlsClientOptions,
    ctrl_rx: Receiver<Control>,
    destroyed: DestroyFlag,
    wake_slot: Arc<Mutex<Option<TcpStream>>>,
) {
    // Connect asynchronously on the worker; failures surface as error + close like Node.
    let mut tcp = match TcpStream::connect((host.as_str(), port)) {
        Ok(t) => t,
        Err(e) => {
            push_event(TlsEngineEvent::Error {
                socket_id,
                message: format!("TLS connect to {host}:{port} failed: {e}"),
            });
            push_event(TlsEngineEvent::Close { socket_id });
            deregister_socket(socket_id);
            return;
        }
    };
    if destroyed.load(Ordering::SeqCst) || drain_control_abort(&ctrl_rx, socket_id, &tcp) {
        tcp.shutdown(Shutdown::Both).ok();
        push_event(TlsEngineEvent::Close { socket_id });
        deregister_socket(socket_id);
        return;
    }
    tcp.set_nodelay(true).ok();
    let (remote_address, remote_port) = peer_text(&tcp);
    if let Ok(clone) = tcp.try_clone() {
        install_wake_handle(&wake_slot, clone);
    }

    let cfg = match build_client_config(&opts) {
        Ok(c) => c,
        Err(msg) => {
            push_event(TlsEngineEvent::Error { socket_id, message: msg });
            push_event(TlsEngineEvent::Close { socket_id });
            deregister_socket(socket_id);
            return;
        }
    };
    // Node-like SNI: rustls maps DNS names to the SNI extension and IP addresses to NO SNI
    // (`ServerName::IpAddress`). A non-DNS, non-IP servername is a descriptive configuration error.
    let sni_name = servername.unwrap_or(host);
    // `ClientConnection::new` needs `ServerName<'static>`: IPs convert losslessly, DNS names
    // move out of the owned `String`; anything else is a descriptive configuration error.
    let name = match sni_name.parse::<std::net::IpAddr>() {
        Ok(ip) => ServerName::from(ip),
        Err(_) => match ServerName::try_from(sni_name.as_str()) {
            Ok(_) => ServerName::try_from(sni_name).expect("validated dns name"),
            Err(_) => {
                push_event(TlsEngineEvent::Error {
                    socket_id,
                    message: format!("TLS error: invalid servername {sni_name:?}"),
                });
                push_event(TlsEngineEvent::Close { socket_id });
                deregister_socket(socket_id);
                return;
            }
        },
    };
    let mut conn = match ClientConnection::new(cfg, name) {
        Ok(c) => c,
        Err(e) => {
            push_event(TlsEngineEvent::Error {
                socket_id,
                message: format!("TLS error: {e}"),
            });
            push_event(TlsEngineEvent::Close { socket_id });
            deregister_socket(socket_id);
            return;
        }
    };

    // Handshake in IO_SLICE slices so destroy() (flag + wake shutdown) interrupts it.
    tcp.set_read_timeout(Some(IO_SLICE)).ok();
    tcp.set_write_timeout(Some(SERVER_HANDSHAKE_TIMEOUT)).ok();
    while conn.is_handshaking() {
        if destroyed.load(Ordering::SeqCst) || drain_control_abort(&ctrl_rx, socket_id, &tcp) {
            tcp.shutdown(Shutdown::Both).ok();
            push_event(TlsEngineEvent::Close { socket_id });
            deregister_socket(socket_id);
            return;
        }
        match conn.complete_io(&mut tcp) {
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => {
                // Node semantics: certificate rejection -> "error" then "close", no secureConnect.
                push_event(TlsEngineEvent::Error {
                    socket_id,
                    message: format!("TLS handshake failed: {e}"),
                });
                tcp.shutdown(Shutdown::Both).ok();
                push_event(TlsEngineEvent::Close { socket_id });
                deregister_socket(socket_id);
                return;
            }
        }
    }

    let alpn_protocol = conn
        .alpn_protocol()
        .map(|p| String::from_utf8_lossy(p).into_owned());
    // authorized mirrors `reject_unauthorized`: true only when certificate verification ran and
    // passed (the handshake above already verified it). With verification disabled it is false.
    let authorized = opts.reject_unauthorized;
    tcp.set_write_timeout(None).ok();
    push_event(TlsEngineEvent::SecureConnect {
        socket_id,
        alpn_protocol,
        authorized,
        remote_address,
        remote_port,
    });

    let mut stream = ConnBox::Client(StreamOwned::new(conn, tcp));
    duplex_loop(socket_id, None, &ctrl_rx, &destroyed, &mut stream);
}

/// Handle control messages during the pre-I/O phases; true when End/Destroy ends the session.
fn drain_control_abort(ctrl_rx: &Receiver<Control>, socket_id: u64, tcp: &TcpStream) -> bool {
    while let Ok(msg) = ctrl_rx.try_recv() {
        match msg {
            Control::Write(_) => {} // nothing writable before the handshake completed
            Control::End(_) | Control::Destroy => {
                let _ = socket_id;
                tcp.shutdown(Shutdown::Both).ok();
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Server listeners
// ---------------------------------------------------------------------------

/// Bind + accept on a worker thread for a TLS server (invalid PEM/key fails fast, before bind).
/// Returns `(server_id, bound address, bound port)` (`port = 0` yields an ephemeral port).
pub fn create_tls_server(
    opts: &TlsServerOptions,
    host: &str,
    port: u16,
) -> Result<(u64, String, u16), String> {
    let cfg = build_server_config(opts)?;
    let normalized = match host.trim() {
        "" | "*" | "any" => "0.0.0.0",
        other => other,
    };
    let listener = TcpListener::bind((normalized, port))
        .map_err(|e| format!("tls listen {normalized}:{port} failed: {e}"))?;
    set_reuseaddr(&listener);
    let bound = listener
        .local_addr()
        .map_err(|e| format!("tls local_addr failed: {e}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("tls nonblocking accept failed: {e}"))?;

    let server_id = next_server_id();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let handle = std::thread::Builder::new()
        .name(format!("cz-tls-accept-{server_id}"))
        .stack_size(TLS_WORKER_STACK_BYTES)
        .spawn(move || accept_loop(listener, stop_thread, server_id, cfg))
        .map_err(|e| format!("cannot spawn TLS accept thread: {e}"))?;
    servers().lock().expect("servers lock").insert(
        server_id,
        ServerEntry {
            stop,
            handle: Some(handle),
        },
    );
    Ok((server_id, bound.ip().to_string(), bound.port()))
}

/// Stop accepting, join the accept thread (bounded by one poll slice), and orderly-close every
/// live socket owned by this server (Node `close()` closes currently open connections).
pub fn close_tls_server(server_id: u64) -> bool {
    // Drop the registry entry FIRST so liveness (`tls_active_count`) reacts immediately.
    let entry = servers().lock().expect("servers lock").remove(&server_id);
    let owned: Vec<u64> = sockets()
        .lock()
        .expect("sockets lock")
        .iter()
        .filter(|(_, e)| e.server_id == Some(server_id))
        .map(|(id, _)| *id)
        .collect();
    for id in owned {
        tls_destroy(id);
    }
    match entry {
        Some(mut e) => {
            // Signal the accept thread, then join it (bounded by one ACCEPT_POLL_MS slice).
            e.stop.store(true, Ordering::SeqCst);
            match e.handle.take() {
                Some(h) => h.join().is_ok(),
                None => true,
            }
        }
        None => false,
    }
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

use std::os::fd::AsRawFd;

fn accept_loop(listener: TcpListener, stop: Arc<AtomicBool>, server_id: u64, cfg: Arc<rustls::ServerConfig>) {
    while !stop.load(Ordering::SeqCst) {
        let mut pfd = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, ACCEPT_POLL_MS) };
        if rc < 0 {
            continue; // EINTR-style: re-check the stop flag and keep polling
        }
        loop {
            match listener.accept() {
                Ok((stream, peer)) => {
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    spawn_server_connection(stream, peer, server_id, cfg.clone());
                }
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted) => break,
                Err(_) => {
                    // Transient accept errors (ECONNABORTED, EMFILE, ENFILE) must not kill the listener.
                    std::thread::sleep(Duration::from_millis(5));
                    break;
                }
            }
        }
    }
}

fn spawn_server_connection(stream: TcpStream, peer: std::net::SocketAddr, server_id: u64, cfg: Arc<rustls::ServerConfig>) {
    std::thread::Builder::new()
        .name(format!("cz-tls-srvconn-{server_id}"))
        .stack_size(TLS_WORKER_STACK_BYTES)
        .spawn(move || server_conn_worker(stream, peer, server_id, cfg))
        .ok();
}

fn server_conn_worker(
    mut tcp: TcpStream,
    peer: std::net::SocketAddr,
    server_id: u64,
    cfg: Arc<rustls::ServerConfig>,
) {
    let socket_id = next_socket_id();
    let (ctrl_tx, ctrl_rx) = channel::<Control>();
    let destroyed: DestroyFlag = Arc::new(AtomicBool::new(false));
    // Register BEFORE the handshake so `server.close()` can destroy even mid-handshake sockets.
    if let Ok(clone) = tcp.try_clone() {
        let wake: Arc<Mutex<Option<TcpStream>>> = Arc::new(Mutex::new(Some(clone)));
        sockets().lock().expect("sockets lock").insert(
            socket_id,
            SocketEntry {
                ctrl_tx,
                wake_slot: wake,
                destroyed: destroyed.clone(),
                server_id: Some(server_id),
            },
        );
    } else {
        return;
    }

    tcp.set_nodelay(true).ok();
    // Bounded handshake window (spec §1.2): a stalled or plaintext client can never pin a worker.
    tcp.set_read_timeout(Some(SERVER_HANDSHAKE_TIMEOUT)).ok();
    tcp.set_write_timeout(Some(SERVER_HANDSHAKE_TIMEOUT)).ok();
    let mut conn = match ServerConnection::new(cfg) {
        Ok(c) => c,
        Err(e) => {
            push_event(TlsEngineEvent::ServerError {
                server_id,
                message: format!("TLS error: {e}"),
            });
            deregister_socket(socket_id);
            return;
        }
    };
    let started = Instant::now();
    while conn.is_handshaking() {
        if destroyed.load(Ordering::SeqCst) {
            tcp.shutdown(Shutdown::Both).ok();
            push_event(TlsEngineEvent::Close { socket_id });
            deregister_socket(socket_id);
            return;
        }
        match conn.complete_io(&mut tcp) {
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) =>
            {
                if started.elapsed() > SERVER_HANDSHAKE_TIMEOUT + IO_SLICE {
                    // Absolute deadline backstop in case timeouts never surface as errors.
                    tcp.shutdown(Shutdown::Both).ok();
                    deregister_socket(socket_id);
                    return;
                }
            }
            Err(e) => {
                // Plaintext-on-TLS / mTLS rejection: drop THIS socket only (+ server 'error').
                let msg = format!("TLS handshake failed: {e}");
                tcp.shutdown(Shutdown::Both).ok();
                if !destroyed.load(Ordering::SeqCst) {
                    push_event(TlsEngineEvent::ServerError {
                        server_id,
                        message: msg,
                    });
                }
                deregister_socket(socket_id);
                return;
            }
        }
    }

    let alpn_protocol = conn
        .alpn_protocol()
        .map(|p| String::from_utf8_lossy(p).into_owned());
    // authorized: the peer certificate chain was presented AND verified (rustls only delivers
    // peer certificates when verification passed). No client cert -> false, exactly like Node.
    let authorized = conn
        .peer_certificates()
        .map(|c| !c.is_empty())
        .unwrap_or(false);
    tcp.set_read_timeout(Some(IO_SLICE)).ok();
    tcp.set_write_timeout(None).ok();
    push_event(TlsEngineEvent::ServerConnection {
        socket_id,
        server_id,
        alpn_protocol,
        authorized,
        remote_address: peer.ip().to_string(),
        remote_port: peer.port(),
    });

    let mut stream = ConnBox::Server(StreamOwned::new(conn, tcp));
    duplex_loop(socket_id, Some(server_id), &ctrl_rx, &destroyed, &mut stream);
}

// ---------------------------------------------------------------------------
// Shared full-duplex I/O loop (client + server sessions)
// ---------------------------------------------------------------------------

fn duplex_loop(
    socket_id: u64,
    _server_id: Option<u64>,
    ctrl_rx: &Receiver<Control>,
    destroyed: &DestroyFlag,
    stream: &mut ConnBox,
) {
    let mut local_end = false;
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        // 1. Drain control messages: writes flush immediately; End/Destroy close orderly.
        while let Ok(msg) = ctrl_rx.try_recv() {
            match msg {
                Control::Write(bytes) => {
                    if local_end || destroyed.load(Ordering::SeqCst) || bytes.is_empty() {
                        continue;
                    }
                    if let Err(e) = stream.write_all(&bytes).and_then(|_| stream.flush()) {
                        push_close_after_error(socket_id, destroyed, local_end, e, stream);
                        return;
                    }
                }
                Control::End(bytes) => {
                    if !local_end && !destroyed.load(Ordering::SeqCst) {
                        if !bytes.is_empty() {
                            if let Err(e) = stream.write_all(&bytes).and_then(|_| stream.flush()) {
                                push_close_after_error(socket_id, destroyed, local_end, e, stream);
                                return;
                            }
                        }
                        stream.send_close_notify();
                        stream.socket().shutdown(Shutdown::Write).ok();
                        local_end = true;
                    }
                }
                Control::Destroy => {
                    stream.send_close_notify();
                    stream.socket().shutdown(Shutdown::Both).ok();
                    push_event(TlsEngineEvent::Close { socket_id });
                    deregister_socket(socket_id);
                    return;
                }
            }
        }

        // 2. Read with the IO_SLICE timeout so writes/end never deadlock on a bare read.
        match stream.read(&mut buf[..]) {
            Ok(n) if n > 0 => {
                push_event(TlsEngineEvent::Data {
                    socket_id,
                    bytes: buf[..n].to_vec(),
                });
            }
            Ok(_) => {
                // Peer closed cleanly (close_notify/EOF): answer with our own close_notify and
                // finish the TCP session; emit End then Close exactly once.
                stream.send_close_notify();
                stream.socket().shutdown(Shutdown::Both).ok();
                push_event(TlsEngineEvent::End { socket_id });
                push_event(TlsEngineEvent::Close { socket_id });
                deregister_socket(socket_id);
                return;
            }
            Err(e) => {
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted
                ) {
                    continue; // next slice: check control channel again
                }
                if destroyed.load(Ordering::SeqCst) {
                    // destroy(): silent teardown — Node emits 'close' only after destroy.
                    push_event(TlsEngineEvent::Close { socket_id });
                    deregister_socket(socket_id);
                    return;
                }
                if local_end && e.kind() == std::io::ErrorKind::UnexpectedEof {
                    // We already sent close_notify; a bare FIN/reset after end() is orderly-ish.
                    push_event(TlsEngineEvent::End { socket_id });
                    push_event(TlsEngineEvent::Close { socket_id });
                    deregister_socket(socket_id);
                    return;
                }
                push_close_after_error(socket_id, destroyed, local_end, e, stream);
                return;
            }
        }
    }
}

/// Terminal failure path: emit `error` (unless externally destroyed / locally ended) then `close`.
fn push_close_after_error(
    socket_id: u64,
    destroyed: &DestroyFlag,
    local_end: bool,
    err: std::io::Error,
    stream: &mut ConnBox,
) {
    stream.socket().shutdown(Shutdown::Both).ok();
    if !destroyed.load(Ordering::SeqCst) && !(local_end && err.kind() == std::io::ErrorKind::UnexpectedEof) {
        push_event(TlsEngineEvent::Error {
            socket_id,
            message: format!("TLS error: {err}"),
        });
    }
    push_event(TlsEngineEvent::Close { socket_id });
    deregister_socket(socket_id);
}

#[cfg(test)]
mod tls_socket_tests {
    use super::*;
    use std::io::Write;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("{}/testdata/{name}", env!("CARGO_MANIFEST_DIR")))
            .expect("fixture pem must exist")
    }

    /// Pop events until `pred` matches (or the deadline expires), keeping unmatched ones.
    fn drain_until(
        pred: impl Fn(&TlsEngineEvent) -> bool,
        kept: &mut Vec<TlsEngineEvent>,
        ms: u64,
    ) -> Option<TlsEngineEvent> {
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(ms) {
            if let Some(ev) = next_tls_event() {
                if pred(&ev) {
                    return Some(ev);
                }
                kept.push(ev);
            } else {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        None
    }

    /// Re-scan already-kept events first, then keep draining the live queue.
    fn drain_find<F: Fn(&TlsEngineEvent) -> bool>(
        pred: F,
        kept: &mut Vec<TlsEngineEvent>,
        ms: u64,
    ) -> Option<TlsEngineEvent> {
        if let Some(pos) = kept.iter().position(|e| pred(e)) {
            return Some(kept.remove(pos));
        }
        drain_until(pred, kept, ms)
    }

    fn drain_leftovers() {
        // Registry hygiene between serial tests: empty the shared queue, tolerate strays.
        let started = Instant::now();
        while next_tls_event().is_some() || tls_has_pending() {
            if started.elapsed() > Duration::from_millis(500) {
                break;
            }
        }
    }

    fn wait_active_zero(ms: u64) -> bool {
        let started = Instant::now();
        while tls_active_count() > 0 {
            if started.elapsed() > Duration::from_millis(ms) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }

    fn server_opts(alpn: Vec<Vec<u8>>) -> TlsServerOptions {
        TlsServerOptions {
            cert_pem: fixture("server-cert.pem"),
            key_pem: fixture("server-key.pem"),
            ca_pem: None,
            require_client_auth: false,
            alpn_protocols: alpn,
        }
    }

    fn mtls_server_opts() -> TlsServerOptions {
        TlsServerOptions {
            cert_pem: fixture("server-cert.pem"),
            key_pem: fixture("server-key.pem"),
            ca_pem: Some(fixture("ca.pem")),
            require_client_auth: true,
            alpn_protocols: Vec::new(),
        }
    }

    fn ca_client(alpn: Vec<Vec<u8>>) -> TlsClientOptions {
        TlsClientOptions {
            ca_pem: Some(fixture("ca.pem")),
            cert_pem: None,
            key_pem: None,
            reject_unauthorized: true,
            alpn_protocols: alpn,
        }
    }

    // ------------------------------------------------------------------ ALPN plumbing
    #[test]
    fn tls_options_alpn_defaults_are_empty() {
        assert!(TlsClientOptions::default().alpn_protocols.is_empty());
        assert!(TlsServerOptions::default().alpn_protocols.is_empty());
    }

    #[test]
    fn builders_wire_alpn_protocols_into_rustls_configs() {
        let client = build_client_config(&TlsClientOptions {
            ca_pem: Some(fixture("ca.pem")),
            alpn_protocols: vec![b"h2".to_vec(), b"cz-proto".to_vec()],
            ..Default::default()
        })
        .expect("client cfg");
        assert_eq!(
            client.alpn_protocols,
            vec![b"h2".to_vec(), b"cz-proto".to_vec()],
            "client ALPN must be wired"
        );

        let server = build_server_config(&TlsServerOptions {
            cert_pem: fixture("server-cert.pem"),
            key_pem: fixture("server-key.pem"),
            ca_pem: None,
            require_client_auth: false,
            alpn_protocols: vec![b"cz-echo".to_vec()],
        })
        .expect("server cfg");
        assert_eq!(server.alpn_protocols, vec![b"cz-echo".to_vec()]);

        // Empty stays empty (no protocol advertised) for existing callers.
        let plain = build_client_config(&TlsClientOptions::default()).expect("plain cfg still builds");
        assert!(plain.alpn_protocols.is_empty());
    }

    #[test]
    fn raw_tls_socket_echo_and_orderly_close_emits_end_then_close() {
        drain_leftovers();
        let (server_id, addr, port) = create_tls_server(&server_opts(Vec::new()), "127.0.0.1", 0)
            .expect("tls server bind");
        assert!(!addr.is_empty(), "bound address reported");
        assert!(port > 0, "ephemeral port reported");
        assert_eq!(tls_active_count(), 1, "listening server counts as active");

        let cid = connect_tls(ca_client(Vec::new()), "127.0.0.1".to_string(), port, Some("localhost".to_string()))
            .expect("connect start");
        assert_eq!(tls_active_count(), 2, "client socket + listener both active");
        // Handshake events land within milliseconds (notify-driven); poll briefly instead of a
        // racy instant read, then prove liveness flipped.
        let started = Instant::now();
        while !tls_has_pending() && started.elapsed() < Duration::from_millis(3000) {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(tls_has_pending(), "handshake events queue toward JS");

        let mut kept: Vec<TlsEngineEvent> = Vec::new();
        let secure = drain_find(
            |e| matches!(e, TlsEngineEvent::SecureConnect { socket_id, .. } if *socket_id == cid),
            &mut kept,
            4000,
        )
        .expect("secureConnect event");
        match secure {
            TlsEngineEvent::SecureConnect { authorized, alpn_protocol, remote_port, .. } => {
                assert!(authorized, "verified client must report authorized=true");
                assert!(alpn_protocol.is_none(), "no ALPN requested -> none negotiated");
                assert!(remote_port > 0, "peer port recorded");
            }
            other => panic!("expected SecureConnect, got {other:?}"),
        }
        let srvconn = drain_find(
            |e| matches!(e, TlsEngineEvent::ServerConnection { server_id: sid, .. } if *sid == server_id),
            &mut kept,
            4000,
        )
        .expect("ServerConnection event");
        let ssid = match srvconn {
            TlsEngineEvent::ServerConnection { socket_id, authorized, .. } => {
                assert!(!authorized, "no client certificate -> authorized=false");
                socket_id
            }
            other => panic!("expected ServerConnection, got {other:?}"),
        };

        // Client -> server plaintext arrives decrypted as a Data event.
        assert!(tls_write(cid, b"cz-ping".to_vec()), "write accepted");
        let d1 = drain_find(
            |e| matches!(e, TlsEngineEvent::Data { socket_id, bytes } if *socket_id == ssid && &bytes[..] == b"cz-ping"),
            &mut kept,
            3000,
        )
        .expect("server-side data echo input");
        let _ = d1;
        // Server -> client reply.
        assert!(tls_write(ssid, b"cz-pong".to_vec()));
        let d2 = drain_find(
            |e| matches!(e, TlsEngineEvent::Data { socket_id, bytes } if *socket_id == cid && &bytes[..] == b"cz-pong"),
            &mut kept,
            3000,
        )
        .expect("client-side reply data");
        let _ = d2;

        // Orderly end(): close_notify + write-half FIN; BOTH sides must see End then Close.
        assert!(tls_end(cid, Vec::new()), "end accepted while open");
        drain_find(|e| matches!(e, TlsEngineEvent::End { .. }), &mut kept, 3000)
            .expect("an End event arrived (either side)");
        let mut closes = 0usize;
        let started = Instant::now();
        while closes < 2 && started.elapsed() < Duration::from_millis(3000) {
            if let Some(ev) = drain_find(|e| matches!(e, TlsEngineEvent::Close { .. }), &mut kept, 60) {
                let _ = ev;
                closes += 1;
            }
        }
        assert_eq!(closes, 2, "both sockets closed after orderly end");
        // Close the LISTENER first: `tls_active_count()` includes listening servers, so zero is
        // only reachable once both sockets have exited AND the server is closed.
        assert!(close_tls_server(server_id), "close_tls_server confirms id");
        assert!(wait_active_zero(3000), "socket workers deregistered");
        assert!(!tls_has_pending(), "drained queue reports no pending work");
        assert_eq!(tls_active_count(), 0, "everything released");
    }

    #[test]
    fn destroy_produces_close_without_data_stall() {
        drain_leftovers();
        let (server_id, _, port) = create_tls_server(&server_opts(Vec::new()), "127.0.0.1", 0)
            .expect("bind");
        let cid = connect_tls(ca_client(Vec::new()), "127.0.0.1".to_string(), port, None).expect("connect");
        let mut kept: Vec<TlsEngineEvent> = Vec::new();
        drain_find(|e| matches!(e, TlsEngineEvent::SecureConnect { socket_id, .. } if *socket_id == cid), &mut kept, 4000)
            .expect("secure");
        drain_find(|e| matches!(e, TlsEngineEvent::ServerConnection { .. }), &mut kept, 4000).expect("srvconn");

        assert!(tls_destroy(cid), "destroy live socket");
        // destroy(): Node emits 'close' (no 'end'); the wake-slot shutdown ripples both sides.
        drain_find(|e| matches!(e, TlsEngineEvent::Close { socket_id, .. } if *socket_id == cid), &mut kept, 2000)
            .expect("client Close after destroy");
        let started = Instant::now();
        while tls_active_count() > 1 && started.elapsed() < Duration::from_millis(3000) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(tls_active_count(), 1, "only the listener remains");
        assert!(close_tls_server(server_id));
        assert!(wait_active_zero(3000));
    }

    #[test]
    fn wrong_ca_client_receives_error_then_close_but_engine_survives() {
        drain_leftovers();
        let (server_id, _, port) = create_tls_server(&server_opts(Vec::new()), "127.0.0.1", 0).expect("bind");
        let bad = TlsClientOptions {
            ca_pem: Some(fixture("wrong-ca.pem")),
            cert_pem: None,
            key_pem: None,
            reject_unauthorized: true,
            alpn_protocols: Vec::new(),
        };
        let cid = connect_tls(bad, "127.0.0.1".to_string(), port, Some("localhost".to_string())).expect("start");
        let mut kept: Vec<TlsEngineEvent> = Vec::new();
        let err = drain_find(|e| matches!(e, TlsEngineEvent::Error { socket_id, .. } if *socket_id == cid), &mut kept, 4000)
            .expect("wrong CA must surface an error event");
        match err {
            TlsEngineEvent::Error { message, .. } => {
                assert!(message.contains("handshake") || message.contains("TLS"), "descriptive msg: {message}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
        drain_find(|e| matches!(e, TlsEngineEvent::Close { socket_id, .. } if *socket_id == cid), &mut kept, 2000)
            .expect("close follows handshake error");
        assert!(
            !kept.iter().any(|e| matches!(e, TlsEngineEvent::Data { .. })),
            "a rejected handshake must never deliver data"
        );

        // A following genuine client still works on the same listener.
        let good = connect_tls(ca_client(Vec::new()), "127.0.0.1".to_string(), port, Some("localhost".to_string()))
            .expect("second connect");
        drain_find(|e| matches!(e, TlsEngineEvent::SecureConnect { socket_id, .. } if *socket_id == good), &mut kept, 4000)
            .expect("second handshake succeeds");
        assert!(tls_end(good, Vec::new()));
        drain_find(|e| matches!(e, TlsEngineEvent::Close { socket_id, .. } if *socket_id == good), &mut kept, 3000).expect("second close");
        assert!(close_tls_server(server_id));
        assert!(wait_active_zero(3000), "engine fully drained after rejection");
    }

    #[test]
    fn alpn_protocols_negotiate_on_raw_engine_sockets() {
        drain_leftovers();
        let (server_id, _, port) = create_tls_server(&server_opts(vec![b"cz-echo".to_vec()]), "127.0.0.1", 0)
            .expect("bind alpn server");
        let cid = connect_tls(
            ca_client(vec![b"cz-probe".to_vec(), b"cz-echo".to_vec()]),
            "127.0.0.1".to_string(),
            port,
            Some("localhost".to_string()),
        )
        .expect("connect");
        let mut kept: Vec<TlsEngineEvent> = Vec::new();
        let secure = drain_find(|e| matches!(e, TlsEngineEvent::SecureConnect { socket_id, .. } if *socket_id == cid), &mut kept, 4000)
            .expect("secure");
        match secure {
            TlsEngineEvent::SecureConnect { alpn_protocol, .. } => {
                assert_eq!(alpn_protocol.as_deref(), Some("cz-echo"), "client negotiated server's pick");
            }
            other => panic!("expected SecureConnect, got {other:?}"),
        }
        let srvconn = drain_find(|e| matches!(e, TlsEngineEvent::ServerConnection { .. }), &mut kept, 4000).expect("srvconn");
        match srvconn {
            TlsEngineEvent::ServerConnection { alpn_protocol, socket_id, .. } => {
                assert_eq!(alpn_protocol.as_deref(), Some("cz-echo"));
                assert!(tls_end(socket_id, Vec::new()), "server-side end ok");
            }
            other => panic!("expected ServerConnection, got {other:?}"),
        }
        // The client sees the server's close_notify as End -> Close.
        drain_find(|e| matches!(e, TlsEngineEvent::End { socket_id, .. } if *socket_id == cid), &mut kept, 3000).expect("client End");
        drain_find(|e| matches!(e, TlsEngineEvent::Close { socket_id, .. } if *socket_id == cid), &mut kept, 3000).expect("client Close");
        assert!(close_tls_server(server_id));
        assert!(wait_active_zero(3000));
    }

    #[test]
    fn plaintext_garbage_on_tls_port_never_yields_connection_and_server_survives() {
        drain_leftovers();
        let (server_id, _, port) = create_tls_server(&server_opts(Vec::new()), "127.0.0.1", 0).expect("bind");

        // Raw HTTP-style bytes at a TLS port: handshake fails; no connection event may appear.
        let mut raw = TcpStream::connect(("127.0.0.1", port)).expect("raw connect");
        raw.write_all(b"GARBAGE PROBE\r\n\r\n").expect("garbage write");
        raw.shutdown(std::net::Shutdown::Write).ok();
        let started = Instant::now();
        let mut kept: Vec<TlsEngineEvent> = Vec::new();
        while started.elapsed() < Duration::from_millis(800) {
            let ev = next_tls_event();
            match ev {
                None => std::thread::sleep(Duration::from_millis(5)),
                Some(e) => {
                    assert!(
                        !matches!(e, TlsEngineEvent::ServerConnection { .. } | TlsEngineEvent::Data { .. }),
                        "plaintext garbage must never produce a connection/data event"
                    );
                    kept.push(e);
                }
            }
        }

        // The listener still serves a genuine TLS client afterwards.
        let good = connect_tls(ca_client(Vec::new()), "127.0.0.1".to_string(), port, Some("localhost".to_string()))
            .expect("post-garbage connect");
        drain_find(|e| matches!(e, TlsEngineEvent::SecureConnect { socket_id, .. } if *socket_id == good), &mut kept, 4000)
            .expect("server survived the attack");
        assert!(tls_end(good, Vec::new()));
        drain_find(|e| matches!(e, TlsEngineEvent::Close { .. }), &mut kept, 3000).expect("close after echo teardown");
        assert!(close_tls_server(server_id));
        assert!(wait_active_zero(4000), "stalled/failed workers must not linger");
    }

    #[test]
    fn mtls_engine_socket_reports_authorized_true_with_valid_client_cert() {
        drain_leftovers();
        let (server_id, _, port) = create_tls_server(&mtls_server_opts(), "127.0.0.1", 0).expect("mtls bind");
        let with_cert = TlsClientOptions {
            ca_pem: Some(fixture("ca.pem")),
            cert_pem: Some(fixture("client-cert.pem")),
            key_pem: Some(fixture("client-key.pem")),
            reject_unauthorized: true,
            alpn_protocols: Vec::new(),
        };
        let cid = connect_tls(with_cert, "127.0.0.1".to_string(), port, Some("localhost".to_string())).expect("mtls connect");
        let mut kept: Vec<TlsEngineEvent> = Vec::new();
        drain_find(|e| matches!(e, TlsEngineEvent::SecureConnect { socket_id, .. } if *socket_id == cid), &mut kept, 4000)
            .expect("mTLS secureConnect");
        let srv = drain_find(|e| matches!(e, TlsEngineEvent::ServerConnection { .. }), &mut kept, 4000).expect("mTLS srvconn");
        match srv {
            TlsEngineEvent::ServerConnection { authorized, socket_id, .. } => {
                assert!(authorized, "verified client certificate -> authorized=true");
                assert!(tls_write(socket_id, b"srv-hi".to_vec()));
            }
            other => panic!("expected ServerConnection, got {other:?}"),
        }
        drain_find(|e| matches!(e, TlsEngineEvent::Data { bytes, .. } if &bytes[..] == b"srv-hi"), &mut kept, 3000)
            .expect("mTLS data reached the client");
        assert!(tls_end(cid, b"bye".to_vec()));
        // Drain both sides' terminal events so the registry returns to zero.
        let started = Instant::now();
        while tls_active_count() > 1 && started.elapsed() < Duration::from_millis(3000) {
            next_tls_event();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(close_tls_server(server_id));
        assert!(wait_active_zero(3000));
    }

    #[test]
    fn pending_liveness_flips_with_queued_events() {
        drain_leftovers();
        assert!(!tls_has_pending(), "idle queue is not pending");
        let (server_id, _, port) = create_tls_server(&server_opts(Vec::new()), "127.0.0.1", 0).expect("bind");
        let cid = connect_tls(ca_client(Vec::new()), "127.0.0.1".to_string(), port, None).expect("connect");
        // Handshake events land in the queue: pending must be true before draining.
        let started = Instant::now();
        while !tls_has_pending() && started.elapsed() < Duration::from_millis(3000) {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(tls_has_pending(), "queued events flip liveness");
        let mut drained = 0usize;
        let started = Instant::now();
        while drained < 2 && started.elapsed() < Duration::from_millis(3000) {
            if next_tls_event().is_some() {
                drained += 1;
            } else {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        assert!(tls_end(cid, Vec::new()));
        assert!(close_tls_server(server_id));
        drain_leftovers();
        assert!(wait_active_zero(3000));
        assert!(!tls_has_pending(), "fully drained -> not pending");
    }

    #[test]
    fn tls_worker_stack_constant_matches_spec() {
        assert_eq!(TLS_WORKER_STACK_BYTES, 256 * 1024);
        assert_eq!(SERVER_HANDSHAKE_TIMEOUT, Duration::from_secs(5));
    }
}
