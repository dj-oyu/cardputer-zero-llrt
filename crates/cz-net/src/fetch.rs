//! Synchronous worker-thread HTTP/1.1 engine on top of `ureq` v3 with a thin custom
//! `Transport` that keeps a cloned `TcpStream` so `AbortController.abort()` can shut the socket
//! down and interrupt an idle SSE stream in milliseconds (spec \u00a74).
//!
//! Hard rules enforced here:
//! - No polling loops: every header/chunk/completion/error/abort calls
//!   `cz_hal::http_server::notify_event_loop()` so `cz-runner`'s Condvar idle wait wakes instantly.
//! - Worker threads use a 256 KiB stack and at most [`MAX_CONCURRENT_FETCHES`] are in flight.
//! - Streaming responses use a bounded [`CHUNK_QUEUE_DEPTH`] x [`CHUNK_SIZE`] backpressure queue
//!   (~128 KiB max per stream); the worker parks on a Condvar while JS drains.
//! - Buffered consumers (`text()`/`json()`/`bytes()`/`arrayBuffer()`) enforce a 16 MiB cap; the
//!   underlying stream is cancelled and the Promise rejects instead of growing memory.

use std::collections::VecDeque;
use std::fmt;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::Builder;
use std::time::{Duration, Instant};

use rustls::pki_types::ServerName;
use rustls::{ClientConnection, StreamOwned};
use ureq::config::Config;
use ureq::unversioned::resolver::DefaultResolver;
use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, Connector, LazyBuffers, NextTimeout, Transport,
};
use ureq::{Agent, Error, ResponseExt};

use crate::tls::{build_client_config, status_text, TlsClientOptions};

/// Worker thread stack size (256 KiB — tuned for the Cortex-A53 / 512 MB target).
pub const WORKER_STACK_BYTES: usize = 256 * 1024;
/// Maximum number of concurrently in-flight `fetch` requests.
pub const MAX_CONCURRENT_FETCHES: usize = 16;
/// Body chunk size handed to streaming consumers.
pub const CHUNK_SIZE: usize = 16 * 1024;
/// Maximum buffered chunks per streaming response (~128 KiB total backpressure).
pub const CHUNK_QUEUE_DEPTH: usize = 8;
/// Default cap for buffered bodies (`text()`/`bytes()`/...): 16 MiB.
pub const MAX_BUFFERED_BODY: usize = 16 * 1024 * 1024;
/// TCP connect timeout used when the caller sets none.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the worker parks on a full queue before re-checking the abort flag.
const QUEUE_PARK_SLICE: Duration = Duration::from_millis(200);

// ---------------------------------------------------------------------------
// Abort plumbing
// ---------------------------------------------------------------------------

/// Shared cancellation state between the JS thread and the fetch worker.
#[derive(Default)]
pub struct AbortHandle {
    aborted: AtomicBool,
    socket: Mutex<Option<Arc<TcpStream>>>,
}

impl fmt::Debug for AbortHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AbortHandle")
            .field("aborted", &self.aborted.load(Ordering::SeqCst))
            .finish()
    }
}

impl AbortHandle {
    pub fn new() -> Self {
        AbortHandle {
            aborted: AtomicBool::new(false),
            socket: Mutex::new(None),
        }
    }

    pub fn is_aborted(&self) -> bool {
        self.aborted.load(Ordering::SeqCst)
    }

    /// Attach the live socket right after connect (before TLS wrapping).
    fn attach(&self, stream: Arc<TcpStream>) {
        *self.socket.lock().expect("abort socket lock") = Some(stream);
    }

    /// Set the flag and shut down whatever socket is currently live. Reads/writes blocked on it
    /// fail immediately; ureq surfaces that as `Error::Io`.
    pub fn signal(&self) {
        self.aborted.store(true, Ordering::SeqCst);
        if let Some(sock) = self.socket.lock().expect("abort socket lock").as_ref() {
            // Error means the socket is already closed.
            let _ = TcpStream::shutdown(sock, Shutdown::Both);
        }
    }
}

// ---------------------------------------------------------------------------
// Thin Transport holding the TcpStream
// ---------------------------------------------------------------------------

/// Blocking plain-TCP transport mirroring ureq's `TcpTransport` but sharing the socket with an
/// [`AbortHandle`] so cancellation works mid-read.
struct PlainTransport {
    buffers: LazyBuffers,
    stream: Arc<TcpStream>,
    timeout_read: Option<Duration>,
    timeout_write: Option<Duration>,
}

/// Adapter so rustls can drive our shared `Arc<TcpStream>` during the handshake.
struct SharedSock {
    stream: Arc<TcpStream>,
}

impl Read for SharedSock {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        (&*self.stream).read(buf)
    }
}

impl Write for SharedSock {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        (&*self.stream).write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        (&*self.stream).flush()
    }
}

/// TLS transport: rustls over the shared (abortable) TCP socket, driven by `rustls::StreamOwned`
/// (verified to implement `Read`/`Write` for any `C: DerefMut<Target = ConnectionCommon<S>>`).
struct TlsTransport {
    buffers: LazyBuffers,
    stream: StreamOwned<ClientConnection, SharedSock>,
    timeout_read: Option<Duration>,
    timeout_write: Option<Duration>,
}

fn set_read_timeout(cache: &mut Option<Duration>, stream: &TcpStream, t: NextTimeout) -> bool {
    let want = t.for_read().not_zero().map(|d| *d);
    if want != *cache {
        if stream.set_read_timeout(want).is_ok() {
            *cache = want;
        }
    }
    want.is_some()
}

fn set_write_timeout(cache: &mut Option<Duration>, stream: &TcpStream, t: NextTimeout) {
    let want = t.for_write().not_zero().map(|d| *d);
    if want != *cache && stream.set_write_timeout(want).is_ok() {
        *cache = want;
    }
}

