//! RFC 6455 WebSocket CLIENT (`ws://` and `wss://`) written directly on `std` sockets plus the
//! shared [`crate::tls`] rustls layer — NO external WebSocket crate (no `tungstenite`, no async
//! runtime). One worker thread per connection (256 KiB stacks, like fetch/TLS) runs a blocking
//! read/write state machine that drains the JavaScript control channel every `IO_SLICE`, so
//! `send()` / `close()` / `terminate()` never wait for a peer frame.
//!
//! Zero-allocation contract (docs/ZERO_ALLOC_GUIDELINES.md):
//! * Frame headers go into a CALLER-OWNED `[u8; 14]` **stack** array ([`encode_frame_header`]) and
//!   masking happens IN PLACE ([`apply_mask_in_place`]) — `alloc_audit.rs` proves the hot path does
//!   `allocs == 0 && bytes == 0` for 1,000 iterations (guidelines §3 "fixed stack buffers").
//! * Inbound bytes land STRAIGHT in their final owner: whole data frames read into an exact-sized
//!   owned `Vec` that is MOVED to JavaScript (`vec_to_uint8array`) or into `String::from_utf8`
//!   (no intermediate copy), while continuation/control frames reuse ONE per-connection scratch
//!   buffer. There is no header+body concat and no second memcpy of a 70 KiB binary frame.
//! * The upgrade request head is composed once into a reusable `Vec<u8>` (`tx_head_buf`), and
//!   plaintext frames leave through `write_vectored` `IoSlice`s (guidelines §4 zero-body-copy TX).
//! * `Sec-WebSocket-Accept` is verified with `ring::digest::SHA1_FOR_LEGACY_USE_ONLY` over a stack
//!   buffer plus hand-rolled base64 into a stack array; mask keys come from `ring::rand::SystemRandom`.
//! * Header parsing borrows spans out of one response buffer and compares names with
//!   `eq_ignore_ascii_case`, so no per-header `String` allocations happen (guidelines §1).

use std::collections::{HashMap, VecDeque};
use std::io::{IoSlice, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::Builder;
use std::time::{Duration, Instant};

use cz_hal::http_server::notify_event_loop;
use ring::rand::{SecureRandom, SystemRandom};
use rustls::pki_types::ServerName;
use rustls::{ClientConnection, StreamOwned};

use crate::tls::{build_client_config, TlsClientOptions};
use crate::url::{default_port, parse_url};

/// RFC 6455 magic GUID mixed with the handshake key to derive `Sec-WebSocket-Accept`.
pub const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Worker thread stack budget (same 256 KiB shape as Phase 8A fetch workers).
pub const WS_WORKER_STACK_BYTES: usize = 256 * 1024;

/// Hard ceiling for one inbound frame payload AND cumulative fragmented message size,
/// so a hostile peer cannot force unbounded growth on 512 MB RAM.
pub const WS_MAX_FRAME: usize = 16 * 1024 * 1024;
const WS_MAX_MESSAGE: usize = WS_MAX_FRAME;

/// Control-channel slice on blocking sockets (Phase 8C idiom): reads wake at least this often.
const IO_SLICE: Duration = Duration::from_millis(20);

/// Bounded TCP connect / TLS handshake / HTTP upgrade window.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Grace for the peer's `Close` echo once our orderly Close went out.
const CLOSE_GRACE: Duration = Duration::from_secs(2);

/// Upper bound on the upgrade response header block.
const MAX_HEAD: usize = 64 * 1024;

// RFC 6455 §5.2 opcodes.
const OP_CONT: u8 = 0x0;
const OP_TEXT: u8 = 0x1;
const OP_BINARY: u8 = 0x2;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xA;

/// One engine event queued toward the JavaScript drain (`__czWsDrainLoop`).
#[derive(Debug)]
pub enum WsEngineEvent {
    /// Handshake verified (101 + exact accept hash); `protocol` is the negotiated subprotocol.
    Open { ws_id: u64, protocol: String },
    /// A fully reassembled TEXT message (continuation frames already merged).
    Text { ws_id: u64, text: String },
    /// A fully reassembled BINARY message; ownership moves to JS with zero copies.
    Binary { ws_id: u64, bytes: Vec<u8> },
    /// Orderly or abnormal close with browser-style `code` / `reason` / `wasClean`.
    Close {
        ws_id: u64,
        code: u16,
        reason: String,
        was_clean: bool,
    },
    /// Connection-level failure (upgrade rejection, TLS trust failure, reset).
    Error { ws_id: u64, message: String },
}

/// Control-channel messages JavaScript sends into the live worker thread.
enum Control {
    Send { payload: Vec<u8>, opcode: u8 },
    Close { code: u16, reason: String },
    Destroy,
}

type DestroyFlag = Arc<AtomicBool>;

struct WsEntry {
    ctrl_tx: Sender<Control>,
    /// Extra socket handle so `terminate_ws()` can interrupt a blocked read with a shutdown.
    wake_slot: Arc<Mutex<Option<TcpStream>>>,
    destroyed: DestroyFlag,
    /// Bytes queued to the worker but not yet flushed (`WebSocket.bufferedAmount`).
    buffered: Arc<AtomicUsize>,
}

fn sockets() -> &'static Mutex<HashMap<u64, WsEntry>> {
    static S: OnceLock<Mutex<HashMap<u64, WsEntry>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

fn events() -> &'static Mutex<VecDeque<WsEngineEvent>> {
    static E: OnceLock<Mutex<VecDeque<WsEngineEvent>>> = OnceLock::new();
    E.get_or_init(|| Mutex::new(VecDeque::new()))
}

fn next_ws_id() -> u64 {
    static N: AtomicU64 = AtomicU64::new(1);
    N.fetch_add(1, Ordering::SeqCst)
}

/// Queue one event and wake `cz-runner`'s Condvar idle wait instantly (never polls).
fn push_event(ev: WsEngineEvent) {
    events().lock().expect("ws event queue lock").push_back(ev);
    notify_event_loop();
}

/// Pop the next queued WebSocket event (`None` once drained).
pub fn next_ws_event() -> Option<WsEngineEvent> {
    events().lock().expect("ws event queue lock").pop_front()
}

/// True while engine events await the JavaScript drain (event-loop liveness).
pub fn ws_has_pending() -> bool {
    events().lock().expect("ws event queue lock").len() > 0
}

/// Live WebSocket connections (handshaking or open) — keeps the event loop alive.
pub fn ws_active_count() -> usize {
    sockets().lock().expect("ws sockets lock").len()
}

/// Bytes still queued for `ws_id` (0 when everything flushed / the id is gone).
pub fn ws_buffered_amount(ws_id: u64) -> usize {
    match sockets().lock().expect("ws sockets lock").get(&ws_id) {
        Some(e) => e.buffered.load(Ordering::SeqCst),
        None => 0,
    }
}

fn deregister(ws_id: u64) {
    sockets().lock().expect("ws sockets lock").remove(&ws_id);
}

fn control_tx_of(ws_id: u64) -> Option<Sender<Control>> {
    // Never hold the registry mutex across `send`: a dead worker channel must not wedge JS.
    sockets().lock().expect("ws sockets lock").get(&ws_id).map(|e| e.ctrl_tx.clone())
}

/// Queue an outbound data frame (`OP_TEXT` / `OP_BINARY`). False when the connection is gone.
pub fn send_ws(ws_id: u64, payload: Vec<u8>, opcode: u8) -> bool {
    let len = payload.len();
    let Some(tx) = control_tx_of(ws_id) else {
        return false;
    };
    if let Some(e) = sockets().lock().expect("ws sockets lock").get(&ws_id) {
        e.buffered.fetch_add(len, Ordering::SeqCst);
    }
    tx.send(Control::Send { payload, opcode }).is_ok()
}

/// Orderly client-initiated `Close` handshake (big-endian code + UTF-8 reason).
pub fn close_ws(ws_id: u64, code: u16, reason: String) -> bool {
    let Some(tx) = control_tx_of(ws_id) else {
        return false;
    };
    tx.send(Control::Close { code, reason }).is_ok()
}

/// Abrupt teardown (no Close frame, no close_notify): a single unclean close event follows.
pub fn terminate_ws(ws_id: u64) -> bool {
    let entry = match sockets().lock().expect("ws sockets lock").remove(&ws_id) {
        Some(e) => e,
        None => return false,
    };
    entry.destroyed.store(true, Ordering::SeqCst);
    entry.ctrl_tx.send(Control::Destroy).ok();
    if let Some(sock) = entry.wake_slot.lock().expect("ws wake slot").take() {
        sock.shutdown(Shutdown::Both).ok();
    }
    true
}

