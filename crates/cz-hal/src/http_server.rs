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

use std::collections::{HashMap, VecDeque};
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Sender};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::http::{
    chunk_frame, error_response, serialize_response, serialize_stream_head, LAST_CHUNK_FRAME,
    ParsedRequest, RequestParser, ResponseFrame,
};

/// Abstract per-connection byte stream: a plain TCP socket OR a TLS-wrapped one.
///
/// The engine drives every request/response over this trait, so an external `StreamAcceptor`
/// (e.g. `cz-net`'s rustls adapter) can transparently upgrade accepted connections while
/// `cz-hal` itself stays strictly `libc`-only and knows nothing about TLS.
pub trait ConnStream: Read + Write + Send {
    /// Enable `TCP_NODELAY` on the underlying socket.
    fn set_nodelay(&self) -> std::io::Result<()>;
    /// Bounded read timeout (`None` clears it). Used to bound TLS handshakes.
    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()>;
    /// Bounded write timeout (`None` clears it).
    fn set_write_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()>;
    /// Best-effort full close of both directions. TLS implementations should emit their
    /// `close_notify` alert through the session BEFORE shutting the raw socket down.
    fn shutdown_both(&mut self);
    /// Peer address as a plain string ("127.0.0.1").
    fn peer_addr_str(&self) -> String;
    /// Raw OS socket fd for the non-blocking liveness probe. `POLLHUP`/`MSG_PEEK` observe
    /// TCP-level FIN/RST regardless of TLS framing, so the probe stays valid on TLS streams.
    fn raw_fd(&self) -> std::os::fd::RawFd;
    /// Local socket address when the OS can report it.
    fn local_addr(&self) -> std::io::Result<std::net::SocketAddr>;
    /// `true` when bytes travel through a TLS session (surfaces as `req.socket.encrypted`).
    fn is_encrypted(&self) -> bool;
}

impl ConnStream for TcpStream {
    fn set_nodelay(&self) -> std::io::Result<()> {
        TcpStream::set_nodelay(self, true)
    }
    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        TcpStream::set_read_timeout(self, timeout)
    }
    fn set_write_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        TcpStream::set_write_timeout(self, timeout)
    }
    fn shutdown_both(&mut self) {
        self.shutdown(std::net::Shutdown::Both).ok();
    }
    fn peer_addr_str(&self) -> String {
        self.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default()
    }
    fn raw_fd(&self) -> std::os::fd::RawFd {
        AsRawFd::as_raw_fd(self)
    }
    fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        TcpStream::local_addr(self)
    }
    fn is_encrypted(&self) -> bool {
        false
    }
}

/// Phase 8B: hook that upgrades a freshly accepted `TcpStream` into a [`ConnStream`] (TLS or otherwise).
/// It runs on the per-connection WORKER thread — never inside `accept_loop` — so a client that
/// stalls mid-handshake can not block other incoming connections.
pub type StreamAcceptor =
    Arc<dyn Fn(TcpStream) -> std::io::Result<Box<dyn ConnStream>> + Send + Sync>;

// Delegating impl so a `Box<dyn ConnStream>` itself acts as a `ConnStream`: the boxed object's
// vtable dispatches to the concrete inner stream (plain TCP or TLS), so `&mut Box<dyn ConnStream>`
// coerces cleanly to `&mut dyn ConnStream`. `Read`/`Write` on the Box come from `alloc`'s own
// blanket impls over the trait's supertraits — declaring them here would conflict (E0119).
impl ConnStream for Box<dyn ConnStream> {
    fn set_nodelay(&self) -> std::io::Result<()> {
        (**self).set_nodelay()
    }
    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        (**self).set_read_timeout(timeout)
    }
    fn set_write_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        (**self).set_write_timeout(timeout)
    }
    fn shutdown_both(&mut self) {
        (**self).shutdown_both()
    }
    fn peer_addr_str(&self) -> String {
        (**self).peer_addr_str()
    }
    fn raw_fd(&self) -> std::os::fd::RawFd {
        (**self).raw_fd()
    }
    fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        (**self).local_addr()
    }
    fn is_encrypted(&self) -> bool {
        (**self).is_encrypted()
    }
}

/// Read size used while pulling bytes off a connection.
const READ_CHUNK: usize = 8 * 1024;
/// Poll slice used by the accept loop (bounded stop latency, low idle CPU).
const ACCEPT_POLL_MS: i32 = 20;
/// Wall-clock budget for JavaScript to answer one request; a hung handler is answered `500` and
/// the socket is closed so clients never stall indefinitely.
pub const DEFAULT_REPLY_TIMEOUT: Duration = Duration::from_secs(30);
/// Slice used while parked on a reply channel: short enough to notice peer closes (via poll) and
/// stop flags quickly, long enough to keep idle CPU near zero while streaming or waiting.
const REPLY_POLL_SLICE: Duration = Duration::from_millis(10);

/// H36 condvar mailbox replacing the per-connection reply channel (Genius instant-wake idiom
/// ported from the event-loop idle pause): replies wake the connection thread IMMEDIATELY and
/// idle connections wait on a condvar instead of polling 10 ms slices. mpsc parity is kept via
/// LEASE COUNTING: `ReplySender` clones/drops flip `alive` exactly when the last sender handle
/// dies (JS GC of an unanswered response => prompt Closed like Disconnected), and the rx guard
/// closes on thread exit. ReplyOutcome mirrors Ok/Disconnected/Timeout.
#[derive(Debug)]
pub struct ReplyBox {
    queue: Mutex<VecDeque<ResponseAction>>,
    cv: Condvar,
    alive: AtomicU64, // 0 once ALL sender leases drop OR the connection thread exits
    leases: AtomicU64, // live ReplySender handles
}