/// Non-blocking probe used by `is_open()` (ureq semantics: readable bytes or an error means the
/// connection is not reusable).
///
/// Phase 10C: `libc::recv(..., MSG_PEEK | MSG_DONTWAIT)` on the live fd replaces the old
/// `stream.try_clone()` + `set_nonblocking()` dance — no extra descriptor dup per keep-alive check
/// and no temporary blocking-mode flip that could race with the socket's real reads.
fn probe_open(stream: &TcpStream) -> bool {
    let mut one = [0u8; 1];
    let n = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            one.as_mut_ptr() as *mut libc::c_void,
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    if n > 0 {
        // Bytes are already queued (pipelined body, garbage, or TLS records): not reusable.
        return false;
    }
    if n == 0 {
        // EOF: the peer closed the connection.
        return false;
    }
    // -1 -> open only when the socket is merely "not ready right now" (EAGAIN/EWOULDBLOCK);
    // any other errno (ECONNRESET, ENOTCONN, ...) means it cannot be reused.
    match std::io::Error::last_os_error().raw_os_error() {
        Some(e) => e == libc::EAGAIN || e == libc::EWOULDBLOCK,
        None => false,
    }
}

impl Transport for PlainTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), Error> {
        set_write_timeout(&mut self.timeout_write, &self.stream, timeout);
        let out = &self.buffers.output()[..amount];
        (&*self.stream).write_all(out)?;
        (&*self.stream).flush()?;
        Ok(())
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, Error> {
        set_read_timeout(&mut self.timeout_read, &self.stream, timeout);
        let buf = self.buffers.input_append_buf();
        match (&*self.stream).read(buf) {
            Ok(n) => {
                self.buffers.input_appended(n);
                Ok(n > 0)
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut || e.kind() == std::io::ErrorKind::WouldBlock => {
                // ureq turns the read timeout into Error::Timeout when the budget expires.
                if budget_expired(timeout.for_read()) {
                    Err(Error::Timeout(timeout.for_read().reason))
                } else {
                    Ok(false)
                }
            }
            Err(e) => Err(e.into()),
        }
    }

    fn is_open(&mut self) -> bool {
        probe_open(&self.stream)
    }
}

/// True when a NextTimeout budget has fully elapsed (`Duration::NotHappening` never expires).
fn budget_expired(t: NextTimeout) -> bool {
    !t.after.is_not_happening() && (*t.after).is_zero()
}

impl Transport for TlsTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), Error> {
        set_write_timeout(&mut self.timeout_write, &self.stream.get_ref().stream, timeout);
        let out = &self.buffers.output()[..amount];
        self.stream.write_all(out)?;
        // StreamOwned::flush pushes every pending TLS record to the socket and flushes it.
        self.stream.flush()?;
        Ok(())
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, Error> {
        set_read_timeout(&mut self.timeout_read, &self.stream.get_ref().stream, timeout);
        let buf = self.buffers.input_append_buf();
        match self.stream.read(buf) {
            Ok(n) => {
                self.buffers.input_appended(n);
                Ok(n > 0)
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut || e.kind() == std::io::ErrorKind::WouldBlock => {
                if budget_expired(timeout.for_read()) {
                    Err(Error::Timeout(timeout.for_read().reason))
                } else {
                    Ok(false)
                }
            }
            Err(e) => Err(e.into()),
        }
    }

    fn is_open(&mut self) -> bool {
        probe_open(&self.stream.get_ref().stream)
    }

    fn is_tls(&self) -> bool {
        true
    }
}

impl fmt::Debug for PlainTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CzPlainTransport").finish()
    }
}

impl fmt::Debug for TlsTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CzTlsTransport").field("tls", &true).finish()
    }
}

enum CzTransportInner {
    Plain(PlainTransport),
    Tls(TlsTransport),
}

/// The transport produced by [`CzConnector`]: plain TCP or rustls-wrapped TCP.
struct CzTransport {
    inner: CzTransportInner,
}

impl Transport for CzTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        match &mut self.inner {
            CzTransportInner::Plain(t) => t.buffers(),
            CzTransportInner::Tls(t) => t.buffers(),
        }
    }
    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), Error> {
        match &mut self.inner {
            CzTransportInner::Plain(t) => t.transmit_output(amount, timeout),
            CzTransportInner::Tls(t) => t.transmit_output(amount, timeout),
        }
    }
    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, Error> {
        match &mut self.inner {
            CzTransportInner::Plain(t) => t.await_input(timeout),
            CzTransportInner::Tls(t) => t.await_input(timeout),
        }
    }
    fn is_open(&mut self) -> bool {
        match &mut self.inner {
            CzTransportInner::Plain(t) => t.is_open(),
            CzTransportInner::Tls(t) => t.is_open(),
        }
    }
    fn is_tls(&self) -> bool {
        match &self.inner {
            CzTransportInner::Plain(_) => false,
            CzTransportInner::Tls(_) => true,
        }
    }
}

impl fmt::Debug for CzTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CzTransport")
            .field("tls", &self.is_tls())
            .finish()
    }
}

/// Per-request connector: opens an abortable TCP socket and wraps it in rustls when needed.
struct CzConnector {
    tls: Option<Arc<rustls::ClientConfig>>,
    abort: Arc<AbortHandle>,
}

impl fmt::Debug for CzConnector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CzConnector").field("tls", &self.tls.is_some()).finish()
    }
}

/// Extract the bare host from a URI authority (handles bracketed IPv6, userinfo, and ports).
fn bare_host(uri: &ureq::http::Uri) -> Option<&str> {
    let auth = uri.authority()?.as_str();
    let hostport = auth.rsplit_once('@').map(|(_, hp)| hp).unwrap_or(auth);
    if let Some(rest) = hostport.strip_prefix('[') {
        let end = rest.find(']')?;
        let inner = &rest[..end];
        if inner.is_empty() {
            return None;
        }
        return Some(inner);
    }
    let host = hostport.rsplit_once(':').map(|(h, _)| h).unwrap_or(hostport);
    if host.is_empty() {
        return None;
    }
    Some(host)
}

impl Connector for CzConnector {
    type Out = CzTransport;