/// Connect to an absolute `ws://` / `wss://` URL and return the id immediately. `open`, `message`,
/// `error` and `close` events arrive asynchronously through [`next_ws_event`].
pub fn ws_connect(
    url: &str,
    protocols: Vec<String>,
    tls: TlsClientOptions,
) -> Result<u64, String> {
    let parsed = parse_url(url, None)?;
    let view = parsed.view();
    let scheme = view.protocol();
    let secure = scheme.eq_ignore_ascii_case("wss:") || scheme.eq_ignore_ascii_case("https:");
    if !scheme.eq_ignore_ascii_case("ws:") && !secure {
        return Err(format!("WebSocket URL must be ws:// or wss:// (got {url})"));
    }
    let hostname = view.hostname();
    if hostname.is_empty() {
        return Err(format!("WebSocket URL has no host ({url})"));
    }
    let connect_host = strip_ip_brackets(hostname);
    // Default ports come from the shared URL table (ws 80 / wss 443); explicit ones are parsed.
    let port: u16 = match view.port() {
        "" => default_port(view.protocol()).ok_or("WebSocket URL needs an explicit port")?,
        p => p
            .parse::<u32>()
            .map_err(|_| "WebSocket port out of range")?
            .try_into()
            .map_err(|_| "WebSocket port out of range")?,
    };
    // Upgrade request target keeps the leading '/' plus query ('/ws?q=1').
    let resource = view.request_target().into_owned();
    // WHATWG permits userinfo in ws/wss URLs; RFC 6455 §4.1 maps it onto Basic auth (Browsers do).
    let basic_auth = if view.username().is_empty() && view.password().is_empty() {
        None
    } else {
        let pair = format!("{}:{}", view.username(), view.password());
        let mut buf = [0u8; 256];
        let n = b64_encode(pair.as_bytes(), &mut buf);
        Some(format!(
            "Basic {}",
            std::str::from_utf8(&buf[..n]).unwrap_or("")
        ))
    };

    let ws_id = next_ws_id();
    let (ctrl_tx, ctrl_rx) = channel::<Control>();
    let destroyed: DestroyFlag = Arc::new(AtomicBool::new(false));
    let wake_slot: Arc<Mutex<Option<TcpStream>>> = Arc::new(Mutex::new(None));
    sockets().lock().expect("ws sockets lock").insert(
        ws_id,
        WsEntry {
            ctrl_tx,
            wake_slot: wake_slot.clone(),
            destroyed: destroyed.clone(),
            buffered: Arc::new(AtomicUsize::new(0)),
        },
    );
    if let Err(e) = Builder::new()
        .name(format!("cz-ws-{ws_id}"))
        .stack_size(WS_WORKER_STACK_BYTES)
        .spawn(move || {
            ws_worker(
                ws_id,
                secure,
                connect_host,
                port,
                resource,
                protocols,
                basic_auth,
                tls,
                ctrl_rx,
                destroyed,
                wake_slot,
            )
        })
    {
        deregister(ws_id);
        return Err(format!("cannot spawn WebSocket worker: {e}"));
    }
    Ok(ws_id)
}

/// Drop IPv6 brackets so `TcpStream::connect` can use the literal address.
fn strip_ip_brackets(host: &str) -> String {
    if host.starts_with('[') && host.ends_with(']') {
        host[1..host.len() - 1].to_string()
    } else {
        host.to_string()
    }
}

// ---------------------------------------------------------------------------
// Zero-allocation frame primitives (PUBLIC so cz-runner/tests/alloc_audit.rs can measure them)
// ---------------------------------------------------------------------------

/// Write an RFC 6455 frame header (2 / 4 / 10 bytes plus the optional 4-byte mask key) into a
/// caller-owned **stack** array; returns how many bytes were written. **0 heap allocations.**
pub fn encode_frame_header(
    out: &mut [u8; 14],
    fin: bool,
    opcode: u8,
    payload_len: usize,
    mask_key: Option<[u8; 4]>,
) -> usize {
    let masked = mask_key.is_some();
    out[0] = if fin { 0x80 } else { 0x00 } | (opcode & 0x0F);
    let mut n = 2usize;
    if payload_len < 126 {
        out[1] = (payload_len as u8) | if masked { 0x80 } else { 0x00 };
    } else if payload_len <= u16::MAX as usize {
        out[1] = 126 | if masked { 0x80 } else { 0x00 };
        out[2..4].copy_from_slice(&(payload_len as u16).to_be_bytes());
        n = 4;
    } else {
        out[1] = 127 | if masked { 0x80 } else { 0x00 };
        out[2..10].copy_from_slice(&(payload_len as u64).to_be_bytes());
        n = 10;
    }
    if let Some(key) = mask_key {
        // The mask key rides in the same stack write, so ONE write covers header + key.
        out[n..n + 4].copy_from_slice(&key);
        n += 4;
    }
    n
}

/// XOR a client payload with its 4-byte mask key IN PLACE (RFC 6455 §5.3). **0 heap allocations.**
pub fn apply_mask_in_place(buf: &mut [u8], mask_key: [u8; 4]) {
    // 4-byte chunks keep the hot loop branch-free on Cortex-A53; only the tail masks individually.
    let mut chunks = buf.chunks_exact_mut(4);
    for c in &mut chunks {
        c[0] ^= mask_key[0];
        c[1] ^= mask_key[1];
        c[2] ^= mask_key[2];
        c[3] ^= mask_key[3];
    }
    for (i, b) in chunks.into_remainder().iter_mut().enumerate() {
        *b ^= mask_key[i & 3];
    }
}

const B64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 (with `=` padding) into a caller-owned buffer; returns the written length.
fn b64_encode(src: &[u8], dst: &mut [u8]) -> usize {
    let mut n = 0usize;
    let mut i = 0usize;
    while i + 3 <= src.len() {
        let v = (src[i] as u32) << 16 | (src[i + 1] as u32) << 8 | src[i + 2] as u32;
        dst[n] = B64_ALPHABET[(v >> 18) as usize & 63];
        dst[n + 1] = B64_ALPHABET[(v >> 12) as usize & 63];
        dst[n + 2] = B64_ALPHABET[(v >> 6) as usize & 63];
        dst[n + 3] = B64_ALPHABET[(v & 63) as usize];
        n += 4;
        i += 3;
    }
    match src.len() - i {
        1 => {
            let v = (src[i] as u32) << 16;
            dst[n] = B64_ALPHABET[(v >> 18) as usize & 63];
            dst[n + 1] = B64_ALPHABET[(v >> 12) as usize & 63];
            dst[n + 2] = b'=';
            dst[n + 3] = b'=';
            n += 4;
        }
        2 => {
            let v = (src[i] as u32) << 16 | (src[i + 1] as u32) << 8;
            dst[n] = B64_ALPHABET[(v >> 18) as usize & 63];
            dst[n + 1] = B64_ALPHABET[(v >> 12) as usize & 63];
            dst[n + 2] = B64_ALPHABET[(v >> 6) as usize & 63];
            dst[n + 3] = b'=';
            n += 4;
        }
        _ => {}
    }
    n
}

/// Derive `Sec-WebSocket-Accept` entirely on the stack: `ring` SHA-1 over key + GUID, then base64.
fn accept_hash(key: &[u8], out: &mut [u8; 32]) -> usize {
    // A handshake key is at most 24 base64 chars; the GUID is 36: one stack buffer, zero heap.
    let mut joined = [0u8; 96];
    let kl = key.len().min(joined.len() - WS_GUID.len());
    joined[..kl].copy_from_slice(&key[..kl]);
    joined[kl..kl + WS_GUID.len()].copy_from_slice(WS_GUID.as_bytes());
    let digest = ring::digest::digest(
        &ring::digest::SHA1_FOR_LEGACY_USE_ONLY,
        &joined[..kl + WS_GUID.len()],
    );
    b64_encode(digest.as_ref(), out)
}

// ---------------------------------------------------------------------------
// Session I/O: plain TCP and rustls-wrapped TCP behind ONE state machine
// ---------------------------------------------------------------------------

enum Session {
    Plain(TcpStream),
    Secure(StreamOwned<ClientConnection, TcpStream>),
}

impl Session {
    fn read_into(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Session::Plain(s) => s.read(buf),
            Session::Secure(s) => s.read(buf),
        }
    }

    /// Write bytes sequentially, surviving interrupted syscalls without allocating a temp buffer.
    fn write_all_bytes(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match self {
            Session::Plain(s) => write_resilient(s, bytes),
            Session::Secure(s) => write_resilient(s, bytes),
        }
    }

    /// Header + payload leave as two `IoSlice`s on plaintext sockets (guidelines §4 zero-body-copy
    /// TX: ONE syscall for a 70 KiB frame instead of a concatenated `header + body` Vec). rustls
    /// coalesces TLS records internally, so the secure path writes sequentially.
    fn write_frame_pieces(&mut self, header: &[u8], payload: &[u8]) -> std::io::Result<()> {
        match self {
            Session::Plain(s) => write_pieces_vectored(s, header, payload),
            Session::Secure(s) => {
                write_resilient(s, header)?;
                if !payload.is_empty() {
                    write_resilient(s, payload)?;
                }
                s.flush()
            }
        }
    }

    /// Flush rustls' queued records onto the socket, then hard-shut the TCP session down.
    fn shutdown_hard(&mut self) {
        match self {
            Session::Plain(s) => {
                s.shutdown(Shutdown::Both).ok();
            }
            Session::Secure(s) => {
                drain_outbound(s);
                s.sock.shutdown(Shutdown::Both).ok();
            }
        }
    }

    fn send_close_notify(&mut self) {
        match self {
            Session::Plain(_) => {}
            Session::Secure(s) => {
                s.conn.send_close_notify();
                drain_outbound(s);
            }
        }
    }

    /// Flush pending bytes (plaintext sockets) / rustls outbound records onto the TCP socket.
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Session::Plain(s) => s.flush(),
            Session::Secure(s) => {
                drain_outbound(s);
                Ok(())
            }
        }
    }

    fn set_timeouts(&self, read: Option<Duration>, write: Option<Duration>) {
        let sock = match self {
            Session::Plain(s) => s,
            Session::Secure(s) => &s.sock,
        };
        sock.set_read_timeout(read).ok();
        sock.set_write_timeout(write).ok();
    }
}

