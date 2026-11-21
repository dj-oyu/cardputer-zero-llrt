//! Native in-memory key-value & structured-record store (`cardputer:kv`) for CardputerZero.
//!
//! All payloads (keys, scalar values, byte buffers, and per-field maps) live on the **Rust
//! heap** — never in a JS `Map`/object — so long-running multi-user servers do not inflate the
//! QuickJS heap/GC. The design mirrors the zcode KV idea but is intentionally compact:
//!
//! - [`KvValue`] is a tagged enum (`Null`/`Bool`/`Int`/`Float`/`Str`/`Bytes`) with exact byte
//!   accounting so `store.stats().bytes` reflects real native footprint.
//! - An [`Entry`] holds exactly one [`EntryPayload`] — a scalar value OR a structured field map
//!   (a record read through a zero-copy row handle) — bounded to 72 bytes so scalar entries do
//!   not pay for an empty `HashMap`, plus a monotonic LRU `seq` and optional absolute TTL in
//!   wall-clock epoch milliseconds (so snapshots survive a reboot).
//! - [`KvStore`] enforces `max_bytes` (default 1 MiB) and `max_entries` (default 1024): inserts
//!   evict least-recently-used entries until within limits and bump `evictions`. Expired entries
//!   are treated as absent and reclaimed lazily on access/stats/keys (TTL removals are *not*
//!   counted as LRU evictions).
//! - Power-loss-safe persistence writes a compact length-prefixed binary snapshot to
//!   `<path>.tmp`, `sync_all()` (fsync) it, then atomically renames over `<path>` — never on
//!   every `set()` (SD-card longevity rule).
//!
//! No panics surface from data-plane operations; only persistence I/O returns `io::Error`.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Default per-store byte cap (1 MiB).
pub const DEFAULT_MAX_BYTES: u64 = 1_048_576;
/// Default per-store entry cap.
pub const DEFAULT_MAX_ENTRIES: u64 = 1024;

// ---------------------------------------------------------------------------
// Wall-clock helper (epoch milliseconds) used for TTL + persistence
// ---------------------------------------------------------------------------

/// Current wall-clock time in milliseconds since the Unix epoch (saturating).
pub fn now_epoch_ms() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as u64,
        Err(_) => 0,
    }
}

/// Convert a positive fractional-seconds TTL into absolute expiry epoch-ms (`None` when not
/// a usable positive number). Fractional values (e.g. `0.15` => 150ms) are honored.
fn ttl_to_expiry_ms(ttl_sec: Option<f64>, now_ms: u64) -> Option<u64> {
    let s = ttl_sec?;
    if !s.is_finite() || s <= 0.0 {
        return None;
    }
    // Round to whole milliseconds but keep sub-second TTLs alive for at least 1ms.
    let ms = (s * 1000.0).round().max(1.0) as u64;
    Some(now_ms.saturating_add(ms))
}

// ---------------------------------------------------------------------------
// Compact tagged native value
// ---------------------------------------------------------------------------

/// A compact tagged native value. Stored entirely on the Rust heap.
#[derive(Clone, Debug, PartialEq)]
pub enum KvValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Box<str>),
    Bytes(Box<[u8]>),
}

impl KvValue {
    /// Approximate native footprint (tag + payload) used for byte accounting.
    pub fn size(&self) -> usize {
        match self {
            KvValue::Null => 0,
            KvValue::Bool(_) => 1,
            KvValue::Int(_) => 8,
            KvValue::Float(_) => 8,
            KvValue::Str(s) => s.len(),
            KvValue::Bytes(b) => b.len(),
        }
    }

    /// Numeric view (`None` for non-numeric values) — used by `incr`.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            KvValue::Int(i) => Some(*i as f64),
            KvValue::Float(f) => Some(*f),
            KvValue::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            _ => None,
        }
    }

    /// Build a numeric value: whole numbers become `Int`, fractional stay `Float`.
    pub fn from_f64(n: f64) -> KvValue {
        if n.is_finite() && n.fract() == 0.0 && n >= i64::MIN as f64 && n <= i64::MAX as f64 {
            KvValue::Int(n as i64)
        } else {
            KvValue::Float(n)
        }
    }

    fn tag(&self) -> u8 {
        match self {
            KvValue::Null => 0,
            KvValue::Bool(_) => 1,
            KvValue::Int(_) => 2,
            KvValue::Float(_) => 3,
            KvValue::Str(_) => 4,
            KvValue::Bytes(_) => 5,
        }
    }
}

// ---------------------------------------------------------------------------
// Binary value encode / decode (shared by entries and field maps)
// ---------------------------------------------------------------------------

fn write_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn write_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn write_value(out: &mut Vec<u8>, v: &KvValue) {
    out.push(v.tag());
    match v {
        KvValue::Null => {}
        KvValue::Bool(b) => out.push(if *b { 1 } else { 0 }),
        KvValue::Int(i) => write_u64(out, *i as u64),
        KvValue::Float(f) => write_u64(out, f.to_bits()),
        KvValue::Str(s) => {
            write_u32(out, s.len() as u32);
            out.extend_from_slice(s.as_bytes());
        }
        KvValue::Bytes(b) => {
            write_u32(out, b.len() as u32);
            out.extend_from_slice(b);
        }
    }
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], &'static str> {
        if self.pos + n > self.data.len() {
            return Err("truncated kv snapshot");
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, &'static str> {
        let b = self.take(1)?;
        Ok(b[0])
    }
    fn u32(&mut self) -> Result<u32, &'static str> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self) -> Result<u64, &'static str> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }
    fn string(&mut self) -> Result<String, &'static str> {
        let len = self.u32()? as usize;
        let raw = self.take(len)?;
        std::str::from_utf8(raw).map(|s| s.to_string()).map_err(|_| "bad utf8 string")
    }
    fn value(&mut self) -> Result<KvValue, &'static str> {
        match self.u8()? {
            0 => Ok(KvValue::Null),
            1 => Ok(KvValue::Bool(self.u8()? != 0)),
            2 => Ok(KvValue::Int(self.u64()? as i64)),
            3 => Ok(KvValue::Float(f64::from_bits(self.u64()?))),
            4 => Ok(KvValue::Str(self.string()?.into())),
            5 => {
                let len = self.u32()? as usize;
                let raw = self.take(len)?;
                Ok(KvValue::Bytes(raw.to_vec().into()))
            }
            _ => Err("unknown value tag"),
        }
    }
}

// ---------------------------------------------------------------------------
// Entry: either scalar value OR structured field map, with LRU + TTL bookkeeping
// ---------------------------------------------------------------------------

/// Unified entry payload: exactly ONE of a bare scalar OR a structured field map lives inline.
/// The old dual `Option<KvValue>` + `HashMap` layout paid ~48 bytes of empty-map overhead on
/// every scalar entry; this enum shares one 48-byte slot instead.
#[derive(Clone, Debug)]
pub enum EntryPayload {
    Scalar(KvValue),
    Record(HashMap<Box<str>, KvValue>),
}

impl EntryPayload {
    /// Payload footprint WITHOUT the owning key (field names + value bytes for records).
    fn value_bytes(&self) -> usize {
        match self {
            EntryPayload::Scalar(v) => v.size(),
            EntryPayload::Record(map) => {
                let mut c = 0;
                for (k, v) in map {
                    c += k.len() + v.size();
                }
                c
            }
        }
    }
}