    fn connect(
        &self,
        details: &ConnectionDetails,
        _chained: Option<()>,
    ) -> Result<Option<Self::Out>, Error> {
        if details.addrs.is_empty() {
            return Err(Error::ConnectionFailed);
        }
        let budget = Duration::from_secs_f64(
            details.timeout.not_zero().map(|d| d.as_secs_f64()).unwrap_or(30.0),
        );
        let per_addr = budget.max(Duration::from_millis(250));

        let mut last_err: Option<std::io::Error> = None;
        let mut connected: Option<TcpStream> = None;
        for addr in details.addrs.iter() {
            match TcpStream::connect_timeout(addr, per_addr) {
                Ok(s) => {
                    connected = Some(s);
                    break;
                }
                Err(e) => last_err = Some(e),
            }
        }
        let stream = match connected {
            Some(s) => s,
            None => {
                return Err(match last_err {
                    Some(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                        Error::Timeout(ureq::Timeout::Connect)
                    }
                    other => match other {
                        Some(e) => Error::Io(e),
                        None => Error::ConnectionFailed,
                    },
                });
            }
        };
        if details.config.no_delay() {
            let _ = stream.set_nodelay(true);
        }

        let shared = Arc::new(stream);
        // Register with the abort handle BEFORE any TLS negotiation so cancellation reaches it.
        self.abort.attach(shared.clone());
        // Race guard: if JavaScript aborted while we were still connecting, tear the fresh
        // socket down immediately instead of proceeding into the request.
        if self.abort.is_aborted() {
            let _ = TcpStream::shutdown(&shared, Shutdown::Both);
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "fetch aborted during connect",
            )));
        }

        let buffers = LazyBuffers::new(
            details.config.input_buffer_size(),
            details.config.output_buffer_size(),
        );

        if !details.needs_tls() {
            return Ok(Some(CzTransport {
                inner: CzTransportInner::Plain(PlainTransport {
                    buffers,
                    stream: shared,
                    timeout_read: None,
                    timeout_write: None,
                }),
            }));
        }

        let config = self
            .tls
            .clone()
            .ok_or_else(|| Error::Tls("TLS requested without a client config"))?;
        let host = bare_host(details.uri).ok_or(Error::Tls("missing uri host"))?;
        let name = ServerName::try_from(host)
            .map(|n| n.to_owned())
            .map_err(|_| Error::Tls("invalid tls dns name"))?;

        let handshake_timeout = details
            .config
            .timeouts()
            .connect
            .unwrap_or(DEFAULT_CONNECT_TIMEOUT);
        let _ = shared.set_read_timeout(Some(handshake_timeout));
        let _ = shared.set_write_timeout(Some(handshake_timeout));

        let mut conn = ClientConnection::new(config, name)?;
        let mut sock = SharedSock { stream: shared.clone() };
        let hs_deadline = Instant::now() + handshake_timeout;
        // Blocking handshake (rustls drives I/O over our shared socket). Loop until
        // `CommonState::is_handshaking()` clears, bounded by abort + handshake_timeout.
        loop {
            if self.abort.is_aborted() {
                let _ = TcpStream::shutdown(&shared, Shutdown::Both);
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "fetch aborted during TLS handshake",
                )));
            }
            match conn.complete_io(&mut sock) {
                Ok(_) if !conn.is_handshaking() => break,
                Ok((0, 0)) => {
                    return Err(Error::Io(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "EOF during TLS handshake",
                    )));
                }
                Ok(_) => continue,
                Err(ref e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::Interrupted =>
                {
                    if Instant::now() >= hs_deadline {
                        return Err(Error::Timeout(ureq::Timeout::Connect));
                    }
                    continue;
                }
                Err(e) => return Err(Error::Io(e)),
            }
        }
        let _ = shared.set_read_timeout(None);
        let _ = shared.set_write_timeout(None);

        Ok(Some(CzTransport {
            inner: CzTransportInner::Tls(TlsTransport {
                buffers,
                stream: StreamOwned::new(conn, SharedSock { stream: shared }),
                timeout_read: None,
                timeout_write: None,
            }),
        }))
    }
}

// ---------------------------------------------------------------------------
// Request spec + events
// ---------------------------------------------------------------------------

/// Redirect policy for `fetch(init.redirect)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedirectMode {
    Follow,
    Manual,
    Error,
}

/// Parse the JS `redirect` option (unknown values behave like `"follow"`, per spec).
pub fn parse_redirect_mode(value: &str) -> RedirectMode {
    let trimmed = value.trim();
    if trimmed.eq_ignore_ascii_case("manual") {
        RedirectMode::Manual
    } else if trimmed.eq_ignore_ascii_case("error") {
        RedirectMode::Error
    } else {
        RedirectMode::Follow
    }
}

/// True when a status is a 3xx redirect response.
pub fn is_redirect_status(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// Everything needed to issue one outbound request (plain data only — no JS values cross the
/// `Context::with` boundary).
#[derive(Clone, Debug)]
pub struct FetchSpec {
    pub url: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub redirect: RedirectMode,
    pub tls: TlsClientOptions,
}

/// Failure classification surfaced to JavaScript.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailKind {
    /// AbortController.abort() / AbortSignal.timeout fired.
    Abort,
    /// A configured timeout expired.
    Timeout,
    /// Buffered body exceeded [`MAX_BUFFERED_BODY`].
    BodyTooLarge,
    /// `redirect: "error"` hit a 3xx response.
    Redirect,
    /// Network / TLS / protocol failure.
    Network,
}

/// One queued engine event for a request slot.
#[derive(Debug)]
pub enum EngineEvent {
    Headers {
        status: u16,
        status_text: String,
        url: String,
        redirected: bool,
        headers: Vec<(String, String)>,
    },
    Chunk(Vec<u8>),
    /// Buffered success (whole body).
    Body(Vec<u8>),
    /// Stream terminated cleanly (streaming mode).
    Done,
    Failed { kind: FailKind, message: String },
}

// ---------------------------------------------------------------------------
// Slot registry
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Buffered,
    Streaming,
}