#[derive(Debug, Clone)]
pub enum ReplyOutcome {
    Action(ResponseAction),
    Closed,
    Timeout,
}

/// mpsc-sender-parity handle handed to the JS side and registry.
#[derive(Debug)]
pub struct ReplySender {
    box_ref: Arc<ReplyBox>,
}

impl Clone for ReplySender {
    fn clone(&self) -> Self {
        self.box_ref.leases.fetch_add(1, Ordering::AcqRel);
        Self { box_ref: self.box_ref.clone() }
    }
}

impl Drop for ReplySender {
    fn drop(&mut self) {
        if self.box_ref.leases.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.box_ref.alive.store(0, Ordering::Release);
            self.box_ref.cv.notify_all(); // prompt wake, mirroring mpsc Disconnected
        }
    }
}

impl ReplySender {
    /// mpsc-compatible signature: Err when the connection thread is gone or unclaimed.
    pub fn send(&self, action: ResponseAction) -> Result<(), ResponseAction> {
        if self.box_ref.alive.load(Ordering::Acquire) == 0 {
            return Err(action);
        }
        self.box_ref.queue.lock().expect("reply box queue").push_back(action);
        self.box_ref.cv.notify_all();
        Ok(())
    }
}

impl ReplyBox {
    fn new() -> (Arc<Self>, ReplySender) {
        let box_ref = Arc::new(Self {
            queue: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
            alive: AtomicU64::new(1),
            leases: AtomicU64::new(1),
        });
        (box_ref.clone(), ReplySender { box_ref })
    }

    pub fn recv_timeout(&self, dur: Duration) -> ReplyOutcome {
        let mut q = self.queue.lock().expect("reply box queue lock");
        let start = Instant::now();
        loop {
            if let Some(a) = q.pop_front() {
                return ReplyOutcome::Action(a);
            }
            if self.alive.load(Ordering::Acquire) == 0 {
                return ReplyOutcome::Closed;
            }
            let waited = self.cv.wait_timeout(q, dur).expect("reply box wait");
            q = waited.0;
            if waited.1.timed_out() || Instant::now() - start >= dur {
                if self.alive.load(Ordering::Acquire) != 0 {
                    return ReplyOutcome::Timeout;
                }
                if q.is_empty() {
                    return ReplyOutcome::Closed;
                }
                return ReplyOutcome::Action(q.pop_front().expect("rechecked queue"));
            }
        }
    }
}

/// RAII: marks the mailbox closed when the connection thread exits (JS replies then see Err,
/// exactly like mpsc Disconnected), waking any waiter.
struct ReplyRxGuard {
    box_ref: Arc<ReplyBox>,
}

impl Drop for ReplyRxGuard {
    fn drop(&mut self) {
        self.box_ref.alive.store(0, Ordering::Release);
        self.box_ref.cv.notify_all();
    }
}

static NEXT_CONN_ID: AtomicU64 = AtomicU64::new(1);
static ACTIVE_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);
static EVENTS_QUEUED: AtomicUsize = AtomicUsize::new(0);
/// Lightweight diagnostic counters so streaming/disconnect paths are objectively observable:
/// `marks` = disconnect notifications recorded, `eof_detects` = peer FIN/RST seen by poll/peek,
/// `chan_gone_exits` = reader threads that left because the JS reply channel died.
static DIAG_MARKS: AtomicUsize = AtomicUsize::new(0);
static DIAG_EOF_DETECTS: AtomicUsize = AtomicUsize::new(0);
static DIAG_CHAN_GONE_EXITS: AtomicUsize = AtomicUsize::new(0);

/// Snapshot of streaming diagnostic counters `(marks, eof_detects, chan_gone_exits)`.
pub fn stream_diagnostics() -> (usize, usize, usize) {
    (
        DIAG_MARKS.load(Ordering::SeqCst),
        DIAG_EOF_DETECTS.load(Ordering::SeqCst),
        DIAG_CHAN_GONE_EXITS.load(Ordering::SeqCst),
    )
}

/// Count of live connection reader threads (for diagnostics/tests).
pub fn active_connections() -> usize {
    ACTIVE_CONNECTIONS.load(Ordering::SeqCst)
}

/// Total requests enqueued toward the JS event loop since process start. Incremented *before*
/// the send so a consumer can detect queued-but-not-yet-drained work without peeking a channel.
pub fn events_queued() -> usize {
    EVENTS_QUEUED.load(Ordering::SeqCst)
}

// ---------------------------------------------------------------------------
// Instant event-loop wakeup (Final Boss: replaces fixed idle sleeps)
// ---------------------------------------------------------------------------

static WAKE: static_waker::Waker = static_waker::Waker::new();

mod static_waker {
    use std::sync::{Condvar, Mutex};

    pub struct Waker {
        // A plain flag + condvar: notify sets the token, wait consumes it or times out.
        mu: Mutex<bool>,
        cv: Condvar,
    }