/// One stored record: a scalar value OR a structured field map, plus LRU/TTL/byte bookkeeping.
/// `payload` replaces the previous `scalar` + `fields` pair, bounding `Entry` to <= 72 bytes
/// on 64-bit targets (`seq` 8 + `expires_at_ms` 16 + `EntryPayload` 48).
#[derive(Clone, Debug)]
pub struct Entry {
    /// Monotonic sequence counter (recency); lowest seq is evicted first under LRU.
    pub seq: u64,
    /// Absolute expiry in epoch milliseconds (`None` = no TTL). Persisted for reboot survival.
    pub expires_at_ms: Option<u64>,
    /// The live payload: either a scalar value or a structured field map (never both).
    pub payload: EntryPayload,
}

impl Entry {
    /// True when the record carries a structured field map (vs a bare scalar).
    pub fn is_structured(&self) -> bool {
        matches!(&self.payload, EntryPayload::Record(map) if !map.is_empty())
    }

    /// Exact native byte footprint: key length + value sizes (scalar OR all field names/values).
    fn byte_cost(&self, key_len: usize) -> usize {
        let mut c = key_len;
        match &self.payload {
            EntryPayload::Scalar(v) => c += v.size(),
            EntryPayload::Record(map) => {
                for (k, v) in map {
                    c += k.len() + v.size();
                }
            }
        }
        c
    }

    /// True when this entry's TTL has elapsed at `now_ms`.
    pub fn expired_at(&self, now_ms: u64) -> bool {
        match self.expires_at_ms {
            Some(e) => e <= now_ms,
            None => false,
        }
    }
}

/// Snapshot of store bookkeeping returned by `stats()`.
#[derive(Clone, Debug, PartialEq)]
pub struct Stats {
    pub name: String,
    pub entries: u64,
    pub bytes: u64,
    pub max_bytes: u64,
    pub max_entries: u64,
    pub evictions: u64,
    pub hits: u64,
    pub misses: u64,
}

// ---------------------------------------------------------------------------
// KvStore
// ---------------------------------------------------------------------------

/// A named Rust-native KV store. All data lives in the heap; no JS objects are involved.
#[derive(Debug)]
pub struct KvStore {
    name: String,
    max_bytes: u64,
    max_entries: u64,
    /// Monotonic LRU sequence source (bumped on every insert AND access).
    seq: u64,
    evictions: u64,
    hits: u64,
    misses: u64,
    entries: HashMap<String, Entry>,
    /// Cached running byte total (sum of entry byte costs) maintained INCREMENTALLY via per-op
    /// deltas — mutation paths never walk the whole map again.
    bytes: u64,
    /// How many entries currently carry a TTL (`0` => `purge_expired` is an O(1) no-op).
    ttl_count: usize,
    /// Conservative LOWER bound on the earliest live expiry: whenever it is still in the future
    /// nothing can have expired, so `purge_expired` returns without any scan.
    earliest_expiry_ms: Option<u64>,
    /// Optional persistence path; `flush()` writes here atomically.
    persist_path: Option<PathBuf>,
}

impl KvStore {
    /// Create a store honoring `max_bytes`/`max_entries` caps (defaults when zero/unspecified).
    pub fn new(name: &str, max_bytes: u64, max_entries: u64, persist_path: Option<PathBuf>) -> Self {
        Self {
            name: name.to_string(),
            max_bytes: if max_bytes == 0 { DEFAULT_MAX_BYTES } else { max_bytes },
            max_entries: if max_entries == 0 { DEFAULT_MAX_ENTRIES } else { max_entries },
            seq: 0,
            evictions: 0,
            hits: 0,
            misses: 0,
            entries: HashMap::new(),
            bytes: 0,
            ttl_count: 0,
            earliest_expiry_ms: None,
            persist_path,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    fn touch(&mut self, key: &str) -> bool {
        // Mark recency by bumping seq; returns whether the entry exists AND is not expired.
        let now = now_epoch_ms();
        match self.entries.get_mut(key) {
            Some(e) if !e.expired_at(now) => {
                self.seq += 1;
                e.seq = self.seq;
                true
            }
            _ => false,
        }
    }

    /// Reclaim any TTL-expired entries (does NOT count toward LRU `evictions`). Returns removed
    /// count. O(1) when no entry carries a TTL or the known earliest expiry has not elapsed yet;
    /// only a real expiry event pays for the exact rebuild of the TTL bookkeeping.
    pub fn purge_expired(&mut self) -> usize {
        if self.ttl_count == 0 {
            return 0;
        }
        let now = now_epoch_ms();
        if matches!(self.earliest_expiry_ms, Some(e) if e > now) {
            return 0;
        }
        let expired: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, e)| e.expired_at(now))
            .map(|(k, _)| k.clone())
            .collect();
        if expired.is_empty() {
            // Stale lower bound: resync exactly so the next call short-circuits again.
            self.resync_ttl_bookkeeping();
            return 0;
        }
        for k in &expired {
            if let Some(e) = self.entries.remove(k) {
                self.bytes = self.bytes.saturating_sub(e.byte_cost(k.len()) as u64);
            }
        }
        self.resync_ttl_bookkeeping();
        expired.len()
    }

    /// Exact TTL count + earliest expiry over the live entries (runs only after a real purge).
    fn resync_ttl_bookkeeping(&mut self) {
        self.ttl_count = self.entries.values().filter(|e| e.expires_at_ms.is_some()).count();
        self.earliest_expiry_ms = self.entries.values().flat_map(|e| e.expires_at_ms).min();
    }

    /// Track one entry's TTL transition (`count` stays exact; `earliest_expiry_ms` only ever moves
    /// DOWNWARD, so the fast-path check above can never wrongly skip a scan).
    #[inline]
    fn note_ttl(&mut self, old: Option<u64>, new: Option<u64>) {
        match (old.is_some(), new.is_some()) {
            (false, true) => self.ttl_count += 1,
            (true, false) => self.ttl_count = self.ttl_count.saturating_sub(1),
            _ => {}
        }
        if let Some(e) = new {
            self.earliest_expiry_ms =
                Some(match self.earliest_expiry_ms { Some(b) if b <= e => b, _ => e });
        }
    }

    /// Account one removed entry's TTL presence without breaking the lower-bound invariant.
    #[inline]
    fn forget_ttl(&mut self, e: &Entry) {
        if e.expires_at_ms.is_some() {
            self.ttl_count = self.ttl_count.saturating_sub(1);
        }
    }


    /// Evict least-recently-used entries until within both caps; counts each eviction.
    fn enforce_limits(&mut self) {
        // First shed on entry-count overflow.
        while self.entries.len() as u64 > self.max_entries {
            match self.pop_lru() {
                Some(k) => {
                    if let Some(e) = self.entries.remove(&k) {
                        self.bytes = self.bytes.saturating_sub(e.byte_cost(k.len()) as u64);
                        self.forget_ttl(&e);
                    }
                    self.evictions += 1;
                }
                None => break,
            }
        }
        // Then shed on byte-budget overflow.
        while self.bytes > self.max_bytes {
            match self.pop_lru() {
                Some(k) => {
                    if let Some(e) = self.entries.remove(&k) {
                        self.bytes = self.bytes.saturating_sub(e.byte_cost(k.len()) as u64);
                        self.forget_ttl(&e);
                    }
                    self.evictions += 1;
                }
                None => break,
            }
        }
    }

    /// Key with the lowest LRU seq (oldest access), or `None` when empty.
    fn pop_lru(&self) -> Option<String> {
        let mut best: Option<(u64, &String)> = None;
        for (k, e) in &self.entries {
            // Least-recently-used == smallest seq. Replace best only when this entry is strictly
            // older (lower seq); ties keep the already-scanned candidate.
            match best {
                Some((s, _)) if e.seq >= s => {}
                _ => best = Some((e.seq, k)),
            }
        }
        best.map(|(_, k)| k.clone())
    }