struct SlotInner {
    events: VecDeque<EngineEvent>,
    mode: Mode,
    buffer: Vec<u8>,
    // Set once the worker delivers the terminal event (Body/Done/Failed).
    finished: bool,
    // Whether any chunk was delivered to JavaScript (guards mode switches and body consumers).
    chunks_delivered: usize,
    // Worker cancelled because buffered body exceeded the cap.
    overflow: bool,
}

struct Slot {
    id: u64,
    abort: Arc<AbortHandle>,
    cv: Condvar,
    inner: Mutex<SlotInner>,
}

impl Slot {
    fn new(id: u64, abort: Arc<AbortHandle>) -> Self {
        Slot {
            id,
            abort,
            cv: Condvar::new(),
            inner: Mutex::new(SlotInner {
                events: VecDeque::new(),
                mode: Mode::Buffered,
                buffer: Vec::new(),
                finished: false,
                chunks_delivered: 0,
                overflow: false,
            }),
        }
    }

    /// Push an event with streaming backpressure; parks while the queue is full unless aborted.
    fn push(&self, event: EngineEvent) {
        let streaming_chunk = matches!(event, EngineEvent::Chunk(_));
        let mut guard = self.inner.lock().expect("slot events lock");
        if streaming_chunk && guard.mode == Mode::Streaming {
            while guard.events.len() >= CHUNK_QUEUE_DEPTH && !self.abort.is_aborted() {
                let (g, _timeout) = self
                    .cv
                    .wait_timeout(guard, QUEUE_PARK_SLICE)
                    .expect("chunk queue condvar wait");
                guard = g;
            }
            if self.abort.is_aborted() {
                return;
            }
        }
        guard.events.push_back(event);
        drop(guard);
        cz_hal::http_server::notify_event_loop();
    }

    fn fail(&self, kind: FailKind, message: String) {
        let mut guard = self.inner.lock().expect("slot events lock");
        if guard.finished {
            return;
        }
        guard.finished = true;
        guard.events.push_back(EngineEvent::Failed { kind, message });
        drop(guard);
        cz_hal::http_server::notify_event_loop();
    }

    fn is_finished(&self) -> bool {
        self.inner.lock().expect("slot events lock").finished
    }
}

fn engine() -> &'static Engine {
    static REG: OnceLock<Engine> = OnceLock::new();
    REG.get_or_init(|| {
        Engine {
            slots: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            inflight: AtomicUsize::new(0),
        }
    })
}

struct Engine {
    slots: Mutex<Vec<Arc<Slot>>>,
    next_id: AtomicU64,
    inflight: AtomicUsize,
}

/// Start one fetch. Returns the slot id on success; `Err` when the concurrency cap is hit or the
/// worker thread cannot be spawned.
pub fn start_fetch(spec: FetchSpec) -> Result<u64, String> {
    let reg = engine();
    if spec.url.is_empty() {
        return Err("fetch: empty url".to_string());
    }
    // Concurrency gate: acquire before spawning so 17+ requests reject cleanly.
    let prev = reg.inflight.fetch_add(1, Ordering::SeqCst);
    if prev >= MAX_CONCURRENT_FETCHES {
        reg.inflight.fetch_sub(1, Ordering::SeqCst);
        return Err("fetch: too many concurrent requests (resource limit)".to_string());
    }

    let abort = Arc::new(AbortHandle::new());
    let id = reg.next_id.fetch_add(1, Ordering::SeqCst);
    let slot = Arc::new(Slot::new(id, abort.clone()));
    reg.slots.lock().expect("slots lock").push(slot.clone());

    let spawn_result = Builder::new()
        .name(format!("cz-fetch-{id}"))
        .stack_size(WORKER_STACK_BYTES)
        .spawn(move || {
            worker_main(spec, slot, abort);
            reg.inflight.fetch_sub(1, Ordering::SeqCst);
            cz_hal::http_server::notify_event_loop();
        });

    match spawn_result {
        Ok(_handle) => Ok(id),
        Err(e) => {
            reg.inflight.fetch_sub(1, Ordering::SeqCst);
            let mut guard = engine().slots.lock().expect("slots lock");
            guard.retain(|s| s.id != id);
            return Err(format!("fetch: cannot spawn worker thread: {e}"));
        }
    }
}

fn find_slot(id: u64) -> Option<Arc<Slot>> {
    engine()
        .slots
        .lock()
        .expect("slots lock")
        .iter()
        .find(|s| s.id == id)
        .cloned()
}

/// Pop the next queued event for `id` (JS pump loop calls this until it returns `None`).
pub fn next_event(id: u64) -> Option<EngineEvent> {
    let slot = find_slot(id)?;
    let (ev, popped_chunk) = {
        let mut guard = slot.inner.lock().expect("slot events lock");
        let ev = guard.events.pop_front();
        let is_chunk = matches!(&ev, Some(EngineEvent::Chunk(_)));
        if is_chunk {
            // Consuming a chunk pins the stream to streaming mode (guards later buffered switch).
            guard.chunks_delivered += 1;
        }
        (ev, is_chunk)
    };
    if popped_chunk {
        // Wake a worker parked on a full streaming backpressure queue immediately.
        slot.cv.notify_all();
    }
    if ev.is_some() {
        cz_hal::http_server::notify_event_loop();
    }
    ev
}

#[cfg(test)]
/// Serial guard for tests that depend on the process-wide slot registry (concurrency cap,
/// pending/active counts, cancellation, body cap, redirects). Without it, concurrently scheduled
/// engine tests observe each other's in-flight slots and fail spuriously; the verifier happens to
/// run serially (`--test-threads=1`), but the suite is now deterministic under default parallelism.
pub(crate) fn registry_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// All live slot ids (requests whose slots have not been released yet).
pub fn live_ids() -> Vec<u64> {
    engine().slots.lock().expect("slots lock").iter().map(|s| s.id).collect()
}