    impl Waker {
        pub const fn new() -> Self {
            Self { mu: Mutex::new(false), cv: Condvar::new() }
        }
        pub fn notify(&self) {
            let mut g = self.mu.lock().expect("wake lock");
            *g = true;
            self.cv.notify_all();
        }
        /// Wait up to `max`; returns true if a notification arrived earlier.
        pub fn wait(&self, max: std::time::Duration) -> bool {
            let mut g = self.mu.lock().expect("wake lock");
            if !*g {
                let (g2, _timed_out) = self.cv.wait_timeout(g, max).expect("wake wait");
                g = g2;
            }
            let woke = *g;
            *g = false;
            woke
        }
    }
}

/// Wake the JS event loop's idle pause immediately (called when a request is queued or a
/// disconnect/timer-relevant state change happens).
pub fn notify_event_loop() {
    WAKE.notify();
}

/// Block the caller up to `max` until `notify_event_loop` fires; returns early on wake.
pub fn idle_wait(max: Duration) -> bool {
    WAKE.wait(max)
}

// ---------------------------------------------------------------------------
// Client-disconnect notifications (Phase 2.5)
// ---------------------------------------------------------------------------

fn disconnect_state() -> &'static Mutex<VecDeque<u64>> {
    static Q: OnceLock<Mutex<VecDeque<u64>>> = OnceLock::new();
    Q.get_or_init(|| Mutex::new(VecDeque::new()))
}

fn closed_conn_ids() -> &'static Mutex<HashMap<u64, ()>> {
    static S: OnceLock<Mutex<HashMap<u64, ()>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Signal that a connection's socket failed or its peer closed: the conn id lands on the queue
/// the JS event loop drains to fire `"close"` listeners, and is remembered as closed so later
/// stream writes report `false` promptly.
/// Public wrapper used by `node:http` server shutdown to force-terminate live streams.
pub fn mark_client_closed_public(conn_id: u64) {
    mark_client_closed(conn_id);
}

fn mark_client_closed(conn_id: u64) {
    DIAG_MARKS.fetch_add(1, Ordering::SeqCst);
    disconnect_state().lock().expect("disconnect queue lock").push_back(conn_id);
    notify_event_loop(); // disconnects must reach JS instantly, not at the next idle slice
    let mut set = closed_conn_ids().lock().expect("closed set lock");
    if !set.contains_key(&conn_id) {
        // Bound the set even if nobody ever drains: drop older entries first (they only matter
        // to already-fired JS links, which keep their own `_closed` flag).
        while set.len() > 2048 {
            if let Some((k, _)) = set.iter().next() {
                let k = *k;
                set.remove(&k);
            } else {
                break;
            }
        }
        set.insert(conn_id, ());
    }
}

/// True once this connection was reported closed (write failure / EOF / server-forced end).
pub fn client_closed(conn_id: u64) -> bool {
    closed_conn_ids().lock().expect("closed set lock").contains_key(&conn_id)
}

/// Pull every pending disconnect notification for THIS round of the event loop and forget the
/// ids (memory stays bounded; JS link objects keep their own closed flags after firing).
pub fn take_disconnect_events() -> Vec<u64> {
    let mut q = disconnect_state().lock().expect("disconnect queue lock");
    let ids: Vec<u64> = q.drain(..).collect();
    let mut set = closed_conn_ids().lock().expect("closed set lock");
    for id in &ids {
        set.remove(id);
    }
    ids
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
    /// Phase 8B: `true` when this connection was upgraded by a TLS `StreamAcceptor`; the JS glue
    /// surfaces it as `req.socket.encrypted`.
    pub encrypted: bool,
    /// Reply channel for THIS request. Phase 2 delivered one buffered frame; Phase 2.5 lets the
    /// channel carry a *sequence*: `StreamHead` -> many `StreamChunk`s -> `StreamEnd`, each chunk
    /// written and flushed onto the socket immediately, or a single `Buffered` frame (Phase 2).
    pub reply: ReplySender,
}

/// What the JS side wants the connection thread to do with the socket for one request.
#[derive(Debug, Clone)]
pub enum ResponseAction {
    /// Complete buffered response (Phase 2 path): headers + `Content-Length` body in one write.
    Buffered(ReplyFrame),
    /// Streaming mode: emit the status line + headers NOW with `Transfer-Encoding: chunked`
    /// (auto-added; any caller `Content-Length` is dropped) and flush, so the client starts
    /// receiving before the body exists.
    StreamHead {
        status: u16,
        reason: Option<String>,
        headers: Vec<(String, String)>,
        keep_alive: bool,
    },
    /// One `<hex>\r\n<bytes>\r\n` chunk frame, written and flushed immediately.
    StreamChunk(Vec<u8>),
    /// Terminating `0\r\n\r\n` chunk, flushed; then honor `keep_alive` for the socket.
    StreamEnd { keep_alive: bool },
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
    spawn_listener_impl(host, port, server_id, tx, reply_timeout, None)
}

/// Node-compatible alias of [`spawn_listener`] (plain TCP, default reply timeout).
pub fn start_listener(
    host: &str,
    port: u16,
    server_id: u64,
    tx: Sender<ConnectionEvent>,
) -> std::io::Result<Listener> {
    start_listener_with_acceptor(host, port, server_id, tx, None)
}