    /// Insert/replace a scalar value with optional TTL. Never touches disk. An existing key is
    /// mutated IN PLACE (no `key.to_string()` / boxed allocation) and `bytes` moves by a delta.
    pub fn set(&mut self, key: &str, value: KvValue, ttl_sec: Option<f64>) {
        let expiry = ttl_to_expiry_ms(ttl_sec, now_epoch_ms());
        self.purge_expired();
        self.seq += 1;
        let seq = self.seq;
        // TTL transition + byte delta are computed per branch, then applied once.
        let old_ex;
        let new_ex;
        if let Some(e) = self.entries.get_mut(key) {
            // Hot update path: the key String is already owned by the map, so nothing allocates.
            let before = e.payload.value_bytes();
            old_ex = e.expires_at_ms;
            new_ex = expiry;
            e.seq = seq;
            e.expires_at_ms = expiry;
            e.payload = EntryPayload::Scalar(value);
            // Direct field update keeps `bytes` disjoint from the live `entries` borrow.
            let after = e.payload.value_bytes();
            if after >= before {
                self.bytes += (after - before) as u64;
            } else {
                self.bytes = self.bytes.saturating_sub((before - after) as u64);
            }
        } else {
            let e = Entry {
                seq,
                expires_at_ms: expiry,
                payload: EntryPayload::Scalar(value),
            };
            self.bytes += e.byte_cost(key.len()) as u64;
            self.entries.insert(key.to_string(), e);
            old_ex = None;
            new_ex = expiry;
        }
        self.note_ttl(old_ex, new_ex);
        self.enforce_limits();
    }

    /// Visitor API: hands the live scalar payload (or `None`) to `f` under the store borrow, so
    /// callers render JS values directly with ZERO `KvValue` clone. Same hit/miss + recency and
    /// record-vs-scalar semantics as [`KvStore::get`].
    pub fn with_value<R>(&mut self, key: &str, f: impl FnOnce(Option<&KvValue>) -> R) -> R {
        self.purge_expired();
        if !self.touch(key) {
            self.misses += 1;
            return f(None);
        }
        self.hits += 1;
        // Structured records never answer `get`-style reads with their field map (like before).
        let v = match self.entries.get(key).map(|e| &e.payload) {
            Some(EntryPayload::Scalar(v)) => Some(v),
            _ => None,
        };
        f(v)
    }

    /// Read a scalar value (counting hit/miss, bumping recency). Expired entries are absent.
    /// Kept for backward compatibility: a thin clone over the zero-copy [`KvStore::with_value`].
    pub fn get(&mut self, key: &str) -> Option<KvValue> {
        self.with_value(key, |v| v.cloned())
    }

    pub fn has(&mut self, key: &str) -> bool {
        self.purge_expired();
        let present = self.touch(key);
        if present {
            self.hits += 1;
        } else {
            self.misses += 1;
        }
        present
    }

    /// Delete a whole entry (scalar or record). Returns whether it existed & was live.
    pub fn del(&mut self, key: &str) -> bool {
        self.purge_expired();
        match self.entries.remove(key) {
            Some(e) => {
                self.bytes = self.bytes.saturating_sub(e.byte_cost(key.len()) as u64);
                self.forget_ttl(&e);
                true
            }
            None => false,
        }
    }

    /// Keys currently live, optionally filtered by a prefix; sorted for deterministic output.
    pub fn keys(&mut self, prefix: Option<&str>) -> Vec<String> {
        self.purge_expired();
        let mut out: Vec<String> = self
            .entries
            .keys()
            .filter(|k| match prefix {
                Some(p) => k.starts_with(p),
                None => true,
            })
            .cloned()
            .collect();
        out.sort();
        out
    }

    /// Set one field on a structured record (creating the record if needed) with optional TTL.
    /// Existing keys/fields mutate IN PLACE (no key `String` or `Box<str>` allocation), and the
    /// running byte total moves by a payload delta instead of a full-map recount.
    pub fn set_field(&mut self, key: &str, field: &str, value: KvValue, ttl_sec: Option<f64>) {
        let now = now_epoch_ms();
        self.purge_expired();
        self.seq += 1;
        let seq = self.seq;
        let expiry = ttl_to_expiry_ms(ttl_sec, now);
        let old_ex;
        let new_ex;
        if let Some(e) = self.entries.get_mut(key) {
            old_ex = e.expires_at_ms;
            e.seq = seq;
            // A supplied TTL refreshes expiry; otherwise the previous TTL (if any) is kept.
            if let Some(ex) = expiry {
                e.expires_at_ms = Some(ex);
            }
            let before = e.payload.value_bytes();
            // Switching to a structured record drops any prior scalar payload.
            if !matches!(e.payload, EntryPayload::Record(_)) {
                e.payload = EntryPayload::Record(HashMap::new());
            }
            if let EntryPayload::Record(map) = &mut e.payload {
                match map.get_mut(field) {
                    // Existing field name: overwrite the value, reuse its boxed key.
                    Some(slot) => *slot = value,
                    None => {
                        map.insert(field.into(), value);
                    }
                }
            }
            let after = e.payload.value_bytes();
            if after >= before {
                self.bytes += (after - before) as u64;
            } else {
                self.bytes = self.bytes.saturating_sub((before - after) as u64);
            }
            new_ex = e.expires_at_ms;
        } else {
            let mut map: HashMap<Box<str>, KvValue> = HashMap::new();
            map.insert(field.into(), value);
            let e = Entry {
                seq,
                expires_at_ms: expiry,
                payload: EntryPayload::Record(map),
            };
            self.bytes += e.byte_cost(key.len()) as u64;
            self.entries.insert(key.to_string(), e);
            old_ex = None;
            new_ex = expiry;
        }
        self.note_ttl(old_ex, new_ex);
        self.enforce_limits();
    }

    /// Snapshot ALL live fields of a structured record in sorted field-name order WITHOUT touching
    /// LRU recency or hit/miss counters (kept for unit-test compatibility; built on the zero-clone
    /// [`KvStore::visit_record`] visitor). Returns `None` for scalar entries, missing keys and
    /// expired records.
    pub fn record_fields(&mut self, key: &str) -> Option<Vec<(String, KvValue)>> {
        let mut out: Vec<(String, KvValue)> = Vec::new();
        if self.visit_record(key, |k, v| out.push((k.to_string(), v.clone()))) {
            Some(out)
        } else {
            None
        }
    }

    /// Visitor API: walk a record's fields in sorted name order with `&str` / `&KvValue` borrows —
    /// no `KvValue` clones and no `Vec<(String, KvValue>)` allocation. Never perturbs LRU recency
    /// or hit/miss counters (used by `JSON.stringify(kv.row(...))`). Returns whether a non-empty
    /// record was visited.
    pub fn visit_record(&mut self, key: &str, mut f: impl FnMut(&str, &KvValue)) -> bool {
        self.purge_expired();
        let entry = match self.entries.get(key) {
            Some(e) => e,
            None => return false,
        };
        let map = match &entry.payload {
            EntryPayload::Record(map) if !map.is_empty() => map,
            _ => return false,
        };
        // Only borrowed field NAMEs are sorted (no value copies); names stay valid because the
        // entry lives untouched inside `entries` for the whole walk.
        let mut names: Vec<&str> = map.keys().map(|k| k.as_ref()).collect();
        names.sort_unstable();
        for name in names {
            f(name, map.get(name).expect("visited field is present"));
        }
        true
    }