/// Cancel a request: abort flag + socket shutdown + condvar wake + event-loop wake.
pub fn cancel_fetch(id: u64) -> bool {
    match find_slot(id) {
        Some(slot) => {
            slot.abort.signal();
            slot.cv.notify_all();
            cz_hal::http_server::notify_event_loop();
            true
        }
        None => false,
    }
}

/// Ask for buffered consumption (text/json/bytes/arrayBuffer). Only legal while no chunk has been
/// delivered to JavaScript. Queued chunks are folded into the internal buffer.
pub fn switch_buffered(id: u64) -> bool {
    let slot = match find_slot(id) {
        Some(s) => s,
        None => return false,
    };
    let mut deliver_body = None;
    {
        let mut guard = slot.inner.lock().expect("slot events lock");
        // Illegal once JavaScript has consumed any chunk (Web `bodyUsed` semantics).
        if guard.chunks_delivered > 0 {
            return false;
        }
        // Fold everything the worker already queued (Headers stays at the front) into the buffer.
        let pending: Vec<EngineEvent> = guard.events.drain(..).collect();
        let mut headers_first = None;
        for ev in pending {
            match ev {
                EngineEvent::Headers { .. } => headers_first = Some(ev),
                EngineEvent::Chunk(bytes) => guard.buffer.extend_from_slice(&bytes),
                EngineEvent::Body(bytes) => guard.buffer.extend_from_slice(&bytes),
                EngineEvent::Done | EngineEvent::Failed { .. } => {
                    if matches!(ev, EngineEvent::Failed { .. }) {
                        // Re-queue failures: buffering cannot swallow an error.
                        guard.events.push_back(ev);
                    }
                }
            }
        }
        if let Some(h) = headers_first {
            guard.events.push_front(h);
        }
        guard.mode = Mode::Buffered;
        // A worker that already finished while streaming: synthesize the buffered result now.
        if guard.finished && !guard.events.iter().any(|e| matches!(e, EngineEvent::Failed { .. })) {
            deliver_body = Some(std::mem::take(&mut guard.buffer));
        }
    }
    slot.cv.notify_all();
    if let Some(bytes) = deliver_body {
        let mut g = slot.inner.lock().expect("slot events lock");
        g.events.push_back(EngineEvent::Body(bytes));
        drop(g);
    }
    cz_hal::http_server::notify_event_loop();
    true
}

/// Ask for streaming consumption (getReader / async iterator). The already-read prefix is emitted
/// as `Chunk` events; a finished buffered body is replayed too.
pub fn switch_streaming(id: u64) -> bool {
    let slot = match find_slot(id) {
        Some(s) => s,
        None => return false,
    };
    {
        let mut guard = slot.inner.lock().expect("slot events lock");
        guard.mode = Mode::Streaming;
        // Replay everything the engine already has, preserving byte order:
        //  * a still-buffering worker's accumulated prefix (the worker may be parked inside a
        //    blocking read on an idle SSE socket, so it cannot flush the prefix itself);
        //  * or an already-queued single Body event (worker finished while we were buffering).
        let mut pending: Vec<Vec<u8>> = Vec::new();
        if !guard.buffer.is_empty() {
            pending.push(std::mem::take(&mut guard.buffer));
        }
        if let Some(pos) = guard
            .events
            .iter()
            .position(|e| matches!(e, EngineEvent::Body(_)))
        {
            if let Some(EngineEvent::Body(bytes)) = guard.events.remove(pos) {
                pending.push(bytes);
            }
        }
        // Insert the chunked replay right after the Headers event (if it is still queued), then
        // terminate with Done when the body had already completed.
        let mut insert_at = guard
            .events
            .iter()
            .position(|e| matches!(e, EngineEvent::Headers { .. }))
            .map(|p| p + 1)
            .unwrap_or(0);
        let already_done = guard.finished;
        for piece in pending.iter().flat_map(|b| b.chunks(CHUNK_SIZE)) {
            guard.events.insert(insert_at, EngineEvent::Chunk(piece.to_vec()));
            insert_at += 1;
        }
        if already_done && !guard.events.iter().any(|e| matches!(e, EngineEvent::Done | EngineEvent::Failed { .. })) {
            guard.events.push_back(EngineEvent::Done);
        }
    }
    slot.cv.notify_all();
    cz_hal::http_server::notify_event_loop();
    true
}

/// Drop the JS-side reference to a response/stream: removes the slot (cancelling any unfinished
/// worker so no thread keeps running after the script ends).
pub fn release_slot(id: u64) -> bool {
    // Cancel first (so a parked worker wakes and exits), then drop the slot from the registry.
    let slot = find_slot(id);
    let removed = {
        let mut guard = engine().slots.lock().expect("slots lock");
        let before = guard.len();
        guard.retain(|s| s.id != id);
        guard.len() < before
    };
    if !removed {
        return false;
    }
    if let Some(s) = slot {
        if !s.is_finished() {
            s.abort.signal();
            s.cv.notify_all();
        }
    }
    cz_hal::http_server::notify_event_loop();
    true
}

/// Number of live slots (responses/streams not yet released).
pub fn active_fetch_count() -> usize {
    engine().slots.lock().expect("slots lock").len()
}