/// Phase 8B: bind + accept thread with an optional per-connection [`StreamAcceptor`]. The
/// acceptor (e.g. a rustls TLS handshake) executes on the spawned per-connection worker thread,
/// never in `accept_loop`, so a stalled client handshake cannot block other connections.
pub fn start_listener_with_acceptor(
    host: &str,
    port: u16,
    server_id: u64,
    tx: Sender<ConnectionEvent>,
    acceptor: Option<StreamAcceptor>,
) -> std::io::Result<Listener> {
    spawn_listener_impl(host, port, server_id, tx, DEFAULT_REPLY_TIMEOUT, acceptor)
}

fn spawn_listener_impl(
    host: &str,
    port: u16,
    server_id: u64,
    tx: Sender<ConnectionEvent>,
    reply_timeout: Duration,
    acceptor: Option<StreamAcceptor>,
) -> std::io::Result<Listener> {
    let listener = bind_socket(host, port)?;
    let bound = listener.local_addr()?;
    listener.set_nonblocking(true)?;
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let acceptor_thread = acceptor.clone();
    let handle = std::thread::Builder::new()
        .name(format!("cz-http-accept-{server_id}"))
        .spawn(move || accept_loop(listener, stop_thread, tx, server_id, reply_timeout, acceptor_thread))?;
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
    acceptor: Option<StreamAcceptor>,
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
                    spawn_connection(
                        stream,
                        peer,
                        server_id,
                        conn_id,
                        tx.clone(),
                        reply_timeout,
                        acceptor.clone(),
                    );
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => break,
                Err(_) => return,
            }
        }
    }
}