    /// Read one field from a structured record (hit/miss counted, recency bumped).
    pub fn get_field(&mut self, key: &str, field: &str) -> Option<KvValue> {
        self.with_field(key, field, |v| v.cloned())
    }

    /// Visitor API: borrow ONE field of a structured record with zero `KvValue` clone, counting
    /// hit/miss and bumping recency exactly like [`KvStore::get_field`].
    pub fn with_field<R>(&mut self, key: &str, field: &str, f: impl FnOnce(Option<&KvValue>) -> R) -> R {
        self.purge_expired();
        if !self.touch(key) {
            self.misses += 1;
            return f(None);
        }
        self.hits += 1;
        let v = self.entries.get(key).and_then(|e| match &e.payload {
            EntryPayload::Record(map) => map.get(field),
            EntryPayload::Scalar(_) => None,
        });
        f(v)
    }

    /// Remove one field from a record. Returns whether it existed.
    pub fn del_field(&mut self, key: &str, field: &str) -> bool {
        self.purge_expired();
        if !self.touch(key) {
            return false;
        }
        let removed = match self.entries.get_mut(key) {
            Some(e) => match &mut e.payload {
                // Targeted delta: only the vanished field's footprint leaves the running total.
                EntryPayload::Record(map) => match map.remove(field) {
                    Some(v) => {
                        self.bytes = self.bytes.saturating_sub((field.len() + v.size()) as u64);
                        true
                    }
                    None => false,
                },
                EntryPayload::Scalar(_) => false,
            },
            None => false,
        };
        removed
    }

    /// Increment a numeric field, treating missing/non-numeric as `0`. Returns the new value.
    /// Existing keys/fields mutate IN PLACE; bytes move by a delta (no full-map recount).
    pub fn incr(&mut self, key: &str, field: &str, delta: f64, ttl_sec: Option<f64>) -> f64 {
        let now = now_epoch_ms();
        self.purge_expired();
        self.seq += 1;
        let seq = self.seq;
        let expiry = ttl_to_expiry_ms(ttl_sec, now);
        let step = if delta.is_finite() { delta } else { 1.0 };
        let old_ex;
        let new_ex;
        let next;
        if let Some(e) = self.entries.get_mut(key) {
            old_ex = e.expires_at_ms;
            e.seq = seq;
            if let Some(ex) = expiry {
                e.expires_at_ms = Some(ex);
            }
            // `incr` always targets a structured field, so the entry becomes (or stays) a Record.
            if !matches!(e.payload, EntryPayload::Record(_)) {
                e.payload = EntryPayload::Record(HashMap::new());
            }
            let before = e.payload.value_bytes();
            let cur = match &e.payload {
                EntryPayload::Record(map) => map.get(field).and_then(|v| v.as_f64()),
                EntryPayload::Scalar(_) => None,
            }
            .unwrap_or(0.0);
            next = cur + step;
            let nv = KvValue::from_f64(next);
            if let EntryPayload::Record(map) = &mut e.payload {
                match map.get_mut(field) {
                    Some(slot) => *slot = nv,
                    None => {
                        map.insert(field.into(), nv);
                    }
                }
            }
            let after = e.payload.value_bytes();
            if after >= before {
                self.bytes += (after - before) as u64;
            } else {
                self.bytes = self.bytes.saturating_sub((before - after) as u64);
            }
            new_ex = e.expires_at_ms;
        } else {
            let mut map: HashMap<Box<str>, KvValue> = HashMap::new();
            map.insert(field.into(), KvValue::from_f64(step));
            let e = Entry {
                seq,
                expires_at_ms: expiry,
                payload: EntryPayload::Record(map),
            };
            self.bytes += e.byte_cost(key.len()) as u64;
            self.entries.insert(key.to_string(), e);
            old_ex = None;
            new_ex = expiry;
            next = step;
        }
        self.note_ttl(old_ex, new_ex);
        self.enforce_limits();
        next
    }

    fn recount_total_bytes(&self) -> u64 {
        let mut t = 0u64;
        for (k, e) in &self.entries {
            t += e.byte_cost(k.len()) as u64;
        }
        t
    }

    pub fn stats(&mut self) -> Stats {
        self.purge_expired();
        Stats {
            name: self.name.clone(),
            entries: self.entries.len() as u64,
            // The running total is maintained incrementally now — no O(N) walk in `stats()`.
            bytes: self.bytes,
            max_bytes: self.max_bytes,
            max_entries: self.max_entries,
            evictions: self.evictions,
            hits: self.hits,
            misses: self.misses,
        }
    }

    /// Drop every entry (in-memory only; does not delete a persisted file).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
        self.ttl_count = 0;
        self.earliest_expiry_ms = None;
    }

    // -- Persistence -----------------------------------------------------------

    /// Serialize the store to the compact binary snapshot format (all variants + field maps).
    pub fn encode_snapshot(&self) -> Vec<u8> {
        let now = now_epoch_ms();
        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(b"CZKV");
        out.push(1u8); // version
        write_u32(&mut out, self.name.len() as u32);
        out.extend_from_slice(self.name.as_bytes());
        write_u64(&mut out, self.max_bytes);
        write_u64(&mut out, self.max_entries);
        write_u64(&mut out, self.seq);
        // Only persist non-expired entries.
        let live: Vec<(&String, &Entry)> =
            self.entries.iter().filter(|(_, e)| !e.expired_at(now)).collect();
        write_u32(&mut out, live.len() as u32);
        for (k, e) in &live {
            write_u32(&mut out, k.len() as u32);
            out.extend_from_slice(k.as_bytes());
            match e.expires_at_ms {
                Some(ex) => {
                    out.push(1);
                    write_u64(&mut out, ex);
                }
                None => out.push(0),
            }
            if e.is_structured() {
                out.push(1u8); // structured
                // Only a non-empty Record can be structured (is_structured checked above).
                let map = match &e.payload {
                    EntryPayload::Record(m) => m,
                    EntryPayload::Scalar(_) => unreachable!("structured entry has record payload"),
                };
                write_u32(&mut out, map.len() as u32);
                for (fk, fv) in map {
                    write_u32(&mut out, fk.len() as u32);
                    out.extend_from_slice(fk.as_bytes());
                    write_value(&mut out, fv);
                }
            } else {
                out.push(0u8); // scalar (an emptied Record persists as Null, like before)
                let v = match &e.payload {
                    EntryPayload::Scalar(v) => v,
                    EntryPayload::Record(_) => &KvValue::Null,
                };
                write_value(&mut out, v);
            }
        }
        out
    }

    /// Apply a snapshot to this store (replacing existing contents), preserving seq/caps.
    fn apply_snapshot(&mut self, data: &[u8]) -> Result<(), &'static str> {
        if data.len() < 5 || &data[0..4] != b"CZKV" {
            return Err("bad magic");
        }
        let version = data[4];
        if version != 1 {
            return Err("unsupported version");
        }
        // Header is exactly: 4-byte magic + 1 version byte -> reader starts at 5.
        let mut r = Reader {
            data,
            pos: 5,
        };
        // Skip the stored name/caps/seq in favor of live values for reload correctness.
        let _name = r.string()?;
        let _max_bytes = r.u64()?;
        let _max_entries = r.u64()?;
        let saved_seq = r.u64()?;
        let count = r.u32()?;
        let now = now_epoch_ms();
        self.entries.clear();
        for _ in 0..count {
            let key = r.string()?;
            let ef = r.u8()?;
            let expires = if ef == 1 { Some(r.u64()?) } else { None };
            let kind = r.u8()?;
            let payload = if kind == 1 {
                let nfields = r.u32()?;
                let mut map: HashMap<Box<str>, KvValue> = HashMap::new();
                for _ in 0..nfields {
                    let fk = r.string()?;
                    let fv = r.value()?;
                    map.insert(fk.into(), fv);
                }
                EntryPayload::Record(map)
            } else {
                EntryPayload::Scalar(r.value()?)
            };
            let e = Entry { seq: 0, expires_at_ms: expires, payload };
            if !e.expired_at(now) {
                self.entries.insert(key, e);
            }
        }
        // Resume monotonic seq above any loaded value so ordering stays sensible.
        self.seq = self.seq.max(saved_seq);
        self.bytes = self.recount_total_bytes();
        self.resync_ttl_bookkeeping();
        Ok(())
    }

    /// Load persisted entries from a snapshot file (`Ok(false)` when absent — first run).
    pub fn load_from_disk(&mut self) -> Result<bool, io::Error> {
        let path = match &self.persist_path {
            Some(p) => p.clone(),
            None => return Ok(false),
        };
        match fs::read(&path) {
            Ok(data) => {
                self.apply_snapshot(&data).map_err(|e| {
                    io::Error::new(io::ErrorKind::InvalidData, format!("kv snapshot: {e}"))
                })?;
                Ok(true)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Atomically persist: write `<path>.tmp`, `sync_all()` (fsync), then rename over `<path>`.
    /// Returns `true` when actually written, `false` for an in-memory-only store.
    pub fn flush(&mut self) -> Result<bool, io::Error> {
        let path = match &self.persist_path {
            Some(p) => p.clone(),
            None => return Ok(false),
        };
        // Make sure the parent directory exists so renames never fail on a fresh tree.
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                fs::create_dir_all(dir)?;
            }
        }
        let tmp = PathBuf::from(path.display().to_string() + ".tmp");
        let snapshot = self.encode_snapshot();
        {
            let mut f = File::create(&tmp)?;
            f.write_all(&snapshot)?;
            f.sync_all()?; // fsync before rename (power-loss-safe)
        }
        fs::rename(&tmp, &path)?;
        // Best-effort directory fsync so the rename itself is durable on SD cards.
        if let Some(dir) = path.parent() {
            if let Ok(df) = File::open(dir) {
                df.sync_all().ok();
            }
        }
        // The .tmp must not linger after a successful atomic rename.
        if tmp.exists() {
            fs::remove_file(&tmp).ok();
        }
        Ok(true)
    }

    /// Snapshot path (for tests/diagnostics).
    pub fn persist_path(&self) -> Option<&Path> {
        self.persist_path.as_deref()
    }
}