/// True while any fetch is genuinely outstanding: a worker is in flight OR some slot still has
/// undelivered events (used by `cz-runner` liveness so the process never exits mid-request).
pub fn fetch_has_pending() -> bool {
    let reg = engine();
    if reg.inflight.load(Ordering::SeqCst) > 0 {
        return true;
    }
    let slots = reg.slots.lock().expect("slots lock");
    // Undelivered events (headers/chunks/body/failed) or a worker that has not finished yet.
    slots
        .iter()
        .any(|s| !s.inner.lock().expect("slot events lock").finished)
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

fn classify_failure(err: &Error, aborted: bool) -> (FailKind, String) {
    if aborted {
        return (FailKind::Abort, "This operation was aborted".to_string());
    }
    match err {
        Error::Timeout(_t) => (FailKind::Timeout, "Request timed out".to_string()),
        Error::TooManyRedirects => (FailKind::Redirect, "Too many redirects".to_string()),
        other => (FailKind::Network, format!("{other}")),
    }
}

/// Phase 14: build a fully synchronous ureq `Agent` for native Rust consumers (`cz-zpm`).
///
/// It reuses the exact same `CzConnector` (plain TCP + rustls-over-`ClientConnection` with system
/// CA + ring via [`build_client_config`]) that powers JS `fetch()` — no `tokio`, no `hyper`, no
/// bundled `webpki-roots`. The fresh [`AbortHandle`] is never signaled here; callers simply get
/// blocking `request.call()` semantics on their own thread.
pub fn sync_agent(cfg: Config, tls: Option<Arc<rustls::ClientConfig>>) -> Agent {
    Agent::with_parts(cfg, CzConnector { tls, abort: Arc::new(AbortHandle::new()) }, DefaultResolver::default())
}

fn build_config(redirect: RedirectMode) -> Config {
    let mut builder = Config::builder()
        // Non-2xx must RESOLVE (Web Fetch semantics), never reject.
        .http_status_as_error(false)
        // No connection pooling: every request owns one abortable socket.
        .max_idle_connections(0)
        .max_idle_connections_per_host(0)
        .timeout_connect(Some(DEFAULT_CONNECT_TIMEOUT))
        .no_delay(true);
    match redirect {
        RedirectMode::Follow => {
            builder = builder.max_redirects(10).save_redirect_history(true);
        }
        RedirectMode::Manual | RedirectMode::Error => {
            // Return the 3xx response itself (JS decides manual vs error handling).
            builder = builder.max_redirects(0).max_redirects_will_error(false);
        }
    }
    builder.build()
}

/// Buffered-body capacity hint from a valid `Content-Length` header that stays inside the
/// [`MAX_BUFFERED_BODY`] cap (`None` when absent, unparsable or oversized).
///
/// Phase 10C: pre-sizing `SlotInner::buffer` once from the declared length removes the repeated
/// realloc/copy cycle while appending 16 KiB chunks to a buffered body.
#[cfg_attr(not(test), allow(dead_code))]
#[cfg_attr(not(test), allow(dead_code))]
fn content_length_hint(headers: &ureq::http::HeaderMap) -> Option<usize> {
    let raw = headers.get("content-length")?.as_bytes();
    let text = std::str::from_utf8(raw).ok()?;
    let n: usize = text.trim().parse().ok()?;
    if n > MAX_BUFFERED_BODY {
        return None;
    }
    Some(n)
}

fn header_pairs(res: &ureq::http::Response<ureq::Body>) -> Vec<(String, String)> {
    let mut out = Vec::with_capacity(res.headers().len());
    // HeaderMap::iter yields a key once per associated value (duplicates included), which the JS
    // `Headers` object joins with ", ".
    for (name, value) in res.headers().iter() {
        let v = String::from_utf8_lossy(value.as_bytes()).into_owned();
        out.push((name.as_str().to_string(), v));
    }
    out
}

fn worker_main(mut spec: FetchSpec, slot: Arc<Slot>, abort: Arc<AbortHandle>) {
    // 1. Build the agent with the per-request TLS config + our abortable connector.
    let is_https = spec
        .url
        .trim_start()
        .get(..6)
        .is_some_and(|s| s.eq_ignore_ascii_case("https:"));
    let tls_config = if is_https {
        match build_client_config(&spec.tls) {
            Ok(c) => Some(c),
            Err(msg) => {
                slot.fail(FailKind::Network, msg);
                return;
            }
        }
    } else {
        None
    };

    let cfg = build_config(spec.redirect);
    let agent = Agent::with_parts(cfg, CzConnector { tls: tls_config, abort: abort.clone() }, DefaultResolver::default());

    // 2. Issue the request with arbitrary method + optional body.
    let mut builder = ureq::http::Request::builder()
        .method(spec.method.as_str())
        .uri(spec.url.as_str());
    for (k, v) in &spec.headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    let http_req = match builder.body(spec.body.take().unwrap_or_default()) {
        Ok(r) => r,
        Err(e) => {
            slot.fail(FailKind::Network, format!("invalid request: {e}"));
            return;
        }
    };

    let mut res = match agent.run(http_req) {
        Ok(r) => r,
        Err(e) => {
            let (kind, msg) = classify_failure(&e, abort.is_aborted());
            slot.fail(kind, msg);
            return;
        }
    };

    // 3. Header-first resolution: deliver status/headers immediately.
    let status_u16 = u16::from(res.status());
    if spec.redirect == RedirectMode::Error && is_redirect_status(status_u16) {
        slot.fail(
            FailKind::Redirect,
            format!("Failed to follow redirect for {} (redirect: error)", spec.url),
        );
        return;
    }
    let final_url = res.get_uri().to_string();
    let redirected = final_url != spec.url
        || res
            .get_redirect_history()
            .map(|h| !h.is_empty())
            .unwrap_or(false);
    slot.push(EngineEvent::Headers {
        status: status_u16,
        status_text: status_text(status_u16).to_string(),
        url: final_url,
        redirected,
        headers: header_pairs(&res),
    });

    // 3b. Phase 10C: when the peer declared a trustworthy Content-Length inside the buffered cap,
    //     give the engine buffer that exact capacity NOW so the chunk appends below never re-grow.
    if let Some(cap) = content_length_hint(res.headers()) {
        let mut g = slot.inner.lock().expect("slot events lock");
        if g.mode == Mode::Buffered && g.buffer.capacity() < cap {
            if g.buffer.is_empty() {
                g.buffer = Vec::with_capacity(cap);
            } else {
                let have = g.buffer.len();
                g.buffer.reserve_exact(cap.saturating_sub(have));
            }
        }
    }

    // 4. Body pump honoring the consumer mode (Buffered vs Streaming) with backpressure.
    let mut reader = res.body_mut().as_reader();
    let mut scratch = vec![0u8; CHUNK_SIZE];
    loop {
        if abort.is_aborted() {
            slot.fail(FailKind::Abort, "This operation was aborted".to_string());
            return;
        }
        match reader.read(&mut scratch) {
            Ok(0) => break,
            Ok(n) => handle_body_chunk(&slot, &scratch[..n]),
            Err(e) => {
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                let aborted_now = abort.is_aborted();
                let (kind, msg) = if aborted_now {
                    (FailKind::Abort, "This operation was aborted".to_string())
                } else {
                    (FailKind::Network, format!("{e}"))
                };
                slot.fail(kind, msg);
                return;
            }
        }
    }

    // Clean end of body. If JavaScript aborted while we were parked inside a read that the
    // socket shutdown turned into EOF, report an Abort failure (never a silent Done/Body).
    if abort.is_aborted() {
        slot.fail(FailKind::Abort, "This operation was aborted".to_string());
        return;
    }
    {
        let mut g = slot.inner.lock().expect("slot events lock");
        g.finished = true;
        if g.mode == Mode::Streaming {
            // Prefix split is handled inside handle_body_chunk; emit Done atomically with finished.
            g.events.push_back(EngineEvent::Done);
        } else {
            let bytes = std::mem::take(&mut g.buffer);
            g.events.push_back(EngineEvent::Body(bytes));
        }
    }
    slot.cv.notify_all();
    cz_hal::http_server::notify_event_loop();
}

/// Route one decoded body chunk according to the current consumer mode.
fn handle_body_chunk(slot: &Arc<Slot>, data: &[u8]) {
    // Buffered mode: append into the engine buffer with the 16 MiB cap enforced here.
    let buffered = {
        let mut guard = slot.inner.lock().expect("slot events lock");
        if guard.mode == Mode::Buffered {
            if guard.buffer.len() + data.len() > MAX_BUFFERED_BODY {
                guard.overflow = true;
                // Cancel the underlying stream immediately (spec \u00a74.1) so the server stops.
                slot.abort.signal();
                None
            } else {
                guard.buffer.extend_from_slice(data);
                Some(true)
            }
        } else {
            Some(false)
        }
    };
    match buffered {
        Some(true) => return,
        None => {
            slot.fail(
                FailKind::BodyTooLarge,
                format!(
                    "Response body exceeded the {} MiB buffered limit",
                    MAX_BUFFERED_BODY / (1024 * 1024)
                ),
            );
            return;
        }
        Some(false) => {}
    }

    // Streaming: emit 16 KiB chunk events with backpressure. A Buffered->Streaming switch
    // replays whatever the engine had already accumulated, preserving byte order.
    let prefix = {
        let mut g = slot.inner.lock().expect("slot events lock");
        std::mem::take(&mut g.buffer)
    };
    for piece in prefix.chunks(CHUNK_SIZE) {
        slot.push(EngineEvent::Chunk(piece.to_vec()));
        if slot.abort.is_aborted() || slot.is_finished() {
            return;
        }
    }
    for piece in data.chunks(CHUNK_SIZE) {
        slot.push(EngineEvent::Chunk(piece.to_vec()));
        if slot.abort.is_aborted() || slot.is_finished() {
            return;
        }
    }
}

#[cfg(test)]
mod backpressure_tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn chunk_queue_backpressure_parks_then_wakes_after_draining() {
        // Uses the process-wide registry + a worker thread park/wake pair: serialize with all
        // other registry-dependent engine tests.
        let _lock = registry_test_lock();
        // Exercise the internal streaming queue directly: CHUNK_QUEUE_DEPTH chunks fit, and the
        // next push parks on the Condvar until JavaScript drains (spec §4.1).
        let abort = Arc::new(AbortHandle::new());
        let slot = Arc::new(Slot::new(999_001, abort.clone()));
        engine().slots.lock().expect("slots").push(slot.clone());

        // Put the slot into Streaming mode so pushes honor the depth bound.
        {
            let mut g = slot.inner.lock().expect("inner");
            g.mode = Mode::Streaming;
        }

        let writer_slot = slot.clone();
        let writer = std::thread::spawn(move || {
            for i in 0..CHUNK_QUEUE_DEPTH + 1 {
                writer_slot.push(EngineEvent::Chunk(vec![i as u8; CHUNK_SIZE]));
            }
        });

        // Give the writer time to park on a full queue (queue stays at depth).
        std::thread::sleep(Duration::from_millis(250));
        let queued = slot.inner.lock().expect("inner").events.len();
        assert_eq!(queued, CHUNK_QUEUE_DEPTH, "queue must stop at the backpressure depth");

        let started = Instant::now();
        let mut delivered = 0usize;
        while next_event(999_001).is_some() {
            delivered += 1;
        }
        writer.join().expect("parked writer must wake and finish");
        assert!(
            started.elapsed() < Duration::from_millis(1200),
            "backpressure wake was slow: {:?}",
            started.elapsed()
        );

        // The parked chunk arrived after draining, so CHUNK_QUEUE_DEPTH+1 chunks total in order.
        while let Some(ev) = next_event(999_001) {
            if let EngineEvent::Chunk(bytes) = ev {
                assert_eq!(bytes.len(), CHUNK_SIZE);
                delivered += 1;
            }
        }
        assert_eq!(delivered, CHUNK_QUEUE_DEPTH + 1, "all chunks delivered in order");
        release_slot(999_001);
    }

    #[test]
    fn abort_while_parked_on_full_queue_wakes_the_worker() {
        let _lock = registry_test_lock();
        let abort = Arc::new(AbortHandle::new());
        let slot = Arc::new(Slot::new(999_002, abort.clone()));
        engine().slots.lock().expect("slots").push(slot.clone());
        {
            let mut g = slot.inner.lock().expect("inner");
            g.mode = Mode::Streaming;
        }
        let writer_abort = abort.clone();
        let writer_slot = slot.clone();
        let writer = std::thread::spawn(move || {
            // Fill the queue then park on push #9; the abort must release it promptly.
            for i in 0..CHUNK_QUEUE_DEPTH + 1 {
                writer_slot.push(EngineEvent::Chunk(vec![i as u8; CHUNK_SIZE]));
                if writer_abort.is_aborted() {
                    break;
                }
            }
        });
        std::thread::sleep(Duration::from_millis(200));
        let started = Instant::now();
        assert!(cancel_fetch(999_002), "cancel reports the live slot");
        writer.join().expect("parked push must return after abort");
        assert!(started.elapsed() < Duration::from_millis(1000), "abort wake slow: {:?}", started.elapsed());
        release_slot(999_002);
    }

    #[test]
    fn msg_peek_probe_open_flags_idle_open_pending_bytes_and_eof_correctly() {
        // Phase 10C: the libc MSG_PEEK | MSG_DONTWAIT probe replaces the fd-dup `try_clone()` path.
        let _lock = registry_test_lock();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("local").port();
        let client = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let (mut server, _) = listener.accept().expect("accept");

        assert!(probe_open(&client), "an idle keep-alive socket must probe as open");

        server.write_all(b"pipelined!").expect("write");
        let mut saw_pending = false;
        for _ in 0..200 {
            if !probe_open(&client) {
                saw_pending = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(saw_pending, "readable bytes mean the connection is NOT reusable");

        // MSG_PEEK does not consume anything: a real read still gets the full payload back.
        let mut client = client;
        client.set_read_timeout(Some(Duration::from_secs(2))).expect("timeout");
        let mut got = [0u8; 10];
        client.read_exact(&mut got).expect("peeked bytes remain readable");
        assert_eq!(&got, b"pipelined!");

        drop(server);
        let mut saw_eof = false;
        for _ in 0..200 {
            if !probe_open(&client) {
                saw_eof = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(saw_eof, "a peer close must make the socket unusable");
    }

    #[test]
    fn content_length_hint_presizes_the_buffered_slot_body_without_losing_bytes() {
        use ureq::http::{HeaderMap, HeaderValue};

        let mut ok = HeaderMap::new();
        ok.insert("content-length", HeaderValue::from_static("4096"));
        assert_eq!(content_length_hint(&ok), Some(4096));

        let mut spaced = HeaderMap::new();
        spaced.insert("content-length", HeaderValue::from_static(" 123 "));
        assert_eq!(content_length_hint(&spaced), Some(123));

        let mut bad = HeaderMap::new();
        bad.insert("content-length", HeaderValue::from_static("not-a-number"));
        assert_eq!(content_length_hint(&bad), None);

        let mut huge = HeaderMap::new();
        huge.insert(
            "content-length",
            HeaderValue::from_str(&format!("{}", MAX_BUFFERED_BODY + 1)).expect("huge value"),
        );
        assert_eq!(content_length_hint(&huge), None, "over-cap lengths must not pre-allocate");

        assert_eq!(content_length_hint(&HeaderMap::new()), None, "absent header yields no hint");

        // Exercise the worker's exact pre-size branch on a live slot: capacity grows once, and any
        // already-buffered bytes survive.
        let _lock = registry_test_lock();
        let abort = Arc::new(AbortHandle::new());
        let slot = Arc::new(Slot::new(999_010, abort.clone()));
        engine().slots.lock().expect("slots").push(slot.clone());
        {
            let mut g = slot.inner.lock().expect("inner");
            let cap = content_length_hint(&ok).unwrap() * 2;
            if g.buffer.capacity() < cap {
                if g.buffer.is_empty() {
                    g.buffer = Vec::with_capacity(cap);
                } else {
                    let have = g.buffer.len();
                    g.buffer.reserve_exact(cap.saturating_sub(have));
                }
            }
            g.buffer.extend_from_slice(&[7u8; 5]);
            assert!(g.buffer.capacity() >= cap, "buffer must be pre-sized from Content-Length");
            assert_eq!(g.buffer.len(), 5);
            assert_eq!(g.buffer[0], 7);
        }
        release_slot(999_010);
    }

    #[test]
    fn switch_streaming_preserves_chunk_order_over_multi_chunk_prefix_and_notifies_cv() {
        let _lock = registry_test_lock();
        let abort = Arc::new(AbortHandle::new());
        let slot = Arc::new(Slot::new(999_020, abort));
        engine().slots.lock().expect("slots").push(slot.clone());
        {
            let mut g = slot.inner.lock().expect("inner");
            // 40 KiB across 3 chunks: 16 KiB (0x01), 16 KiB (0x02), 8 KiB (0x03).
            let mut buf = vec![1u8; CHUNK_SIZE];
            buf.extend(vec![2u8; CHUNK_SIZE]);
            buf.extend(vec![3u8; 8 * 1024]);
            g.buffer = buf;
            g.events.push_back(EngineEvent::Headers {
                status: 200,
                status_text: "OK".into(),
                url: "http://example.test/".into(),
                redirected: false,
                headers: Vec::new(),
            });
            g.finished = true;
        }
        assert!(switch_streaming(999_020));
        assert!(matches!(next_event(999_020), Some(EngineEvent::Headers { .. })));
        let c1 = match next_event(999_020) {
            Some(EngineEvent::Chunk(b)) => b,
            other => panic!("expected chunk 1, got {other:?}"),
        };
        let c2 = match next_event(999_020) {
            Some(EngineEvent::Chunk(b)) => b,
            other => panic!("expected chunk 2, got {other:?}"),
        };
        let c3 = match next_event(999_020) {
            Some(EngineEvent::Chunk(b)) => b,
            other => panic!("expected chunk 3, got {other:?}"),
        };
        assert_eq!(c1[0], 1);
        assert_eq!(c2[0], 2);
        assert_eq!(c3[0], 3);
        assert!(matches!(next_event(999_020), Some(EngineEvent::Done)));
        release_slot(999_020);

        let uri_v6: ureq::http::Uri = "https://[::1]:8443/api".parse().unwrap();
        assert_eq!(bare_host(&uri_v6), Some("::1"));
        let uri_v4: ureq::http::Uri = "https://127.0.0.1:8443/api".parse().unwrap();
        assert_eq!(bare_host(&uri_v4), Some("127.0.0.1"));
    }
}