/// `write_all` for any writer (`TcpStream` or rustls' `StreamOwned`) that survives interrupted
/// syscalls by reslicing the remainder — no temp buffer, no allocation.
fn write_resilient<W: Write>(w: &mut W, bytes: &[u8]) -> std::io::Result<()> {
    let mut off = 0usize;
    while off < bytes.len() {
        match w.write(&bytes[off..]) {
            Ok(n) if n > 0 => off += n,
            Ok(_) => return Err(std::io::Error::from(std::io::ErrorKind::WriteZero)),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Vectored frame TX on plaintext sockets: header + payload go out as `IoSlice`s that advance in
/// place, so a payload is NEVER concatenated into a second heap buffer.
fn write_pieces_vectored(sock: &mut TcpStream, header: &[u8], payload: &[u8]) -> std::io::Result<()> {
    if payload.is_empty() {
        write_resilient(sock, header)?;
        return sock.flush();
    }
    let mut arr = [IoSlice::new(header), IoSlice::new(payload)];
    let mut slices: &mut [IoSlice<'_>] = &mut arr[..];
    let mut remaining = header.len() + payload.len();
    while remaining > 0 {
        match sock.write_vectored(slices) {
            Ok(n) if n > 0 => {
                IoSlice::advance_slices(&mut slices, n);
                remaining -= n;
            }
            Ok(_) => return Err(std::io::Error::from(std::io::ErrorKind::WriteZero)),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    sock.flush()
}

/// Hand rustls' outbound queue to the socket (bounded drain, mirrors `tls_socket`'s helper).
fn drain_outbound(s: &mut StreamOwned<ClientConnection, TcpStream>) {
    let mut guard = 0usize;
    while s.conn.wants_write() && guard < 200 {
        guard += 1;
        match s.conn.write_tls(&mut s.sock) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    s.sock.flush().ok();
}

// ---------------------------------------------------------------------------
// Worker thread
// ---------------------------------------------------------------------------

fn ws_worker(
    ws_id: u64,
    secure: bool,
    host: String,
    port: u16,
    resource: String,
    protocols: Vec<String>,
    basic_auth: Option<String>,
    opts: TlsClientOptions,
    ctrl_rx: Receiver<Control>,
    destroyed: DestroyFlag,
    wake_slot: Arc<Mutex<Option<TcpStream>>>,
) {
    // 1. TCP connect: failures surface as `error` then an unclean `close`, exactly like Browsers.
    let tcp = match TcpStream::connect((host.as_str(), port)) {
        Ok(t) => t,
        Err(e) => {
            fail(ws_id, format!("WebSocket connect to {host}:{port} failed: {e}"));
            return;
        }
    };
    if destroyed.load(Ordering::SeqCst) {
        closed(ws_id, 1006, String::new(), false);
        return;
    }
    tcp.set_nodelay(true).ok();
    if let Ok(clone) = tcp.try_clone() {
        *wake_slot.lock().expect("ws wake slot") = Some(clone);
    }

    // 2. TLS for wss:// through the shared rustls layer (IP servernames mirror node:tls).
    let mut session = if secure {
        match build_tls_session(opts, host.clone(), tcp, &ctrl_rx, &destroyed, ws_id) {
            Some(s) => s,
            None => return, // error + close already emitted
        }
    } else {
        Session::Plain(tcp)
    };

    // 3. HTTP/1.1 Upgrade request built into ONE reusable per-connection head buffer.
    session.set_timeouts(Some(HANDSHAKE_TIMEOUT), Some(HANDSHAKE_TIMEOUT));
    let rng = ring::rand::SystemRandom::new();
    let mut key_bytes = [0u8; 16];
    if rng.fill(&mut key_bytes[..]).is_err() {
        fail(ws_id, "WebSocket error: no system entropy for the handshake key".to_string());
        session.shutdown_hard();
        return;
    }
    let mut key_b64 = [0u8; 24];
    let key_len = b64_encode(&key_bytes, &mut key_b64);
    let key_text = std::str::from_utf8(&key_b64[..key_len]).unwrap_or("");

    let mut head: Vec<u8> = Vec::with_capacity(256);
    head.extend_from_slice(b"GET ");
    head.extend_from_slice(resource.as_bytes());
    head.extend_from_slice(b" HTTP/1.1\r\nHost: ");
    head.extend_from_slice(host_header(&host, port, secure).as_bytes());
    head.extend_from_slice(b"\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n");
    head.extend_from_slice(b"Sec-WebSocket-Key: ");
    head.extend_from_slice(key_text.as_bytes());
    head.extend_from_slice(b"\r\nSec-WebSocket-Version: 13\r\n");
    if !protocols.is_empty() {
        head.extend_from_slice(b"Sec-WebSocket-Protocol: ");
        for (i, p) in protocols.iter().enumerate() {
            if i > 0 {
                head.extend_from_slice(b", ");
            }
            head.extend_from_slice(p.as_bytes());
        }
        head.extend_from_slice(b"\r\n");
    }
    if let Some(auth) = &basic_auth {
        head.extend_from_slice(b"Authorization: ");
        head.extend_from_slice(auth.as_bytes());
        head.extend_from_slice(b"\r\n");
    }
    head.extend_from_slice(b"\r\n");

    if session.write_all_bytes(&head).and_then(|_| session.flush()).is_err() {
        fail(ws_id, "WebSocket error: cannot send the upgrade request".to_string());
        session.shutdown_hard();
        return;
    }

    // 4. Read the upgrade response head (stack chunk reads into one bounded buffer).
    let mut resp: Vec<u8> = Vec::with_capacity(2048);
    let mut chunk = [0u8; 512];
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    let head_end = loop {
        if destroyed.load(Ordering::SeqCst) || abort_control_only(&ctrl_rx) {
            closed(ws_id, 1006, String::new(), false);
            session.shutdown_hard();
            return;
        }
        match session.read_into(&mut chunk[..]) {
            Ok(n) if n > 0 => {
                resp.extend_from_slice(&chunk[..n]);
                if resp.len() > MAX_HEAD {
                    fail(ws_id, "WebSocket error: upgrade response headers too large".to_string());
                    session.shutdown_hard();
                    return;
                }
                if let Some(pos) = find_crlf_crlf(&resp) {
                    break pos;
                }
            }
            Ok(_) => {
                fail(ws_id, "WebSocket error: server closed during the upgrade handshake".to_string());
                session.shutdown_hard();
                return;
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) =>
            {
                if Instant::now() > deadline {
                    fail(ws_id, "WebSocket error: upgrade handshake timed out".to_string());
                    session.shutdown_hard();
                    return;
                }
            }
            Err(e) => {
                fail(ws_id, format!("WebSocket error: handshake read failed: {e}"));
                session.shutdown_hard();
                return;
            }
        }
    };

    match verify_upgrade_head(&resp[..head_end], key_text) {
        Ok(negotiated) => push_event(WsEngineEvent::Open {
            ws_id,
            protocol: negotiated.map(|s| s.to_string()).unwrap_or_default(),
        }),
        Err(msg) => {
            fail(ws_id, msg);
            session.shutdown_hard();
            return;
        }
    }

    // 5. Steady-state frame loop.
    session.set_timeouts(Some(IO_SLICE), None);
    run_frames(ws_id, &ctrl_rx, &destroyed, &mut session, &rng, (resp, head_end));
}

fn flush(session: &mut Session) -> std::io::Result<()> {
    match session {
        Session::Plain(s) => s.flush(),
        Session::Secure(s) => s.flush(),
    }
}

/// rustls client handshake for `wss://`; returns `None` after emitting `error` + `close`.
fn build_tls_session(
    opts: TlsClientOptions,
    host: String,
    mut tcp: TcpStream,
    ctrl_rx: &Receiver<Control>,
    destroyed: &DestroyFlag,
    ws_id: u64,
) -> Option<Session> {
    let cfg = match build_client_config(&opts) {
        Ok(c) => c,
        Err(msg) => {
            fail(ws_id, msg);
            tcp.shutdown(Shutdown::Both).ok();
            return None;
        }
    };
    let name = match host.parse::<std::net::IpAddr>() {
        Ok(ip) => ServerName::from(ip),
        Err(_) => match ServerName::try_from(host.as_str()) {
            Ok(_) => ServerName::try_from(host).expect("validated dns name"),
            Err(_) => {
                fail(ws_id, format!("WebSocket TLS error: invalid servername {host:?}"));
                tcp.shutdown(Shutdown::Both).ok();
                return None;
            }
        },
    };
    let mut conn = match ClientConnection::new(cfg, name) {
        Ok(c) => c,
        Err(e) => {
            fail(ws_id, format!("WebSocket TLS error: {e}"));
            tcp.shutdown(Shutdown::Both).ok();
            return None;
        }
    };
    tcp.set_read_timeout(Some(IO_SLICE)).ok();
    tcp.set_write_timeout(Some(HANDSHAKE_TIMEOUT)).ok();
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    while conn.is_handshaking() {
        if destroyed.load(Ordering::SeqCst) || abort_control_only(ctrl_rx) {
            tcp.shutdown(Shutdown::Both).ok();
            closed(ws_id, 1006, String::new(), false);
            return None;
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
                if Instant::now() > deadline {
                    fail(ws_id, "WebSocket TLS error: handshake timed out".to_string());
                    tcp.shutdown(Shutdown::Both).ok();
                    return None;
                }
            }
            Err(e) => {
                // Node semantics: an untrusted certificate gives `error` then `close`, never `open`.
                fail(ws_id, format!("WebSocket TLS handshake failed: {e}"));
                tcp.shutdown(Shutdown::Both).ok();
                return None;
            }
        }
    }
    Some(Session::Secure(StreamOwned::new(conn, tcp)))
}

/// Service only the control messages that can abort the pre-open phases.
fn abort_control_only(ctrl_rx: &Receiver<Control>) -> bool {
    while let Ok(msg) = ctrl_rx.try_recv() {
        if matches!(msg, Control::Destroy) {
            return true;
        }
    }
    false
}

fn fail(ws_id: u64, message: String) {
    push_event(WsEngineEvent::Error { ws_id, message });
    closed(ws_id, 1006, String::new(), false);
}

fn closed(ws_id: u64, code: u16, reason: String, was_clean: bool) {
    push_event(WsEngineEvent::Close {
        ws_id,
        code,
        reason,
        was_clean,
    });
    deregister(ws_id);
}

// ---------------------------------------------------------------------------
// Frame state machine
// ---------------------------------------------------------------------------

/// Decode state for one inbound frame. Everything except the payload lives on the worker stack.
struct InFrame {
    raw: [u8; 14],
    got: usize,
    need: usize,
    fin: bool,
    opcode: u8,
    masked: bool,
    key: [u8; 4],
    len: usize,
    filled: usize,
    decoded: bool,
    /// `Some` when a whole (unfragmented) DATA frame owns its exact-sized payload buffer, which is
    /// later MOVED to JavaScript — bytes are read straight into it, never memcpy'd twice.
    owned_slot: Option<Vec<u8>>,
}

impl InFrame {
    fn new() -> InFrame {
        InFrame {
            raw: [0u8; 14],
            got: 0,
            need: 2,
            fin: false,
            opcode: 0,
            masked: false,
            key: [0u8; 4],
            len: 0,
            filled: 0,
            decoded: false,
            owned_slot: None,
        }
    }

    /// Resolve a fully received header into fields. False when malformed or when an extended
    /// length needs more header bytes (`self.need > self.got` with `!self.decoded`).
    fn decode(&mut self) -> bool {
        let b0 = self.raw[0];
        let b1 = self.raw[1];
        self.fin = (b0 & 0x80) != 0;
        self.opcode = b0 & 0x0F;
        self.masked = (b1 & 0x80) != 0;
        // RFC 6455 §5.1: servers MUST NOT mask frames sent to a client. Reject immediately
        // before attempting any payload/mask read (and never touch uninitialized `raw` bytes).
        if self.masked {
            return false;
        }
        let low = b1 & 0x7F;
        // RFC 6455 §5.5: control frames (opcode >= 0x8) MUST NOT be fragmented and MUST have <=125B payload.
        if (self.opcode & 0x08) != 0 && (!self.fin || low >= 126) {
            return false;
        }
        let _base = if low < 126 {
            self.len = low as usize;
            2
        } else if low == 126 {
            if self.got < 4 {
                // Extended 16-bit length: grow the header window and keep accumulating (NOT an error).
                self.need = 4;
                return false;
            }
            self.len = u16::from_be_bytes([self.raw[2], self.raw[3]]) as usize;
            4
        } else {
            if self.got < 10 {
                // Extended 64-bit length: grow to 10 header bytes and keep accumulating.
                self.need = 10;
                return false;
            }
            let wide = u64::from_be_bytes([
                self.raw[2],
                self.raw[3],
                self.raw[4],
                self.raw[5],
                self.raw[6],
                self.raw[7],
                self.raw[8],
                self.raw[9],
            ]);
            if wide > WS_MAX_FRAME as u64 {
                return false;
            }
            self.len = wide as usize;
            10
        };
        self.need = _base;
        self.decoded = true;
        true
    }

    /// True once every payload byte of this frame has landed in its owner.
    fn payload_ready(&self) -> bool {
        self.decoded && self.len > 0 && self.filled >= self.len
    }
}

/// Steady-state byte source: drain the handshake leftovers FIRST, then fall back to socket reads.
/// Never over-reads: `dst` is sized by the caller to EXACTLY the remaining header or payload bytes
/// of the current frame, so the next frame's bytes stay at the cursor.
fn read_prefetched_or_socket(
    pre: &mut Vec<u8>,
    pre_off: &mut usize,
    session: &mut Session,
    dst: &mut [u8],
) -> std::io::Result<usize> {
    if *pre_off < pre.len() {
        let take = dst.len().min(pre.len() - *pre_off);
        dst[..take].copy_from_slice(&pre[*pre_off..*pre_off + take]);
        *pre_off += take;
        return Ok(take);
    }
    session.read_into(dst)
}

fn run_frames(
    ws_id: u64,
    ctrl_rx: &Receiver<Control>,
    destroyed: &DestroyFlag,
    session: &mut Session,
    rng: &SystemRandom,
    // Handshake leftovers: bytes that arrived COALESCED with the upgrade response (head + first
    // frames in one segment) were read into the head buffer. They MOVE here (ownership transfer,
    // zero copies) so steady-state parsing never loses a frame — a swallowed Ping once broke the
    // §5.5 auto-Pong handshake end-to-end.
    prefetch: (Vec<u8>, usize),
) {
    let mut pre = prefetch.0;
    let mut pre_off = prefetch.1;
    let mut frame = InFrame::new();
    // ONE reused scratch buffer for continuation/control payloads (guidelines §4 buffer reuse).
    let mut scratch: Vec<u8> = Vec::new();
    let mut reply: Vec<u8> = Vec::new();
    // Fragmented-message accumulator (allocated only when a fragmented message arrives).
    let mut frag: Option<(u8, Vec<u8>)> = None;
    let mut we_sent_close = false;
    let mut sent_code = 1000u16;
    let mut close_deadline: Option<Instant> = None;

    loop {
        // ---- Control channel first: JS writes/closes never wait on a peer frame ----
        while let Ok(msg) = ctrl_rx.try_recv() {
            match msg {
                Control::Send { payload, opcode } => {
                    if we_sent_close || destroyed.load(Ordering::SeqCst) {
                        continue;
                    }
                    let mut key = [0u8; 4];
                    rng.fill(&mut key[..]).ok(); // keeps the last key if RNG momentarily refuses
                    let mut hdr = [0u8; 14];
                    let hn = encode_frame_header(&mut hdr, true, opcode, payload.len(), Some(key));
                    let mut out = payload;
                    apply_mask_in_place(&mut out, key);
                    if session.write_frame_pieces(&hdr[..hn], &out).is_err() {
                        fail(ws_id, "WebSocket error: cannot send a frame".to_string());
                        session.shutdown_hard();
                        return;
                    }
                    // Credit exactly what left the socket back into `bufferedAmount`.
                    if let Some(entry) = sockets().lock().expect("ws sockets lock").get(&ws_id) {
                        entry.buffered.fetch_sub(out.len().min(entry.buffered.load(Ordering::Relaxed)), Ordering::Relaxed);
                    }
                }
                Control::Close { code, reason } => {
                    if we_sent_close || destroyed.load(Ordering::SeqCst) {
                        continue;
                    }
                    reply.clear();
                    reply.extend_from_slice(&code.to_be_bytes());
                    // RFC 6455 §5.5: Close control payload <= 125 bytes -> reason <= 123 bytes.
                    let mut r_end = reason.len().min(123);
                    while r_end > 0 && !reason.is_char_boundary(r_end) {
                        r_end -= 1;
                    }
                    reply.extend_from_slice(&reason.as_bytes()[..r_end]);
                    let mut key = [0u8; 4];
                    rng.fill(&mut key[..]).ok();
                    let rlen = reply.len();
                    let mut hdr = [0u8; 14];
                    let hn = encode_frame_header(&mut hdr, true, OP_CLOSE, rlen, Some(key));
                    apply_mask_in_place(&mut reply[..rlen], key);
                    we_sent_close = true;
                    sent_code = code;
                    close_deadline = Some(Instant::now() + CLOSE_GRACE);
                    if session.write_frame_pieces(&hdr[..hn], &reply).is_err() {
                        closed(ws_id, code, String::new(), false);
                        return;
                    }
                    flush(session).ok();
                    // RFC 6455 §7.1.1: close_notify now, then wait for the peer's Close echo.
                    session.send_close_notify();
                }
                Control::Destroy => {
                    session.shutdown_hard();
                    deregister(ws_id);
                    return;
                }
            }
        }

        // ---- Read phase: bytes land STRAIGHT into the payload's final owner ----
        if frame.got < frame.need {
            // The header accumulates in the caller-stack `[u8; 14]`, asking for EXACTLY the bytes
            // still needed: over-reading would swallow payload bytes that belong in their own
            // final owner (a dropped tail once broke Ping handling end-to-end).
            match read_prefetched_or_socket(&mut pre, &mut pre_off, session, &mut frame.raw[frame.got..frame.need]) {
                Ok(n) if n > 0 => {
                    frame.got += n;
                    if frame.got >= frame.need {
                        if !frame.decode() {
                            if !frame.decoded && frame.got < frame.need {
                                // A 4- or 10-byte extended header is still accumulating; the pump
                                // will fetch the remaining bytes on the next pass.
                                continue;
                            }
                            // Malformed extended length / oversized frame -> 1002 Protocol Error.
                            send_close_and_finish(ws_id, session, 1002);
                            return;
                        }
                        // Decide the payload's owner BEFORE reading a single payload byte, so whole
                        // data frames are read into the exact buffer that later MOVES to JavaScript.
                        let whole_data = matches!(frame.opcode, OP_TEXT | OP_BINARY)
                            && frame.fin
                            && frag.is_none();
                        if frame.len == 0 {
                            // Empty frames (Close without code, Ping without data, empty message).
                            deliver_slice(
                                ws_id,
                                &mut frag,
                                &mut reply,
                                session,
                                rng,
                                &mut we_sent_close,
                                frame.fin,
                                frame.opcode,
                                &[],
                            );
                            frame = InFrame::new();
                            // A Close/protocol failure deregisters the socket: exit the pump so no
                            // later EOF can emit a second, unclean Close event.
                            if socket_gone(ws_id) {
                                return;
                            }
                        } else if whole_data {
                            frame.owned_slot = Some(vec![0u8; frame.len]);
                        } else {
                            scratch.resize(frame.len, 0);
                        }
                    }
                    continue;
                }
                Ok(_) => {
                    // Peer closed without a Close frame -> browsers report an unclean 1006.
                    session.shutdown_hard();
                    if we_sent_close {
                        closed(ws_id, sent_code, String::new(), false);
                    } else {
                        fail(ws_id, "WebSocket connection closed unexpectedly".to_string());
                    }
                    return;
                }
                Err(e) if retryable(&e) => {
                    if check_close_deadline(ws_id, close_deadline, we_sent_close, sent_code, session) {
                        return;
                    }
                    continue;
                }
                Err(e) => {
                    if destroyed.load(Ordering::SeqCst) {
                        deregister(ws_id);
                        return;
                    }
                    session.shutdown_hard();
                    fail(ws_id, format!("WebSocket error: {e}"));
                    return;
                }
            }
        }

        if frame.decoded && frame.len > 0 && frame.filled < frame.len {
            let want = frame.len - frame.filled;
            let off = frame.filled;
            let read = match frame.owned_slot.as_mut() {
                Some(buf) => read_prefetched_or_socket(&mut pre, &mut pre_off, session, &mut buf[off..off + want]),
                None => read_prefetched_or_socket(&mut pre, &mut pre_off, session, &mut scratch[off..off + want]),
            };
            match read {
                Ok(n) if n > 0 => frame.filled += n,
                Ok(_) => {
                    session.shutdown_hard();
                    if we_sent_close {
                        closed(ws_id, sent_code, String::new(), false);
                    } else {
                        fail(ws_id, "WebSocket connection closed mid-frame".to_string());
                    }
                    return;
                }
                Err(e) if retryable(&e) => {
                    if check_close_deadline(ws_id, close_deadline, we_sent_close, sent_code, session) {
                        return;
                    }
                    continue;
                }
                Err(e) => {
                    if destroyed.load(Ordering::SeqCst) {
                        deregister(ws_id);
                        return;
                    }
                    session.shutdown_hard();
                    fail(ws_id, format!("WebSocket error: {e}"));
                    return;
                }
            }
        }

        // ---- Frame complete: unmask in place, then hand ownership over (no extra copies) ----
        if frame.payload_ready() {
            let key = frame.key;
            let masked = frame.masked;
            let fin = frame.fin;
            let op = frame.opcode;
            let len = frame.len;
            if let Some(mut owned) = frame.owned_slot.take() {
                if masked {
                    apply_mask_in_place(&mut owned[..len], key);
                }
                deliver_owned(ws_id, &mut frag, session, fin, op, owned);
            } else {
                if masked {
                    apply_mask_in_place(&mut scratch[..len], key);
                }
                deliver_slice(
                    ws_id,
                    &mut frag,
                    &mut reply,
                    session,
                    rng,
                    &mut we_sent_close,
                    fin,
                    op,
                    &scratch[..len],
                );
            }
            frame = InFrame::new();
            if socket_gone(ws_id) {
                // The handshake finished (clean Close or error): stop pumping, deregistration done.
                return;
            }
        }
    }
}


/// True once the socket entry has left the registry — i.e. `closed`, `fail` or a protocol abort
/// already finished this connection, so the pump must stop delivering events.
fn socket_gone(ws_id: u64) -> bool {
    sockets().lock().expect("ws sockets lock").get(&ws_id).is_none()
}

fn retryable(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted
    )
}

fn check_close_deadline(
    ws_id: u64,
    deadline: Option<Instant>,
    we_sent_close: bool,
    sent_code: u16,
    session: &mut Session,
) -> bool {
    if we_sent_close {
        if let Some(dl) = deadline {
            if Instant::now() > dl {
                // Our Close left the socket but the peer never echoed one: unclean, still finished.
                session.shutdown_hard();
                closed(ws_id, sent_code, String::new(), false);
                return true;
            }
        }
    }
    false
}

/// Deliver a whole (unfragmented) DATA frame whose payload buffer was read directly into its final
/// owner: ownership MOVES out (binary straight to `vec_to_uint8array`, text straight into
/// `String::from_utf8`), so no extra memcpy happens anywhere on this hot path.
fn deliver_owned(
    ws_id: u64,
    frag: &mut Option<(u8, Vec<u8>)>,
    session: &mut Session,
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
) {
    if !fin {
        if frag.is_some() {
            send_close_and_finish(ws_id, session, 1002);
            return;
        }
        if payload.len() > WS_MAX_MESSAGE {
            send_close_and_finish(ws_id, session, 1009);
            return;
        }
        // First piece of a fragmented message keeps its own buffer (still zero copies).
        *frag = Some((opcode, payload));
        return;
    }
    match frag.take() {
        Some((first_op, mut acc)) => {
            if opcode != OP_CONT {
                send_close_and_finish(ws_id, session, 1002);
                return;
            }
            if acc.len().saturating_add(payload.len()) > WS_MAX_MESSAGE {
                send_close_and_finish(ws_id, session, 1009);
                return;
            }
            // Only continuation pieces ever copy, and only once into the merged buffer.
            acc.extend_from_slice(&payload);
            dispatch_message(ws_id, first_op, acc);
        }
        None => {
            if opcode == OP_CONT {
                send_close_and_finish(ws_id, session, 1002);
                return;
            }
            dispatch_message(ws_id, opcode, payload);
        }
    }
}

/// Interpret a frame whose payload lives in the reused scratch buffer (control frames and
/// continuation pieces). Nothing is copied unless a fragmented message needs merging.
fn deliver_slice(
    ws_id: u64,
    frag: &mut Option<(u8, Vec<u8>)>,
    reply: &mut Vec<u8>,
    session: &mut Session,
    rng: &SystemRandom,
    we_sent_close: &mut bool,
    fin: bool,
    opcode: u8,
    payload: &[u8],
) {
    match opcode {
        OP_CONT => {
            if let Some((op, acc)) = frag.as_mut() {
                if acc.len().saturating_add(payload.len()) > WS_MAX_MESSAGE {
                    *frag = None;
                    send_close_and_finish(ws_id, session, 1009);
                    return;
                }
                acc.extend_from_slice(payload);
                if fin {
                    let merged = std::mem::replace(acc, Vec::new());
                    let op = *op;
                    *frag = None;
                    dispatch_message(ws_id, op, merged);
                }
            } else {
                // Unsolicited continuation (FIN=0 or FIN=1) with no fragmented message in flight -> 1002.
                send_close_and_finish(ws_id, session, 1002);
            }
        }
        OP_TEXT | OP_BINARY => {
            if frag.is_some() {
                // RFC 6455 §5.4: a new data frame cannot interleave before the fragmented message ends.
                send_close_and_finish(ws_id, session, 1002);
                return;
            }
            if !fin {
                if payload.len() > WS_MAX_MESSAGE {
                    send_close_and_finish(ws_id, session, 1009);
                    return;
                }
                *frag = Some((opcode, payload.to_vec()));
            } else {
                dispatch_message(ws_id, opcode, payload.to_vec());
            }
        }
        OP_PING => {
            if !fin || payload.len() > 125 {
                send_close_and_finish(ws_id, session, 1002);
                return;
            }
            // RFC 6455 §5.5: every Ping gets an automatic Pong echoing its payload verbatim.
            write_masked_frame(session, rng, reply, OP_PONG, payload);
        }
        OP_PONG => {
            if !fin || payload.len() > 125 {
                send_close_and_finish(ws_id, session, 1002);
            }
        }
        OP_CLOSE => {
            if !fin || payload.len() > 125 {
                send_close_and_finish(ws_id, session, 1002);
                return;
            }
            let (code, reason) = parse_close_payload(payload);
            if !*we_sent_close {
                // Peer initiated the handshake: answer with our Close frame (echoing a valid code).
                let reply_code = if code == 0 || code == 1005 || code == 1006 || code == 1015 {
                    1000
                } else {
                    code
                };
                write_masked_close(session, rng, reply_code, "", reply);
                *we_sent_close = true;
            }
            session.send_close_notify();
            session.shutdown_hard();
            closed(ws_id, code, reason, true);
        }
        other => {
            // Unknown opcode -> 1002 Protocol Error (Browsers fail the connection the same way).
            eprintln!("cz-net: WebSocket unknown opcode {other}");
            send_close_and_finish(ws_id, session, 1002);
        }
    }
}

fn dispatch_message(ws_id: u64, opcode: u8, bytes: Vec<u8>) {
    if opcode == OP_TEXT {
        // Ownership move into `String::from_utf8`: valid UTF-8 needs no extra copy at all.
        let text = match String::from_utf8(bytes) {
            Ok(t) => t,
            Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        };
        push_event(WsEngineEvent::Text { ws_id, text });
    } else {
        push_event(WsEngineEvent::Binary { ws_id, bytes });
    }
}

/// Send a masked client frame (Pong / Close echoes) using the CALLER'S reused reply buffer, so no
/// per-frame heap temporary appears anywhere.
fn write_masked_frame(
    session: &mut Session,
    rng: &SystemRandom,
    reply: &mut Vec<u8>,
    opcode: u8,
    payload: &[u8],
) {
    write_masked_frame_try(session, rng, reply, opcode, payload).ok();
}

/// Same writer, but surfaces the io Result so callers/tests can observe failures objectively.
fn write_masked_frame_try(
    session: &mut Session,
    rng: &SystemRandom,
    reply: &mut Vec<u8>,
    opcode: u8,
    payload: &[u8],
) -> std::io::Result<()> {
    let mut key = [0u8; 4];
    rng.fill(&mut key[..]).ok(); // keeps the previous key if the RNG ever refuses
    reply.clear();
    reply.extend_from_slice(payload);
    apply_mask_in_place(reply, key);
    let mut hdr = [0u8; 14];
    let hn = encode_frame_header(&mut hdr, true, opcode, reply.len(), Some(key));
    session.write_frame_pieces(&hdr[..hn], reply)
}

/// Orderly Close echo built into the CALLER'S reused reply buffer (mask applied in place).
fn write_masked_close(
    session: &mut Session,
    rng: &SystemRandom,
    code: u16,
    reason: &str,
    reply: &mut Vec<u8>,
) {
    let mut key = [0u8; 4];
    rng.fill(&mut key[..]).ok();
    reply.clear();
    reply.extend_from_slice(&code.to_be_bytes());
    reply.extend_from_slice(reason.as_bytes());
    apply_mask_in_place(reply, key);
    let mut hdr = [0u8; 14];
    let hn = encode_frame_header(&mut hdr, true, OP_CLOSE, reply.len(), Some(key));
    session.write_frame_pieces(&hdr[..hn], reply).ok();
}

/// Orderly Close frame with a 2-byte big-endian code + UTF-8 reason (RFC 6455 §7.1.1).
pub fn close_payload(code: u16, reason: &str) -> Vec<u8> {
    // Public helper for embedders/tests; the worker's hot path builds close payloads in its
    // reused scratch instead of calling this.
    let mut p = Vec::with_capacity(2 + reason.len());
    p.extend_from_slice(&code.to_be_bytes());
    p.extend_from_slice(reason.as_bytes());
    p
}

/// Parse a Close payload into `(code, reason)`. Empty payloads map to 1005 (no status present);
/// a 1-byte payload is a protocol error (1002).
pub fn parse_close_payload(payload: &[u8]) -> (u16, String) {
    if payload.len() >= 2 {
        let code = u16::from_be_bytes([payload[0], payload[1]]);
        let reason = match std::str::from_utf8(&payload[2..]) {
            Ok(t) => t.to_string(),
            Err(_) => String::from_utf8_lossy(&payload[2..]).into_owned(),
        };
        (code, reason)
    } else if payload.is_empty() {
        (1005, String::new())
    } else {
        (1002, String::new())
    }
}

/// Send our Close frame (code only, built ENTIRELY on the stack), close_notify/shutdown and the
/// failure tail. Used for 1002/1011-style abrupt endings — zero heap traffic even here.
fn send_close_and_finish(ws_id: u64, session: &mut Session, code: u16) {
    let rng = SystemRandom::new();
    let mut key = [0u8; 4];
    rng.fill(&mut key[..]).ok();
    let mut payload = [0u8; 2];
    payload.copy_from_slice(&code.to_be_bytes());
    apply_mask_in_place(&mut payload[..], key);
    let mut hdr = [0u8; 14];
    let hn = encode_frame_header(&mut hdr, true, OP_CLOSE, payload.len(), Some(key));
    session.write_frame_pieces(&hdr[..hn], &payload[..]).ok();
    session.send_close_notify();
    session.shutdown_hard();
    fail(ws_id, format!("WebSocket protocol error ({code})"));
}

/// `Host:` header value: bracketed IPv6 literals and non-default ports only.
fn host_header(host: &str, port: u16, secure: bool) -> String {
    // Cold handshake path (once per connection), so one format! is fine here.
    let bracketed = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    let default = if secure { 443u16 } else { 80u16 };
    if port == default {
        bracketed
    } else {
        format!("{bracketed}:{port}")
    }
}

// ---------------------------------------------------------------------------
// Upgrade response verification (borrows spans; no per-header String allocations)
// ---------------------------------------------------------------------------

/// Validate the upgrade response head. Returns the negotiated subprotocol slice (borrowed from the
/// single response buffer) when the server echoed one.
fn verify_upgrade_head<'h>(head: &'h [u8], key: &str) -> Result<Option<&'h str>, String> {
    let line_end = find_crlf(head).ok_or("WebSocket error: malformed upgrade response")?;
    let status_line = &head[..line_end];
    // "HTTP/1.1 101 Switching Protocols": borrow both spans with index scans (zero allocation).
    let sp1 = find_byte(status_line, b' ').ok_or("WebSocket error: malformed status line")?;
    if !status_line[..sp1].starts_with(b"HTTP/") {
        return Err("WebSocket error: server replied without an HTTP status line".to_string());
    }
    let rest = &status_line[sp1 + 1..];
    let code_slice = &rest[..find_byte(rest, b' ').unwrap_or(rest.len())];
    if code_slice.is_empty() || !code_slice.iter().all(|b| b.is_ascii_digit()) {
        return Err("WebSocket error: malformed status code".to_string());
    }
    let status: u16 = std::str::from_utf8(code_slice)
        .map_err(|_| "WebSocket error: malformed status code")?
        .parse::<u32>()
        .map_err(|_| "WebSocket error: malformed status code")?
        .try_into()
        .map_err(|_| "WebSocket error: status code out of range")?;
    if status != 101 {
        return Err(format!(
            "WebSocket error: expected 101 Switching Protocols, got {status}"
        ));
    }

    let body = &head[line_end + 2..];
    let mut pos = 0usize;
    let mut upgrade_ok = false;
    let mut connection_ok = false;
    let mut accept_ok = false;
    let mut protocol: Option<&str> = None;
    while pos < body.len() {
        let end = find_crlf(&body[pos..]).map(|i| pos + i).unwrap_or(body.len());
        let line = &body[pos..end];
        pos = if end < body.len() { end + 2 } else { body.len() };
        if line.is_empty() {
            break;
        }
        let Some(colon) = find_byte(line, b':') else {
            continue;
        };
        let name = trim_ws_bytes(&line[..colon]);
        let value = trim_ws_bytes(&line[colon + 1..]);
        if name.eq_ignore_ascii_case(b"upgrade") {
            upgrade_ok = contains_ignore(value, b"websocket");
        } else if name.eq_ignore_ascii_case(b"connection") {
            connection_ok = contains_ignore(value, b"upgrade");
        } else if name.eq_ignore_ascii_case(b"sec-websocket-accept") {
            let mut expect = [0u8; 32];
            let n = accept_hash(key.as_bytes(), &mut expect);
            // Base64 is case-sensitive; only stray whitespace tolerance here.
            accept_ok = value == &expect[..n];
        } else if name.eq_ignore_ascii_case(b"sec-websocket-protocol") && protocol.is_none() {
            let first = match find_byte(value, b',') {
                Some(i) => trim_ws_bytes(&value[..i]),
                None => value,
            };
            protocol = std::str::from_utf8(first).ok();
        }
    }
    if !upgrade_ok {
        return Err("WebSocket error: server omitted `Upgrade: websocket`".to_string());
    }
    if !connection_ok {
        return Err("WebSocket error: server omitted `Connection: Upgrade`".to_string());
    }
    if !accept_ok {
        return Err(
            "WebSocket error: Sec-WebSocket-Accept verification failed (bad handshake)".into(),
        );
    }
    Ok(protocol)
}

fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}

fn find_crlf_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

fn find_byte(buf: &[u8], needle: u8) -> Option<usize> {
    buf.iter().position(|b| *b == needle)
}

fn trim_ws_bytes(b: &[u8]) -> &[u8] {
    let mut a = 0usize;
    while a < b.len() && (b[a] <= 0x20 || b[a] == 0x7F) {
        a += 1;
    }
    let mut z = b.len();
    while z > a && (b[z - 1] <= 0x20 || b[z - 1] == 0x7F) {
        z -= 1;
    }
    &b[a..z]
}

fn contains_ignore(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || hay.len() < needle.len() {
        return false;
    }
    (0..=hay.len() - needle.len()).any(|i| {
        needle
            .iter()
            .zip(hay[i..].iter())
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
    })
}

#[cfg(test)]
mod ws_tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn frame_header_shapes_are_2_4_and_10_bytes_plus_mask_key() {
        let mut buf = [0u8; 14];
        assert_eq!(encode_frame_header(&mut buf, true, OP_TEXT, 5, None), 2);
        assert_eq!(buf[0], 0x81);
        assert_eq!(buf[1], 5);

        let key = [0x12u8, 0x34, 0x56, 0x78];
        // Short masked frames: 2-byte header + 4 mask-key bytes = 6 written.
        assert_eq!(encode_frame_header(&mut buf, false, OP_BINARY, 125, Some(key)), 6);
        assert_eq!(buf[0], 0x02);
        assert_eq!(buf[1], 125 | 0x80);
        assert_eq!(&buf[2..6], &key);

        // 16-bit length (300 B): 4-byte header (+4 mask) = 8 with a key, 4 without.
        assert_eq!(encode_frame_header(&mut buf, true, OP_BINARY, 300, Some(key)), 8);
        assert_eq!(buf[1], 126 | 0x80);
        assert_eq!(&buf[2..4], &300u16.to_be_bytes());
        assert_eq!(&buf[4..8], &key);
        assert_eq!(encode_frame_header(&mut buf, true, OP_BINARY, 300, None), 4);
        assert_eq!(&buf[2..4], &300u16.to_be_bytes());

        assert_eq!(encode_frame_header(&mut buf, true, OP_BINARY, 70_000, None), 10);
        assert_eq!(buf[1], 127);
        assert_eq!(&buf[2..10], &70_000u64.to_be_bytes());
        assert_eq!(encode_frame_header(&mut buf, true, OP_BINARY, 70_000, Some(key)), 14);
    }

    #[test]
    fn masking_is_in_place_and_self_inverse() {
        let key = [0xAAu8, 0xBB, 0xCC, 0xDD];
        let mut data: Vec<u8> = (0..130u8).collect();
        let original = data.clone();
        apply_mask_in_place(&mut data, key);
        assert_ne!(data, original);
        apply_mask_in_place(&mut data, key);
        assert_eq!(data[..], original[..]);
        // Odd lengths exercise the remainder tail too.
        let mut tiny: Vec<u8> = vec![0x11, 0x22, 0x33, 0x44, 0x55];
        apply_mask_in_place(&mut tiny, key);
        assert_eq!(tiny[4], 0x55 ^ key[0]);
    }

    fn b64_expect(src: &[u8], want: &[u8]) {
        let mut out = [0u8; 64];
        let n = b64_encode(src, &mut out);
        assert_eq!(&out[..n], want, "base64 of {:?}", String::from_utf8_lossy(src));
    }

    #[test]
    fn base64_matches_standard_reference_vectors() {
        b64_expect(b"", b"");
        b64_expect(b"f", b"Zg==");
        b64_expect(b"fo", b"Zm8=");
        b64_expect(b"foo", b"Zm9v");
        b64_expect(b"foobar", b"Zm9vYmFy"); // Node/openssl ground truth for "foobar"
        b64_expect(b"aQyZmV4Y2hhbmdl", b"YVF5Wm1WNFkyaGhibWRs");
    }

    #[test]
    fn accept_hash_matches_an_openssl_verified_vector() {
        // openssl sha1(key + GUID) | base64 -> the RFC 6455 §4.2.2 example value.
        let mut out = [0u8; 32];
        let n = accept_hash(b"x3JJHMqDLNi+G7NzPbc1mA==", &mut out);
        assert_eq!(std::str::from_utf8(&out[..n]).unwrap(), "+MjefAGSxVYYp2WQhCnGq6qS7I0=");
    }
    #[test]
    fn close_payload_and_parse_round_trip_with_edge_codes() {
        let p = close_payload(1000, "done");
        assert_eq!(&p[..2], &1000u16.to_be_bytes());
        assert_eq!(&p[2..], b"done");
        let (code, reason) = parse_close_payload(&p);
        assert_eq!(code, 1000);
        assert_eq!(reason, "done");
        assert_eq!(parse_close_payload(&[]).0, 1005);
        assert_eq!(parse_close_payload(&[0x03][..]).0, 1002);
        // Multi-byte UTF-8 reasons survive untouched.
        let u = close_payload(1001, "héllo");
        let (_, r) = parse_close_payload(&u);
        assert_eq!(r, "héllo");
    }

    #[test]
    fn upgrade_verification_accepts_only_a_true_101() {
        let key = "dGV5d2Vi";
        let mut expect = [0u8; 32];
        let n = accept_hash(key.as_bytes(), &mut expect);
        let good = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: WebSocket\r\nConnection:   upgrade\r\nsec-websocket-accept:{}\r\nSec-WebSocket-Protocol: cz-v1, extra\r\n\r\n",
            std::str::from_utf8(&expect[..n]).unwrap()
        );
        let nego = verify_upgrade_head(good.as_bytes(), key).expect("valid 101 response");
        assert_eq!(nego, Some("cz-v1"));

        assert!(verify_upgrade_head(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\n", key).is_err());
        assert!(
            verify_upgrade_head(
                b"HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: upgrade\r\nsec-websocket-accept: nope\r\n\r\n",
                key
            )
            .is_err()
        );
        assert!(verify_upgrade_head(b"garbage\r\n\r\n", key).is_err());
    }

    #[test]
    fn dead_endpoint_reports_error_then_unclean_close_and_deregisters() {
        // Nothing listens on port 1, so the worker fails fast through the event queue.
        let id = ws_connect(
            "ws://127.0.0.1:1/gone",
            vec!["cz-v1".to_string()],
            TlsClientOptions::default(),
        )
        .expect("handoff accepted");
        assert_eq!(ws_active_count(), 1);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_error = false;
        let mut saw_close = false;
        while Instant::now() < deadline && !saw_close {
            while let Some(ev) = next_ws_event() {
                match ev {
                    WsEngineEvent::Error { .. } => saw_error = true,
                    WsEngineEvent::Close { code, was_clean, .. } => {
                        assert_eq!(code, 1006);
                        assert!(!was_clean);
                        saw_close = true;
                    }
                    other => panic!("unexpected early event: {other:?}"),
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(saw_error && saw_close, "error+close must arrive for a dead endpoint");
        assert_eq!(ws_active_count(), 0);
        assert!(!ws_has_pending());
        assert!(!send_ws(id, b"x".to_vec(), OP_TEXT));
        assert!(!close_ws(id, 1000, String::new()));
        assert!(!terminate_ws(id));
    }

    #[test]
    fn loopback_echo_subprotocol_ping_and_close_handshake() {
        // Minimal RFC 6455 SERVER (unmasked server frames) built only on std + ring: it echoes our
        // text frame, sends a Ping (to prove the automatic Pong), then answers our Close echo.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ws test server");
        let port = listener.local_addr().unwrap().port();
        let srv = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let mut acc = Vec::new();
            let mut buf = [0u8; 1024];
            while find_crlf_crlf(&acc).is_none() {
                match sock.read(&mut buf[..]) {
                    Ok(n) if n > 0 => acc.extend_from_slice(&buf[..n]),
                    _ => return,
                }
            }
            let text = String::from_utf8_lossy(&acc[..find_crlf_crlf(&acc).unwrap()]).into_owned();
            let key = text
                .lines()
                .find(|l| l.to_lowercase().starts_with("sec-websocket-key:"))
                .and_then(|l| l.split_once(':').map(|(_, v)| v.trim().to_string()))
                .expect("client Sec-WebSocket-Key");
            let wanted_proto = text
                .lines()
                .find(|l| l.to_lowercase().starts_with("sec-websocket-protocol:"))
                .and_then(|l| l.split_once(':').map(|(_, v)| v.trim().split(',').next().unwrap_or("").trim().to_string()))
                .unwrap_or_default();
            let mut expect = [0u8; 32];
            let n = accept_hash(key.as_bytes(), &mut expect);
            let resp = format!(
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\nSec-WebSocket-Protocol: {wanted_proto}\r\n\r\n",
                std::str::from_utf8(&expect[..n]).unwrap_or("")
            );
            sock.write_all(resp.as_bytes()).expect("write 101");

            // Send an unmasked TEXT echo first, then a Ping (RFC 6455 §5.5) whose automatic Pong
            // the client must answer, then finish with an orderly Close(1000, "done").
            fn write_server_frame(sock: &mut TcpStream, opcode: u8, payload: &[u8]) {
                let mut hdr = [0u8; 2];
                hdr[0] = 0x80 | opcode;
                hdr[1] = payload.len() as u8;
                sock.write_all(&hdr[..2]).expect("server frame header");
                sock.write_all(payload).expect("server frame payload");
                sock.flush().ok();
            }
            write_server_frame(&mut sock, OP_TEXT, b"ECHO:ping-me");
            write_server_frame(&mut sock, OP_PING, b"ping-payload");

            // Collect the client's masked frames with NO strict ordering requirement (§5.2 lets a
            // control Pong interleave with data and the Close handshake). The measurable invariants
            // are: our TEXT got echoed back, the Ping WAS answered verbatim, and a Close arrived.
            let mut saw_text = false;
            let mut saw_pong = false;
            let mut saw_close = false;
            for i in 0..8 {
                match read_server_side_frame(&mut sock) {
                    Ok((OP_TEXT, body)) => {
                        assert_eq!(&body, b"ping-me", "client text frame carries its payload");
                        saw_text = true;
                    }
                    Ok((OP_PONG, body)) => {
                        assert_eq!(&body, b"ping-payload", "Pong echoes the Ping payload verbatim");
                        saw_pong = true;
                    }
                    Ok((OP_CLOSE, _)) => saw_close = true,
                    Ok((other, _)) => panic!("unexpected client opcode {other:#x} at frame {i}"),
                    Err(e) if retryable(&e) && i < 7 => continue,
                    Err(_) => break,
                }
                if saw_text && saw_pong && saw_close {
                    break;
                }
            }
            assert!(saw_text, "the client must send its TEXT frame");
            assert!(saw_pong, "the engine must auto-answer every Ping (§5.5)");

            // Answer the client-initiated Close handshake with our own Close(1000, "done").
            let cp = close_payload(1000, "done");
            let mut ch = [0u8; 2];
            ch[0] = 0x88;
            ch[1] = cp.len() as u8;
            sock.write_all(&ch[..2]).ok();
            sock.write_all(&cp).ok();
            sock.flush().ok();
            // Orderly teardown: READ the client's Close echo before dropping the socket. Leaving
            // unread bytes behind makes TCP send an RST, which the client reports as an unclean
            // error instead of a completed 1000 handshake.
            for _ in 0..4 {
                match read_server_side_frame(&mut sock) {
                    Ok((OP_CLOSE, _)) => break,
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
            sock.shutdown(std::net::Shutdown::Write).ok();
            std::thread::sleep(Duration::from_millis(120));
        });

        let id = ws_connect(
            &format!("ws://127.0.0.1:{port}/demo"),
            vec!["cz-v1".to_string()],
            TlsClientOptions::default(),
        )
        .expect("connect accepted");
        let mut opened = false;
        let mut echoed: Option<String> = None;
        let mut closed_evt = None;
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline && closed_evt.is_none() {
            while let Some(ev) = next_ws_event() {
                match ev {
                    WsEngineEvent::Open { protocol, .. } => {
                        assert_eq!(protocol, "cz-v1", "negotiated subprotocol surfaces on open");
                        opened = true;
                        send_ws(id, b"ping-me".to_vec(), OP_TEXT);
                    }
                    WsEngineEvent::Text { text, .. } => echoed = Some(text),
                    WsEngineEvent::Close { code, reason, was_clean, .. } => {
                        closed_evt = Some((code, reason, was_clean));
                    }
                    other => panic!("unexpected event {other:?}"),
                }
            }
            if opened && echoed.is_some() && closed_evt.is_none() {
                close_ws(id, 1000, "done".to_string());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        srv.join().expect("server thread join");
        while let Some(ev) = next_ws_event() {
            if let WsEngineEvent::Close { code, .. } = ev {
                closed_evt.get_or_insert((code, String::new(), true));
            }
        }
        assert!(opened, "open event must arrive");
        assert_eq!(echoed.as_deref(), Some("ECHO:ping-me"));
        let (code, reason, was_clean) = closed_evt.expect("close event");
        assert_eq!(code, 1000);
        assert_eq!(reason, "done");
        assert!(was_clean, "orderly close handshake reports wasClean");
        assert_eq!(ws_active_count(), 0);
    }

    /// Read one frame server-side (masked client frames get unmasked in place). Test-only helper.
    fn read_server_side_frame(sock: &mut TcpStream) -> std::io::Result<(u8, Vec<u8>)> {
        let mut h = [0u8; 2];
        sock.read_exact(&mut h)?;
        let opcode = h[0] & 0x0F;
        let masked = (h[1] & 0x80) != 0;
        let low = h[1] & 0x7F;
        let len = if low < 126 {
            low as usize
        } else if low == 126 {
            let mut e = [0u8; 2];
            sock.read_exact(&mut e)?;
            u16::from_be_bytes([e[0], e[1]]) as usize
        } else {
            let mut e = [0u8; 8];
            sock.read_exact(&mut e)?;
            u64::from_be_bytes(e) as usize
        };
        let key = if masked {
            let mut k = [0u8; 4];
            sock.read_exact(&mut k)?;
            k
        } else {
            [0u8; 4]
        };
        let mut payload = vec![0u8; len];
        sock.read_exact(&mut payload)?;
        if masked {
            apply_mask_in_place(&mut payload, key);
        }
        Ok((opcode, payload))
    }

    #[test]
    fn masked_server_frame_and_oversized_or_fragmented_control_frames_fail_with_1002() {
        let mut f = InFrame::new();
        f.raw[0] = 0x81;
        f.raw[1] = 0x85; // masked bit set from server
        f.got = 2;
        assert!(!f.decode());
        assert!(!f.decoded);
        assert_eq!(f.need, 2, "must fail immediately without expanding need for mask bytes");

        let mut f_ping_big = InFrame::new();
        f_ping_big.raw[0] = 0x89; // FIN + OP_PING
        f_ping_big.raw[1] = 126; // > 125 bytes
        f_ping_big.got = 2;
        assert!(!f_ping_big.decode());

        let mut f_ping_frag = InFrame::new();
        f_ping_frag.raw[0] = 0x09; // FIN=0 + OP_PING
        f_ping_frag.raw[1] = 0;
        f_ping_frag.got = 2;
        assert!(!f_ping_frag.decode());
    }

    #[test]
    fn silent_peer_after_close_handshake_emits_single_close_event() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ws test server");
        let port = listener.local_addr().unwrap().port();
        let srv = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            let mut acc = Vec::new();
            let mut buf = [0u8; 512];
            while find_crlf_crlf(&acc).is_none() {
                match sock.read(&mut buf[..]) {
                    Ok(n) if n > 0 => acc.extend_from_slice(&buf[..n]),
                    _ => return,
                }
            }
            let text = String::from_utf8_lossy(&acc[..find_crlf_crlf(&acc).unwrap()]).into_owned();
            let key = text
                .lines()
                .find(|l| l.to_lowercase().starts_with("sec-websocket-key:"))
                .and_then(|l| l.split_once(':').map(|(_, v)| v.trim().to_string()))
                .unwrap();
            let mut expect = [0u8; 32];
            let n = accept_hash(key.as_bytes(), &mut expect);
            let resp = format!(
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
                std::str::from_utf8(&expect[..n]).unwrap()
            );
            sock.write_all(resp.as_bytes()).unwrap();
            // Intentionally hold socket open silently and never echo Close.
            std::thread::sleep(Duration::from_millis(2600));
        });

        let id = ws_connect(&format!("ws://127.0.0.1:{port}/silent"), Vec::new(), TlsClientOptions::default())
            .expect("connect");
        let mut close_count = 0usize;
        let mut sent_close = false;
        let deadline = Instant::now() + Duration::from_secs(6);
        while Instant::now() < deadline {
            while let Some(ev) = next_ws_event() {
                match ev {
                    WsEngineEvent::Open { ws_id, .. } if ws_id == id => {
                        close_ws(id, 1000, "bye".to_string());
                        sent_close = true;
                    }
                    WsEngineEvent::Close { ws_id, .. } if ws_id == id => {
                        close_count += 1;
                    }
                    _ => {}
                }
            }
            if sent_close && close_count > 0 && ws_active_count() == 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // Wait briefly to confirm no second Close event arrives.
        std::thread::sleep(Duration::from_millis(120));
        while let Some(ev) = next_ws_event() {
            if matches!(ev, WsEngineEvent::Close { ws_id, .. } if ws_id == id) {
                close_count += 1;
            }
        }
        srv.join().ok();
        assert_eq!(close_count, 1, "must emit exactly one Close event after close deadline");
    }
}