// ---------------------------------------------------------------------------
// Process-wide named registry (Arc-backed so JS handles keep stores alive)
// ---------------------------------------------------------------------------

fn registry() -> &'static Mutex<HashMap<String, Arc<Mutex<KvStore>>>> {
    static REG: OnceLock<Mutex<HashMap<String, Arc<Mutex<KvStore>>>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Registry mutation counter — lets the one-slot `peek_store` memo validate freshness without
/// re-locking (Rust-side Arc state only; safe per the JS-value memo Dead End rule).
fn reg_version() -> &'static AtomicU64 {
    static V: OnceLock<AtomicU64> = OnceLock::new();
    V.get_or_init(|| AtomicU64::new(0))
}

thread_local! {
    static PEEK_MEMO: std::cell::RefCell<Option<(String, u64, Arc<Mutex<KvStore>>)>> =
        const { std::cell::RefCell::new(None) };
}

/// Open (or reuse) a named store and return its shared handle. When `persistPath` is given and a
/// snapshot already exists on disk, non-expired entries are loaded eagerly.
pub fn open_store(
    name: &str,
    max_bytes: u64,
    max_entries: u64,
    persist_path: Option<PathBuf>,
) -> Result<Arc<Mutex<KvStore>>, io::Error> {
    let mut reg = registry().lock().expect("kv registry lock");
    if let Some(existing) = reg.get(name) {
        // Reuse the live store; a different persist path for the same name is not re-applied.
        return Ok(existing.clone());
    }
    let store = KvStore::new(name, max_bytes, max_entries, persist_path.clone());
    let arc = Arc::new(Mutex::new(store));
    if persist_path.is_some() {
        // Load eagerly but tolerate a missing first-run file.
        let mut st = arc.lock().expect("kv store lock");
        st.load_from_disk()?;
    }
    reg.insert(name.to_string(), arc.clone());
    reg_version().fetch_add(1, Ordering::Release);
    Ok(arc)
}

/// Remove (free) a named store from the registry; returns whether it existed.
pub fn drop_store(name: &str) -> bool {
    let mut reg = registry().lock().expect("kv registry lock");
    reg.remove(name).is_some()
}

/// True when a named store currently exists in the registry.
pub fn has_store(name: &str) -> bool {
    let reg = registry().lock().expect("kv registry lock");
    reg.contains_key(name)
}