/// Detached per-connection reader thread: parse -> (error answer | forward + await reply).
fn spawn_connection(
    stream: TcpStream,
    peer: std::net::SocketAddr,
    server_id: u64,
    conn_id: u64,
    tx: Sender<ConnectionEvent>,
    reply_timeout: Duration,
    acceptor: Option<StreamAcceptor>,
) {
    ACTIVE_CONNECTIONS.fetch_add(1, Ordering::SeqCst);
    std::thread::Builder::new()
        .name(format!("cz-http-conn-{conn_id}"))
        // Intentionally detached: `server.close()` may leave clients mid-flight, and the process
        // exits once every server closed; stray threads must not block shutdown.
        .spawn(move || {
            // Decrement ACTIVE_CONNECTIONS when this thread ends (also on early `return`s).
            let _guard = ConnectionGuard;
            // Phase 8B: materialize the (possibly TLS-wrapped) stream HERE, inside the worker
            // thread. A stalled or hostile handshake only ever affects this one connection.
            let mut conn: Box<dyn ConnStream> = match &acceptor {
                Some(make_stream) => match make_stream(stream) {
                    Ok(conn) => conn,
                    Err(_) => return, // handshake failed: drop THIS socket only, no crash
                },
                None => Box::new(stream),
            };
            conn.set_nodelay().ok();
            let local = conn.local_addr().unwrap_or(peer);
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
                            if write_all_eof_free(&mut conn, crate::http::continue_frame()).is_err() {
                                return;
                            }
                        }
                        Ok(crate::http::ParseStep::NeedMore) => {
                            match conn.read(&mut buf) {
                                Ok(0) => return, // client closed the connection
                                Ok(n) => parser.push(&buf[..n]),
                                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                                Err(_) => return,
                            }
                        }
                        Err(err) => {
                            // Malformed (400) / oversized (413): answer, then drop only THIS socket.
                            let _ = write_all_eof_free(&mut conn, error_response(&err));
                            conn.shutdown_both();
                            return;
                        }
                    }
                };

                let keep_alive = req.keep_alive;
                let (reply_box, reply_tx) = ReplyBox::new();
                EVENTS_QUEUED.fetch_add(1, Ordering::SeqCst);
                notify_event_loop();
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
                        encrypted: conn.is_encrypted(),
                        reply: reply_tx,
                    })
                    .is_err()
                {
                    // Consumer gone (event loop exited): close quietly.
                    conn.shutdown_both();
                    return;
                }

                // Phase 2.5: the reply channel may carry a *sequence* of actions. Every streamed
                // chunk is written + flushed immediately; socket failures and peer EOF are
                // reported back to the JS event loop as `"close"` notifications.
                let mut streaming = false;
                let mut deadline = Instant::now() + reply_timeout;
                // Closing this thread marks the mailbox closed (JS replies get Err like mpsc).
                let _rx_guard = ReplyRxGuard { box_ref: reply_box.clone() };
                'reply: loop {
                    match reply_box.recv_timeout(REPLY_POLL_SLICE) {
                        ReplyOutcome::Action(action) => {
                            // Fresh budget for the NEXT action on this (possibly long-lived) stream.
                            deadline = Instant::now() + reply_timeout;
                            match action {
                                ResponseAction::Buffered(frame) => {
                                    let bytes = frame.encode();
                                    if write_all_eof_free(&mut conn, bytes).is_err() {
                                        mark_client_closed(conn_id);
                                        conn.shutdown_both();
                                        return;
                                    }
                                    if !frame.keep_alive {
                                        conn.shutdown_both();
                                        return;
                                    }
                                    break 'reply;
                                }
                                ResponseAction::StreamHead {
                                    status,
                                    reason,
                                    headers,
                                    keep_alive,
                                } => {
                                    let bytes =
                                        serialize_stream_head(status, reason.as_deref(), &headers, keep_alive);
                                    if write_all_eof_free(&mut conn, bytes).is_err() {
                                        mark_client_closed(conn_id);
                                        conn.shutdown_both();
                                        return;
                                    }
                                    streaming = true;
                                }
                                ResponseAction::StreamChunk(data) => {
                                    if !streaming || data.is_empty() {
                                        continue 'reply; // nothing to frame
                                    }
                                    if write_all_eof_free(&mut conn, chunk_frame(&data)).is_err() {
                                        mark_client_closed(conn_id);
                                        conn.shutdown_both();
                                        return;
                                    }
                                    // Rapid pushes must not starve EOF detection: probe the peer
                                    // after each flush (non-blocking), so a hung-up client fires
                                    // "close" within milliseconds even while data keeps flowing.
                                    if peer_closed(conn.raw_fd()) {
                                        mark_client_closed(conn_id);
                                        conn.shutdown_both();
                                        return;
                                    }
                                }
                                ResponseAction::StreamEnd { keep_alive } => {
                                    if streaming {
                                        if write_all_eof_free(&mut conn, LAST_CHUNK_FRAME.to_vec()).is_err() {
                                            mark_client_closed(conn_id);
                                            conn.shutdown_both();
                                            return;
                                        }
                                        // `streaming` ends with this request; the per-request
                                        // loop below re-initializes it for the next one.
                                    }
                                    if !keep_alive {
                                        conn.shutdown_both();
                                        return;
                                    }
                                    break 'reply;
                                }
                            }
                        }
                        ReplyOutcome::Closed => {
                            DIAG_CHAN_GONE_EXITS.fetch_add(1, Ordering::SeqCst);
                            // JS dropped the response object without finalizing it.
                            if !streaming {
                                let body = ResponseFrame::error(500, "request handler did not respond", false);
                                let _ = write_all_eof_free(&mut conn, body.encode());
                            }
                            // Headers already went out mid-stream: just close that socket.
                            conn.shutdown_both();
                            return;
                        }
                        ReplyOutcome::Timeout => {
                            // Peer gone (FIN/RST)? Notify so JS can clear intervals right away.
                            if peer_closed(conn.raw_fd()) {
                                mark_client_closed(conn_id);
                                conn.shutdown_both();
                                return;
                            }
                            if Instant::now() >= deadline {
                                if streaming {
                                    // A wedged streamer must not hold the socket hostage: end it
                                    // and fire the disconnect path so JS cleans up.
                                    mark_client_closed(conn_id);
                                } else {
                                    let body =
                                        ResponseFrame::error(500, "request handler did not respond", false);
                                    let _ = write_all_eof_free(&mut conn, body.encode());
                                }
                                conn.shutdown_both();
                                return;
                            }
                        }
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

fn write_all_eof_free(stream: &mut dyn ConnStream, bytes: Vec<u8>) -> std::io::Result<()> {
    // H27: offset advance instead of drain(..n) — drain memmoves the shrinking tail on every
    // partial write (O(remaining)); an index is O(1). Byte-for-byte identical wire semantics.
    let len = bytes.len();
    let mut off = 0usize;
    while off < len {
        match stream.write(&bytes[off..]) {
            Ok(0) => return Err(std::io::Error::new(ErrorKind::BrokenPipe, "socket closed")),
            Ok(n) => {
                // Advance past the accepted prefix (partial writes are fully handled).
                off += n;
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    stream.flush()
}

/// Non-blocking liveness probe: POLLHUP/POLLERR, or a readable socket whose `MSG_PEEK` returns
/// 0 bytes, means the peer closed. Incoming request bytes (keep-alive pipelining) are NOT a
/// close signal, so a waiting connection never mistakes them for a disconnect.
fn peer_closed(fd: std::os::fd::RawFd) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN | libc::POLLHUP | libc::POLLERR,
        revents: 0,
    };
    let rc = unsafe { libc::poll(&mut pfd, 1, 0) };
    if rc <= 0 {
        return false; // no event (or EINTR): keep waiting on the reply channel
    }
    if pfd.revents & (libc::POLLHUP | libc::POLLERR) != 0 {
        return true;
    }
    if pfd.revents & libc::POLLIN != 0 {
        let mut probe = [0u8; 1];
        let n = unsafe {
            libc::recv(
                fd,
                probe.as_mut_ptr() as *mut libc::c_void,
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        if n == 0 {
            DIAG_EOF_DETECTS.fetch_add(1, Ordering::SeqCst);
            return true; // graceful FIN/EOF from the client
        }
        if n < 0 {
            // A hard reset (ECONNRESET/EPIPE) is also a disconnect: peers that drop with unread
            // received data trigger RST instead of an orderly FIN.
            let errno = std::io::Error::last_os_error();
            return matches!(errno.kind(), ErrorKind::ConnectionReset | ErrorKind::BrokenPipe);
        }
        return false; // readable request bytes (keep-alive pipelining) are NOT a close signal
    }
    false
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
    use std::sync::mpsc::channel;
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

    /// Disconnect notifications live in process-wide statics, so socket-level engine tests must
    /// serialize (same pattern as the cz-modules http tests).
    fn engine_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Poll `take_disconnect_events` until `id` shows up (bounded wall time).
    fn wait_for_disconnect(id: u64, budget: Duration) -> bool {
        let t0 = Instant::now();
        loop {
            let ids = take_disconnect_events();
            if ids.contains(&id) {
                return true;
            }
            if t0.elapsed() > budget {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn ephemeral_bind_reports_real_address_and_port_plus_answered_request() {
        let _lock = engine_test_lock();
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
                .send(ResponseAction::Buffered(ReplyFrame {
                    status: 200,
                    reason: None,
                    headers: vec![("content-type".into(), "application/json".into())],
                    body: b"{\"ok\":true}".to_vec(),
                    keep_alive: ev.keep_alive,
                }))
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
        let _lock = engine_test_lock();
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
                    .send(ResponseAction::Buffered(ReplyFrame::new(
                        if i == 0 { 200 } else { 201 },
                        body.into_bytes(),
                        ev.keep_alive,
                    )))
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
        let _lock = engine_test_lock();
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
        ev.reply
            .send(ResponseAction::Buffered(ReplyFrame::new(200, b"still-up".to_vec(), false)))
            .unwrap();
        let mut buf = String::new();
        oksock.read_to_string(&mut buf).unwrap();
        assert!(buf.starts_with("HTTP/1.1 200 OK\r\n"), "post-400 response {buf:?}");
        assert!(stop_listener(&mut listener));
    }

    #[test]
    fn oversized_body_gets_413_and_closes_only_that_connection() {
        let _lock = engine_test_lock();
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
        ev.reply
            .send(ResponseAction::Buffered(ReplyFrame::new(200, b"fine".to_vec(), false)))
            .unwrap();
        let mut text = String::new();
        fine.read_to_string(&mut text).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "{text:?}");
        assert!(stop_listener(&mut listener));
    }

    #[test]
    fn eight_concurrent_clients_are_all_served_without_dropping() {
        let _lock = engine_test_lock();
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
                            .send(ResponseAction::Buffered(ReplyFrame::new(
                                200,
                                body.into_bytes(),
                                ev.keep_alive,
                            )))
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
        let _lock = engine_test_lock();
        let (tx, rx) = channel::<ConnectionEvent>();
        let mut listener = spawn_listener("0.0.0.0", 0, 31, tx).expect("bind");
        let consumer = std::thread::spawn(move || {
            let ev = rx.recv_timeout(Duration::from_secs(5)).expect("chunked event");
            assert_eq!(ev.request.head.header("transfer-encoding").as_deref(), Some("chunked"));
            assert_eq!(ev.request.body, b"{\"brightness\":65}".to_vec());
            ev.reply
                .send(ResponseAction::Buffered(ReplyFrame {
                    status: 200,
                    reason: None,
                    headers: vec![("content-type".into(), "application/json".into())],
                    body: b"{\"brightness\":65}".to_vec(),
                    keep_alive: false,
                }))
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
        let _lock = engine_test_lock();
        // Bind, stop, then re-bind the SAME port (proves the socket was really released).
        let (tx, rx) = channel::<ConnectionEvent>();
        let mut listener = spawn_listener("127.0.0.1", 0, 41, tx).expect("bind first");
        let port = listener.port;
        let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect before stop");
        s.write_all(b"GET /pre HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
        let ev = rx.recv_timeout(Duration::from_secs(5)).expect("pre-stop event");
        ev.reply
            .send(ResponseAction::Buffered(ReplyFrame::new(200, b"pre".to_vec(), false)))
            .unwrap();
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
        let _lock = engine_test_lock();
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
        let _lock = engine_test_lock();
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
        let _lock = engine_test_lock();
        let (tx, rx) = channel::<ConnectionEvent>();
        let mut listener = spawn_listener("127.0.0.1", 0, 47, tx).expect("bind");
        let consumer = std::thread::spawn(move || {
            let ev = rx.recv_timeout(Duration::from_secs(5)).expect("expect-continue event");
            assert_eq!(ev.request.head.method, "PUT");
            assert_eq!(ev.request.body, b"payload-here".to_vec());
            ev.reply
                .send(ResponseAction::Buffered(ReplyFrame::new(201, b"stored".to_vec(), false)))
                .unwrap();
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

    #[test]
    fn stream_head_flushes_before_chunks_then_terminator_and_keep_alive_next_request() {
        let _lock = engine_test_lock();
        let (tx, rx) = channel::<ConnectionEvent>();
        let mut listener = spawn_listener("127.0.0.1", 0, 61, tx).expect("bind");
        // Handshake: the consumer emits ONLY the stream head first; the client must observe it
        // before the body chunks are pushed at all (proof of a real flush, not one buffered write).
        let (head_seen_tx, head_seen_rx) = channel::<()>();
        let consumer = std::thread::spawn(move || {
            let ev = rx.recv_timeout(Duration::from_secs(5)).expect("sse event");
            assert!(ev.keep_alive, "keep-alive GET should own a long-lived stream");
            ev.reply
                .send(ResponseAction::StreamHead {
                    status: 200,
                    reason: None,
                    headers: vec![
                        ("content-type".into(), "text/event-stream; charset=utf-8".into()),
                        ("cache-control".into(), "no-cache".into()),
                    ],
                    keep_alive: true,
                })
                .expect("stream head");
            head_seen_rx.recv_timeout(Duration::from_secs(5)).expect("client saw head");
            ev.reply
                .send(ResponseAction::StreamChunk(b"event: t\ndata: 1\n\n".to_vec()))
                .expect("chunk 1"); // 18 bytes -> hex 12
            ev.reply
                .send(ResponseAction::StreamChunk(b"data: 2\n\n".to_vec()))
                .expect("chunk 2"); // 9 bytes -> hex 9
            ev.reply.send(ResponseAction::StreamEnd { keep_alive: true }).expect("stream end");
            // Keep-alive: the same socket then serves a *buffered* Phase 2 request.
            let stats = rx.recv_timeout(Duration::from_secs(5)).expect("stats request");
            assert_eq!(stats.request.head.target, "/stats");
            stats
                .reply
                .send(ResponseAction::Buffered(ReplyFrame::new(200, b"done".to_vec(), false)))
                .expect("buffered follow-up");
        });

        let mut sock = TcpStream::connect(("127.0.0.1", listener.port)).expect("connect sse");
        sock.set_nodelay(true).ok();
        sock.write_all(b"GET /events HTTP/1.1\r\nHost: z\r\nConnection: keep-alive\r\n\r\n").unwrap();
        // 1. Read just the header section: it must arrive while ZERO chunks exist yet.
        let mut acc = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            if find_sub(&acc, b"\r\n\r\n").is_some() {
                break;
            }
            let n = sock.read(&mut tmp).expect("read stream head");
            assert!(n > 0, "stream head never arrived");
            acc.extend_from_slice(&tmp[..n]);
        }
        let head_end = find_sub(&acc, b"\r\n\r\n").unwrap();
        let head = String::from_utf8_lossy(&acc[..head_end]).to_string();
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "stream head {head:?}");
        // Header field names are case-insensitive on the wire; compare parsed values.
        assert_eq!(
            header_of(&head, "content-type").as_deref(),
            Some("text/event-stream; charset=utf-8"),
            "head {head:?}"
        );
        assert_eq!(header_of(&head, "transfer-encoding").as_deref(), Some("chunked"), "head {head:?}");
        assert!(header_of(&head, "content-length").is_none(), "no length while streaming: head {head:?}");
        assert_eq!(header_of(&head, "connection").as_deref(), Some("keep-alive"), "head {head:?}");
        // Nothing beyond the header may have been written yet (chunks only follow the handshake).
        assert_eq!(acc.len(), head_end + 4, "premature body bytes while head-only: {acc:?}");
        head_seen_tx.send(()).expect("handshake");

        // 2. Decode the two live chunk frames and the terminator from this point on.
        let mut cursor = head_end + 4;
        let mut stream_body = String::new();
        let mut chunks = 0usize;
        let mut terminated = false;
        while !terminated {
            loop {
                if let Some(nl) = find_sub(&acc[cursor..], b"\r\n") {
                    let line = String::from_utf8_lossy(&acc[cursor..cursor + nl]).to_string();
                    let size = usize::from_str_radix(line.trim(), 16).expect("chunk hex size line");
                    cursor += nl + 2;
                    if size == 0 {
                        // Consume the empty trailer line too; `0\r\n\r\n` fully framed.
                        while acc.len() < cursor + 2 {
                            let n = sock.read(&mut tmp).expect("read terminator CRLF");
                            assert!(n > 0, "closed before terminator completed");
                            acc.extend_from_slice(&tmp[..n]);
                        }
                        assert_eq!(&acc[cursor..cursor + 2], b"\r\n", "terminator must be 0\\r\\n\\r\\n");
                        cursor += 2;
                        terminated = true;
                        break;
                    }
                    while acc.len() < cursor + size + 2 {
                        let n = sock.read(&mut tmp).expect("read chunk payload");
                        assert!(n > 0, "closed mid chunk payload");
                        acc.extend_from_slice(&tmp[..n]);
                    }
                    stream_body.push_str(&String::from_utf8_lossy(&acc[cursor..cursor + size]));
                    cursor += size + 2;
                    chunks += 1;
                    break;
                }
                let n = sock.read(&mut tmp).expect("read chunk size line");
                assert!(n > 0, "closed mid-frame");
                acc.extend_from_slice(&tmp[..n]);
            }
        }
        assert_eq!(chunks, 2, "two live chunk frames expected");
        assert_eq!(stream_body, "event: t\ndata: 1\n\ndata: 2\n\n");
        // Wire framing spot-check: 18 bytes -> hex "12", 9 bytes -> "9".
        let wire = String::from_utf8(acc.clone()).unwrap();
        assert!(wire.contains("12\r\nevent: t\ndata: 1\n\n\r\n"), "hex-18 frame {wire:?}");
        assert!(wire.contains("9\r\ndata: 2\n\n\r\n0\r\n\r\n"), "hex-9+terminator {wire:?}");

        // 3. Same socket, buffered follow-up (Phase 2 semantics preserved after streaming).
        sock.write_all(b"GET /stats HTTP/1.1\r\nHost: z\r\nConnection: close\r\n\r\n").unwrap();
        let stats = read_http_response(&mut sock);
        assert!(stats.starts_with("HTTP/1.1 200 OK\r\n"), "follow-up {stats:?}");
        assert!(stats.contains("Content-Length: 4\r\n"), "buffered length header {stats:?}");
        assert!(stats.ends_with("done"));
        consumer.join().unwrap();
        assert!(stop_listener(&mut listener));
    }

    #[test]
    fn client_disconnect_while_streaming_is_detected_and_notified() {
        let _lock = engine_test_lock();
        let (tx, rx) = channel::<ConnectionEvent>();
        let mut listener = spawn_listener("127.0.0.1", 0, 63, tx).expect("bind");
        let mut sock = TcpStream::connect(("127.0.0.1", listener.port)).expect("connect");
        sock.set_nodelay(true).ok();
        sock.write_all(b"GET /events HTTP/1.1\r\nHost: z\r\nConnection: keep-alive\r\n\r\n").unwrap();

        // The engine side of a live SSE stream: deliver the head, then the peer vanishes.
        let ev = rx.recv_timeout(Duration::from_secs(5)).expect("event");
        let conn_id = ev.conn_id;
        ev.reply
            .send(ResponseAction::StreamHead {
                status: 200,
                reason: None,
                headers: vec![("content-type".into(), "text/event-stream".into())],
                keep_alive: true,
            })
            .expect("stream head");
        // Ensure the head really reached the client before it hangs up.
        let mut tmp = [0u8; 4096];
        let n = sock.read(&mut tmp).expect("read head");
        assert!(String::from_utf8_lossy(&tmp[..n]).contains("Transfer-Encoding: chunked"));
        drop(sock); // EventSource-style disconnect: FIN arrives on the socket

        assert!(
            wait_for_disconnect(conn_id, Duration::from_secs(3)),
            "poll/peek EOF must notify disconnect for conn {conn_id}"
        );
        // The reader thread returned (receiver dropped): later stream writes report failure.
        let t0 = Instant::now();
        loop {
            if ev.reply.send(ResponseAction::StreamChunk(b"late".to_vec())).is_err() {
                break;
            }
            assert!(t0.elapsed() < Duration::from_secs(3), "dead channel never observed");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(stop_listener(&mut listener));
    }

    #[test]
    fn rapid_pushes_do_not_starve_disconnect_detection_after_peer_hangs_up() {
        let _lock = engine_test_lock();
        let (tx, rx) = channel::<ConnectionEvent>();
        let mut listener = spawn_listener("127.0.0.1", 0, 67, tx).expect("bind");
        let mut sock = TcpStream::connect(("127.0.0.1", listener.port)).expect("connect");
        sock.set_nodelay(true).ok();
        sock.write_all(b"GET /events HTTP/1.1\r\nHost: z\r\nConnection: keep-alive\r\n\r\n").unwrap();
        let ev = rx.recv_timeout(Duration::from_secs(5)).expect("event");
        let conn_id = ev.conn_id;
        ev.reply
            .send(ResponseAction::StreamHead {
                status: 200,
                reason: None,
                headers: vec![("content-type".into(), "text/event-stream".into())],
                keep_alive: true,
            })
            .expect("head");
        // Mimic the JS interval: keep pushing chunks while the client is gone. The post-write
        // peer probe must notice EOF/RST within a few pushes (no starvation).
        std::thread::sleep(Duration::from_millis(80));
        drop(sock); // client hangs up mid-stream (unread server data pending -> RST possible)
        let mut pushed = 0usize;
        let detected = loop {
            pushed += 1;
            if ev.reply.send(ResponseAction::StreamChunk(format!("tick-{pushed}\n").into_bytes())).is_err() {
                break true; // dead channel: thread exited via disconnect path
            }
            if wait_for_disconnect(conn_id, Duration::from_millis(40)) {
                break true;
            }
            assert!(pushed < 400, "disconnect never detected after {pushed} rapid pushes");
        };
        assert!(detected, "rapid pushes must still detect hang-up");
        drop(ev.reply);
        assert!(stop_listener(&mut listener));
    }

    #[test]
    fn wedged_stream_hits_deadline_socket_closes_and_disconnect_is_notified() {
        let _lock = engine_test_lock();
        let (tx, rx) = channel::<ConnectionEvent>();
        // Tiny per-action budget: a handler that flushes headers then goes silent must not hold
        // the socket hostage; it gets ended and reported as closed.
        let mut listener =
            spawn_listener_with_timeout("127.0.0.1", 0, 65, tx, Duration::from_millis(150)).expect("bind");
        let mut sock = TcpStream::connect(("127.0.0.1", listener.port)).expect("connect");
        sock.set_nodelay(true).ok();
        sock.write_all(b"GET /events HTTP/1.1\r\nHost: z\r\nConnection: keep-alive\r\n\r\n").unwrap();
        let ev = rx.recv_timeout(Duration::from_secs(5)).expect("event");
        let conn_id = ev.conn_id;
        ev.reply
            .send(ResponseAction::StreamHead {
                status: 200,
                reason: None,
                headers: vec![("content-type".into(), "text/event-stream".into())],
                keep_alive: true,
            })
            .expect("head");
        // Client stays OPEN (no EOF): only the deadline can end this stream.
        let t0 = Instant::now();
        let mut saw_eof = false;
        let mut tmp = [0u8; 64];
        while t0.elapsed() < Duration::from_secs(3) {
            match sock.read(&mut tmp) {
                Ok(0) => {
                    saw_eof = true;
                    break;
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        assert!(saw_eof, "engine must close a wedged stream socket");
        assert!(
            wait_for_disconnect(conn_id, Duration::from_secs(2)),
            "deadline-exceeded stream must fire the disconnect path"
        );
        drop(ev.reply);
        assert!(stop_listener(&mut listener));
    }
}