/// Borrow an already-open store handle without creating it (`None` when the name is unknown).
/// Lets native consumers such as the JSON engine serialize `EntryPayload::Record` directly.
pub fn peek_store(name: &str) -> Option<Arc<Mutex<KvStore>>> {
    // One-slot memo (same idiom as the lazy registry doc() memo): w5's 2,000 same-name lookups
    // per run skip mutex+hash; freshness is pinned by the mutation version, so drop_store or a
    // newly opened store invalidate automatically.
    let ver = reg_version().load(Ordering::Acquire);
    PEEK_MEMO.with(|m| {
        if let Some((memo_name, memo_ver, store)) = &*m.borrow() {
            if *memo_ver == ver && memo_name.as_str() == name {
                return Some(store.clone());
            }
        }
        let reg = registry().lock().expect("kv registry lock");
        let found = reg.get(name).cloned();
        if let Some(arc) = &found {
            *m.borrow_mut() = Some((name.to_string(), ver, arc.clone()));
        } else {
            m.borrow_mut().take();
        }
        found
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn tmp_kv_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cz-kv-hal-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("tmp dir");
        dir
    }

    fn fresh_store(name: &str, max_bytes: u64, max_entries: u64) -> KvStore {
        KvStore::new(name, max_bytes, max_entries, None)
    }

    #[test]
    fn scalar_set_get_roundtrip_all_types() {
        let mut s = fresh_store("scalars", DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES);
        s.set("n", KvValue::Int(7), None);
        assert_eq!(s.get("n"), Some(KvValue::Int(7)));
        s.set("f", KvValue::Float(1.5), None);
        assert_eq!(s.get("f"), Some(KvValue::Float(1.5)));
        s.set("b", KvValue::Bool(true), None);
        assert_eq!(s.get("b"), Some(KvValue::Bool(true)));
        s.set("st", KvValue::Str("hello".into()), None);
        assert_eq!(s.get("st"), Some(KvValue::Str("hello".into())));
        s.set("by", KvValue::Bytes(vec![1u8, 2, 3, 255].into()), None);
        assert_eq!(s.get("by"), Some(KvValue::Bytes(vec![1u8, 2, 3, 255].into())));
        s.set("nul", KvValue::Null, None);
        // A stored Null is a live entry; has() is true and get returns Null.
        assert!(s.has("nul"));
        assert_eq!(s.get("nul"), Some(KvValue::Null));
    }

    #[test]
    fn missing_key_returns_none_and_counts_misses() {
        let mut s = fresh_store("miss", DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES);
        assert_eq!(s.get("absent"), None);
        assert!(!s.has("absent"));
        let st = s.stats();
        assert_eq!(st.misses, 2);
        assert_eq!(st.hits, 0);
    }

    #[test]
    fn del_removes_entry_and_reclaims_bytes() {
        let mut s = fresh_store("del", DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES);
        s.set("k", KvValue::Str("abcdefgh".into()), None);
        let before = s.stats().bytes;
        assert!(before >= "abcdefgh".len() as u64);
        assert!(s.del("k"));
        assert_eq!(s.stats().bytes, 0);
        assert!(!s.del("k")); // second delete is a no-op
    }

    #[test]
    fn keys_prefix_filtering_and_sorted_order() {
        let mut s = fresh_store("keys", DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES);
        s.set("user:1", KvValue::Int(1), None);
        s.set("user:2", KvValue::Int(2), None);
        s.set("other", KvValue::Int(3), None);
        let all = s.keys(None);
        assert_eq!(all, vec!["other", "user:1", "user:2"]);
        assert_eq!(s.keys(Some("user:")), vec!["user:1", "user:2"]);
    }

    #[test]
    fn setfield_getfield_build_structured_record() {
        let mut s = fresh_store("fields", DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES);
        s.set_field("u", "views", KvValue::Int(3), None);
        s.set_field("u", "name", KvValue::Str("alice".into()), None);
        assert!(s.entries.get("u").map(|e| e.is_structured()).unwrap_or(false));
        assert_eq!(s.get_field("u", "views"), Some(KvValue::Int(3)));
        assert_eq!(s.get_field("u", "name"), Some(KvValue::Str("alice".into())));
        assert_eq!(s.get_field("u", "missing"), None);
    }

    #[test]
    fn incr_treats_missing_as_zero_and_supports_delta() {
        let mut s = fresh_store("incr", DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES);
        assert_eq!(s.incr("c", "n", 1.0, None), 1.0);
        assert_eq!(s.incr("c", "n", 5.0, None), 6.0);
        // Fractional accumulate stays Float.
        assert_eq!(s.incr("c", "score", 1.5, None), 1.5);
        assert_eq!(s.incr("c", "score", 2.5, None), 4.0);
        assert_eq!(s.get_field("c", "n"), Some(KvValue::Int(6)));
    }

    #[test]
    fn del_field_removes_only_that_field() {
        let mut s = fresh_store("delfield", DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES);
        s.set_field("u", "a", KvValue::Str("x".into()), None);
        s.set_field("u", "b", KvValue::Str("y".into()), None);
        assert!(s.del_field("u", "a"));
        assert!(!s.del_field("u", "a"));
        assert_eq!(s.get_field("u", "b"), Some(KvValue::Str("y".into())));
        assert_eq!(s.get_field("u", "a"), None);
    }

    #[test]
    fn lru_eviction_under_max_entries_keeps_recent_survivors() {
        let mut s = fresh_store("lru", 1_048_576, 3);
        s.set("k1", KvValue::Str("v1".into()), None);
        s.set("k2", KvValue::Str("v2".into()), None);
        s.set("k3", KvValue::Str("v3".into()), None);
        // Touch k1 so k2 becomes least-recently-used.
        assert_eq!(s.get("k1"), Some(KvValue::Str("v1".into())));
        s.set("k4", KvValue::Str("v4".into()), None);
        assert!(!s.has("k2"));
        assert!(s.has("k1") && s.has("k3") && s.has("k4"));
        assert!(s.stats().evictions >= 1);
        assert_eq!(s.entries.len(), 3);
    }

    #[test]
    fn lru_byte_budget_eviction_counts_and_sheds_oldest() {
        // Each entry ~ key + 8KiB value; cap forces eviction of the oldest blobs.
        let mut s = fresh_store("lru_bytes", (3 * 8192 + 32) as u64, 1000);
        for i in 0..5 {
            s.set(&format!("blob{}", i), KvValue::Bytes(vec![i as u8; 8192].into()), None);
        }
        let st = s.stats();
        assert!(st.evictions > 0, "byte-budget evictions expected");
        assert!(st.bytes <= st.max_bytes, "bytes {} exceed cap {}", st.bytes, st.max_bytes);
        // The two newest survive; oldest were shed.
        assert!(s.has("blob3") && s.has("blob4"));
        assert!(!s.has("blob0"));
    }

    #[test]
    fn ttl_expiry_makes_entry_absent_and_reclaims() {
        let mut s = fresh_store("ttl", DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES);
        s.set("eph", KvValue::Int(123), Some(0.12));
        assert_eq!(s.get("eph"), Some(KvValue::Int(123)));
        std::thread::sleep(Duration::from_millis(160));
        assert_eq!(s.get("eph"), None);
        assert!(!s.has("eph"));
        // Reclaimed: the key is fully gone from the map.
        assert!(!s.entries.contains_key("eph"));
    }

    #[test]
    fn fractional_ttl_is_honored_in_milliseconds() {
        let ms = ttl_to_expiry_ms(Some(0.15), 1000).unwrap();
        assert_eq!(ms, 1150);
        assert_eq!(ttl_to_expiry_ms(Some(-1.0), 0), None);
        assert_eq!(ttl_to_expiry_ms(Some(0.0), 0), None);
        assert_eq!(ttl_to_expiry_ms(None, 0), None);
    }

    #[test]
    fn snapshot_roundtrip_all_value_variants_and_field_maps() {
        let mut s = fresh_store("snap", DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES);
        s.set("i", KvValue::Int(-9), None);
        s.set("f", KvValue::Float(2.25), None);
        s.set("b", KvValue::Bool(false), None);
        s.set("st", KvValue::Str("café ok".into()), None);
        s.set("by", KvValue::Bytes(vec![0u8, 255, 128].into()), None);
        s.set_field("u", "views", KvValue::Int(77), None);
        s.set_field("u", "theme", KvValue::Str("amber".into()), None);
        s.set_field("u", "flag", KvValue::Bool(true), None);
        s.set_field("u", "bytes", KvValue::Bytes(vec![9u8].into()), None);
        let snap = s.encode_snapshot();

        let mut t = fresh_store("other-name", DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES);
        t.apply_snapshot(&snap).expect("apply");
        assert_eq!(t.get("i"), Some(KvValue::Int(-9)));
        assert_eq!(t.get("f"), Some(KvValue::Float(2.25)));
        assert_eq!(t.get("b"), Some(KvValue::Bool(false)));
        assert_eq!(t.get("st"), Some(KvValue::Str("café ok".into())));
        assert_eq!(t.get("by"), Some(KvValue::Bytes(vec![0u8, 255, 128].into())));
        assert_eq!(t.get_field("u", "views"), Some(KvValue::Int(77)));
        assert_eq!(t.get_field("u", "theme"), Some(KvValue::Str("amber".into())));
        assert_eq!(t.get_field("u", "flag"), Some(KvValue::Bool(true)));
        assert_eq!(t.get_field("u", "bytes"), Some(KvValue::Bytes(vec![9u8].into())));
    }

    #[test]
    fn atomic_flush_write_and_load_across_store_instances() {
        let dir = tmp_kv_dir();
        let path = dir.join("state.czkv");
        let tmp_path = PathBuf::from(path.display().to_string() + ".tmp");
        fs::remove_file(&path).ok();
        fs::remove_file(&tmp_path).ok();

        let mut a = KvStore::new("p1", DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES, Some(path.clone()));
        a.set("greeting", KvValue::Str("hello_cz".into()), None);
        a.set("raw", KvValue::Bytes(vec![1u8, 2, 3, 255].into()), None);
        a.set_field("u_42", "views", KvValue::Int(77), None);
        a.set_field("u_42", "theme", KvValue::Str("amber".into()), None);
        assert!(a.flush().expect("flush"));
        assert!(path.exists(), "persisted file must exist");
        assert!(!tmp_path.exists(), ".tmp must not linger after rename");

        let mut b = KvStore::new("p2", DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES, Some(path.clone()));
        assert!(b.load_from_disk().expect("load"));
        assert_eq!(b.get("greeting"), Some(KvValue::Str("hello_cz".into())));
        assert_eq!(b.get("raw"), Some(KvValue::Bytes(vec![1u8, 2, 3, 255].into())));
        assert_eq!(b.get_field("u_42", "views"), Some(KvValue::Int(77)));
        assert_eq!(b.get_field("u_42", "theme"), Some(KvValue::Str("amber".into())));

        fs::remove_file(&path).ok();
    }

    #[test]
    fn flush_returns_false_for_in_memory_only_store() {
        let mut s = fresh_store("mem", DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES);
        s.set("x", KvValue::Int(1), None);
        assert_eq!(s.flush().unwrap(), false);
    }

    #[test]
    fn load_from_disk_missing_file_is_ok_first_run() {
        let dir = tmp_kv_dir();
        let path = dir.join("does-not-exist-yet.czkv");
        fs::remove_file(&path).ok();
        let mut s = KvStore::new("m", DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES, Some(path));
        assert_eq!(s.load_from_disk().unwrap(), false);
    }

    #[test]
    fn expired_entries_are_not_persisted() {
        let dir = tmp_kv_dir();
        let path = dir.join("ttl-persist.czkv");
        fs::remove_file(&path).ok();
        let mut s = KvStore::new("tp", DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES, Some(path.clone()));
        s.set("keep", KvValue::Str("k".into()), None);
        s.set("gone", KvValue::Str("g".into()), Some(0.05)); // expires in 50ms
        std::thread::sleep(Duration::from_millis(80));
        s.purge_expired();
        s.flush().expect("flush");
        let _raw = fs::read(&path).expect("raw");
        // Decoding by loading into a fresh store should NOT contain the expired key.
        let mut t = KvStore::new("tp2", DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES, Some(path.clone()));
        t.load_from_disk().expect("reload");
        assert_eq!(t.get("keep"), Some(KvValue::Str("k".into())));
        assert_eq!(t.get("gone"), None);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn clear_resets_entries_and_bytes() {
        let mut s = fresh_store("clear", DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES);
        s.set("a", KvValue::Int(1), None);
        s.set("b", KvValue::Int(2), None);
        s.clear();
        assert_eq!(s.entries.len(), 0);
        assert_eq!(s.bytes, 0);
        assert_eq!(s.stats().bytes, 0);
    }

    #[test]
    fn stats_reports_all_named_fields_and_shape() {
        let mut s = fresh_store("shape-store", 2048, 5);
        s.set("x", KvValue::Int(9), None);
        s.get("x");
        s.get("nope");
        let st = s.stats();
        assert_eq!(st.name, "shape-store");
        assert_eq!(st.entries, 1);
        assert!(st.bytes >= 1);
        assert_eq!(st.max_bytes, 2048);
        assert_eq!(st.max_entries, 5);
        assert_eq!(st.hits, 1);
        assert_eq!(st.misses, 1);
        // evictions field present (may be 0 here)
        assert!(st.evictions <= 1);
    }

    #[test]
    fn byte_accounting_matches_native_payload() {
        let mut s = fresh_store("acct", DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES);
        // Key + value bytes must both count toward running totals.
        s.set("kk", KvValue::Str("abcdef".into()), None);
        assert_eq!(s.stats().bytes, ("kk".len() + "abcdef".len()) as u64);
    }

    #[test]
    fn registry_open_reuses_same_named_store_and_drop_frees_it() {
        let a = open_store("reg-shared", DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES, None).expect("open");
        {
            let mut st = a.lock().unwrap();
            st.set("shared", KvValue::Int(1), None);
        }
        let b = open_store("reg-shared", DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES, None).expect("reopen");
        // Same Arc -> same live store, so the mutation is visible.
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(b.lock().unwrap().get("shared"), Some(KvValue::Int(1)));
        assert!(has_store("reg-shared"));
        assert!(drop_store("reg-shared"));
        assert!(!has_store("reg-shared"));
        assert!(!drop_store("reg-shared"));
    }

    #[test]
    fn byte_cost_includes_field_names_and_values() {
        let mut map: HashMap<Box<str>, KvValue> = HashMap::new();
        map.insert("views".into(), KvValue::Int(1));
        map.insert("name".into(), KvValue::Str("bob".into()));
        let e = Entry { seq: 1, expires_at_ms: None, payload: EntryPayload::Record(map) };
        // key len 2 + (views5+int8) + (name4+str3)
        assert_eq!(e.byte_cost(2), 2 + (5 + 8) + (4 + 3));
        // A bare scalar costs only its own bytes — no empty-map overhead.
        let s = Entry {
            seq: 1,
            expires_at_ms: None,
            payload: EntryPayload::Scalar(KvValue::Str("abcd".into())),
        };
        assert_eq!(s.byte_cost(2), 6);
    }

    /// Layout guard: `Entry` must stay at <= 72 bytes on 64-bit targets after the
    /// `EntryPayload` unification (seq 8 + Option<u64> 16 + payload 48).
    #[test]
    fn entry_payload_layout_stays_within_72_bytes() {
        assert_eq!(std::mem::size_of::<usize>(), 8, "64-bit target expected");
        assert!(std::mem::size_of::<EntryPayload>() <= 48);
        let size = std::mem::size_of::<Entry>();
        assert!(size <= 72, "Entry grew to {size} bytes (target <= 72)");
        // The old dual-field layout was strictly larger than the unified enum.
        let old_like = std::mem::size_of::<(u64, Option<u64>, Option<KvValue>, HashMap<Box<str>, KvValue>)>();
        assert!(size < old_like, "unified Entry ({size}) should beat dual layout ({old_like})");
    }

    #[test]
    fn scalar_to_record_payload_conversion_drops_scalar_and_keeps_liveness() {
        let mut s = fresh_store("payload_conv", DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES);
        s.set("u", KvValue::Str("12345".into()), None);
        assert_eq!(s.get("u"), Some(KvValue::Str("12345".into())));
        assert_eq!(s.stats().bytes, ("u".len() + 5) as u64);
        s.set_field("u", "views", KvValue::Int(8), None);
        assert!(s.entries.get("u").map(|e| e.is_structured()).unwrap_or(false));
        // Structured records never resurrect a stale scalar (same semantics as before).
        assert_eq!(s.get("u"), None);
        assert!(s.has("u"));
        assert_eq!(s.stats().bytes, ("u".len() + "views".len() + 8) as u64);
        assert_eq!(s.get_field("u", "views"), Some(KvValue::Int(8)));
        // Deleting the last field keeps the entry live as an emptied Record.
        assert!(s.del_field("u", "views"));
        assert!(s.has("u"));
        assert_eq!(s.get_field("u", "views"), None);
    }

    #[test]
    fn visitor_api_reads_payloads_with_identical_hit_miss_accounting() {
        let mut s = fresh_store("visitor_read", DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES);
        s.set("sc", KvValue::Int(21), None);
        // with_value hands the BORROWED scalar to the callback (no clone needed to read it).
        let doubled = s.with_value("sc", |v| v.and_then(|x| x.as_f64()).map(|n| n * 2.0));
        assert_eq!(doubled, Some(42.0));
        assert_eq!(s.with_value("absent", |v| v.is_some()), false);
        // Structured records answer scalar reads with None through the visitor too.
        s.set_field("rec", "n", KvValue::Int(3), None);
        assert_eq!(s.with_value("rec", |v| v.is_some()), false);
        assert_eq!(s.with_field("rec", "n", |v| v.cloned()), Some(KvValue::Int(3)));
        assert_eq!(s.with_field("rec", "ghost", |v| v.is_some()), false);
        assert_eq!(s.with_field("sc", "n", |v| v.is_some()), false);
        let st = s.stats();
        // Legacy semantics kept intact: touching a LIVE key counts as a hit even when the asked
        // member is absent (records answer scalar reads with None, missing fields read undefined).
        // hits: sc, rec-as-scalar, rec.n, rec.ghost, sc-as-record = 5; misses: absent key = 1.
        assert_eq!(st.hits, 5, "hits {:?}", st);
        assert_eq!(st.misses, 1, "misses {:?}", st);
    }

    #[test]
    fn visit_record_walks_sorted_field_names_without_perturbing_stats() {
        let mut s = fresh_store("visitor_record", DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES);
        s.set_field("u", "zeta", KvValue::Int(1), None);
        s.set_field("u", "alpha", KvValue::Str("a".into()), None);
        s.set_field("u", "mid", KvValue::Bool(true), None);
        let before = s.stats();
        let mut seen: Vec<String> = Vec::new();
        let found = s.visit_record("u", |k, v| {
            seen.push(k.to_string());
            // Borrowed values are readable in place (sorted name order, no snapshot Vec).
            assert!(!k.is_empty());
            let _ = v.size();
        });
        assert!(found);
        assert_eq!(seen, vec!["alpha", "mid", "zeta"]);
        let after = s.stats();
        // Row stringify must not bump recency or hit/miss counters.
        assert_eq!(after.hits, before.hits);
        assert_eq!(after.misses, before.misses);
        // Scalar entries and missing keys visit nothing.
        s.set("scalar", KvValue::Int(9), None);
        assert!(!s.visit_record("scalar", |_k, _v| unreachable!("scalar has no fields")));
        assert!(!s.visit_record("missing", |_k, _v| unreachable!("missing key visits nothing")));
        // Backward-compatible record_fields keeps the same sorted shape.
        assert_eq!(
            s.record_fields("u"),
            Some(vec![
                ("alpha".to_string(), KvValue::Str("a".into())),
                ("mid".to_string(), KvValue::Bool(true)),
                ("zeta".to_string(), KvValue::Int(1)),
            ])
        );
    }

    #[test]
    fn incremental_byte_accounting_tracks_every_mutation_without_recounting() {
        let mut s = fresh_store("acct_delta", DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES);
        // Repeated updates to the SAME key/field: running total must stay exactly right while
        // mutation paths never walk the whole map (`recount_total_bytes` is the oracle here).
        for i in 0..200 {
            s.set("counter", KvValue::Int(i), None);
            s.set_field("rec", "n", KvValue::Str(format!("row-{}", i).into()), None);
            s.incr("rec", "hits", 1.0, None);
            assert_eq!(s.bytes, s.recount_total_bytes(), "diverged at step {i}");
        }
        assert_eq!(s.get("counter"), Some(KvValue::Int(199)));
        assert_eq!(s.get_field("rec", "n"), Some(KvValue::Str("row-199".into())));
        assert_eq!(s.get_field("rec", "hits"), Some(KvValue::Int(200)));
        assert_eq!(s.stats().bytes, s.recount_total_bytes());
        // Field removal and whole-entry delete shed exactly their footprint.
        let field_cost = "n".len() + "row-199".len();
        let before = s.bytes;
        assert!(s.del_field("rec", "n"));
        assert_eq!(s.bytes, before - field_cost as u64);
        assert_eq!(s.bytes, s.recount_total_bytes());
        assert!(s.del("counter"));
        assert_eq!(s.bytes, s.recount_total_bytes());
        // Record -> scalar conversion drops the field map from the total.
        let mut t = fresh_store("acct_convert", DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES);
        t.set_field("x", "aaaa", KvValue::Bytes(vec![1u8, 2, 3].into()), None);
        t.set("x", KvValue::Str("zz".into()), None);
        assert_eq!(t.bytes, ("x".len() + 2) as u64);
        assert_eq!(t.bytes, t.recount_total_bytes());
    }

    #[test]
    fn ttl_purge_short_circuits_when_nothing_has_expired() {
        let mut s = fresh_store("ttl_fast", DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES);
        // No TTL entries at all: purge is an O(1) no-op and bookkeeping stays pristine.
        for i in 0..50 {
            s.set(&format!("plain{}", i), KvValue::Int(i as i64), None);
        }
        assert_eq!(s.ttl_count, 0);
        assert_eq!(s.earliest_expiry_ms, None);
        assert_eq!(s.purge_expired(), 0);
        // A far-future TTL sets the earliest-expiry lower bound; purge still skips the scan.
        s.set("far", KvValue::Int(1), Some(3600.0));
        assert_eq!(s.ttl_count, 1);
        assert!(s.earliest_expiry_ms.is_some());
        assert_eq!(s.purge_expired(), 0);
        assert_eq!(s.get("far"), Some(KvValue::Int(1)));
        // Refreshing a TTL keeps the count stable (no double counting).
        s.set("far", KvValue::Int(2), Some(3600.0));
        assert_eq!(s.ttl_count, 1);
        s.set_field("rec", "f", KvValue::Int(1), Some(3600.0));
        assert_eq!(s.ttl_count, 2);
        // Clearing the TTL on an update decrements exactly once.
        s.set("far", KvValue::Int(3), None);
        assert_eq!(s.ttl_count, 1);
        assert_eq!(s.get("far"), Some(KvValue::Int(3)));
        // A short TTL really expires and the resync leaves exact bookkeeping behind.
        s.set("eph", KvValue::Str("bye".into()), Some(0.05));
        std::thread::sleep(Duration::from_millis(90));
        assert_eq!(s.ttl_count, 2); // `rec.f` + `eph` carry TTLs before the purge
        assert_eq!(s.purge_expired(), 1, "only the elapsed entry is reclaimed");
        assert!(!s.entries.contains_key("eph"));
        // The post-purge resync is exact: count + earliest expiry match the live entries.
        assert_eq!(s.ttl_count, s.entries.values().filter(|e| e.expires_at_ms.is_some()).count());
        assert_eq!(
            s.earliest_expiry_ms,
            s.entries.values().flat_map(|e| e.expires_at_ms).min()
        );
        assert!(!s.entries.contains_key("eph"));
        // Deleting a TTL'd entry frees its count too.
        assert!(s.del("rec"));
        assert_eq!(s.ttl_count, 0);
        assert_eq!(s.purge_expired(), 0);
    }

    #[test]
    fn record_payload_survives_snapshot_roundtrip_and_stays_record() {
        let mut s = fresh_store("payload_snap", DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES);
        s.set_field("rec", "a", KvValue::Int(2), None);
        s.set_field("rec", "b", KvValue::Bool(true), None);
        let snap = s.encode_snapshot();
        let mut t = fresh_store("payload_snap_b", DEFAULT_MAX_BYTES, DEFAULT_MAX_ENTRIES);
        t.apply_snapshot(&snap).expect("apply");
        assert!(t.entries.get("rec").map(|e| matches!(e.payload, EntryPayload::Record(_))).unwrap_or(false));
        assert_eq!(t.get_field("rec", "a"), Some(KvValue::Int(2)));
        assert_eq!(t.get_field("rec", "b"), Some(KvValue::Bool(true)));
    }
}
