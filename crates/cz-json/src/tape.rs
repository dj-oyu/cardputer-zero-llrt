//! Layer 1 — the 16-byte arena `TapeNode` document and its adaptive indexing.
//!
//! Nodes live in ONE contiguous `Vec<TapeNode>` reserved as `Vec::with_capacity(input.len() / 12)`
//! (roughly one malloc per document) in pre-order: children of an Object/Array are appended
//! immediately after their parent, each node's `skip_to` is the index just past its whole subtree
//! (so siblings are stepped in O(1)), and `end` carries the direct child count for containers.
//! Object key lookup adapts to width: `<= 16` children scan linearly behind the `key_fp: u16`
//! prefilter (zero extra memory, L1 friendly), `> 16` children build and cache a
//! `HashMap<u64, u32>` on first access.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::parse::{decode_string_content, Key, ParseErr, Sink};
use crate::simd::parse_int_fast;
use crate::val::Span;

thread_local! {
    /// One-slot reader memo for array child-offset tables (H25): lock-free hit path.
    /// Keyed by (document token, array node) — node ids are doc-relative, so a bare node key
    /// would collide across documents in one process (caught by phase5 e2e immediately).
    static ARRAY_MEMO: std::cell::RefCell<Option<(usize, u32, Arc<Vec<u32>>)>> =
        const { std::cell::RefCell::new(None) };
}

/// Unique identity per TapeDoc: monotonically increasing, never reused (no pointer-reuse risk).
fn doc_token() -> &'static AtomicUsize {
    static T: OnceLock<AtomicUsize> = OnceLock::new();
    T.get_or_init(|| AtomicUsize::new(1))
}

/// Absent-key sentinel inside `key_spans` (a real empty key `""` has span `(x, x)` with x valid).
const NO_KEY: Span = (u32::MAX, u32::MAX);

/// Node kinds (spec §1.2).
pub const KIND_OBJECT: u8 = 1;
pub const KIND_ARRAY: u8 = 2;
pub const KIND_STRING: u8 = 3;
pub const KIND_INT: u8 = 4;
pub const KIND_FLOAT: u8 = 5;
pub const KIND_BOOL: u8 = 6;
pub const KIND_NULL: u8 = 7;

/// Node flag bits.
pub const FLAG_HAS_ESCAPE: u8 = 1; // value string body holds escapes
pub const FLAG_BOOL_TRUE: u8 = 2; // Bool node carries `true`
pub const FLAG_KEY_ESCAPE: u8 = 4; // owning member key needs escape decoding

/// Compile-time proof of the exact C layout (unit tests assert it again at runtime).
const _: () = assert!(std::mem::size_of::<TapeNode>() == 16);
const _: () = assert!(std::mem::align_of::<TapeNode>() == 4);

/// One arena node: exactly 16 bytes, `#[repr(C)]`, plain-old-data (no pointers, no Drop).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TapeNode {
    pub kind: u8,
    pub flags: u8,
    pub key_fp: u16,
    pub start: u32,
    pub end: u32,
    pub skip_to: u32,
}

impl TapeNode {
    #[inline]
    pub fn is_container(&self) -> bool {
        self.kind == KIND_OBJECT || self.kind == KIND_ARRAY
    }

    #[inline]
    pub fn child_count(&self) -> usize {
        if self.is_container() {
            self.end as usize
        } else {
            0
        }
    }

    #[inline]
    pub fn is_true(&self) -> bool {
        self.kind == KIND_BOOL && (self.flags & FLAG_BOOL_TRUE) != 0
    }
}

/// Cheap 16-bit key fingerprint: length + first/last byte folded through a multiplicative mix.
/// An exact byte comparison still confirms any hit, so collisions can never return wrong data.
#[inline]
pub fn key_fingerprint(key: &str) -> u16 {
    let b = key.as_bytes();
    let first = if b.is_empty() { 0u32 } else { b[0] as u32 };
    let last = if b.is_empty() { 0u32 } else { b[b.len() - 1] as u32 };
    let mut h = (b.len() as u32) ^ ((first << 8) | last) ^ 0x9E37_79B9;
    h = h.wrapping_mul(2654435761);
    ((h >> 16) ^ h) as u16
}

/// Full 64-bit FNV-1a hash of a key (the wide-object adaptive index key).
#[inline]
pub fn hash64(key: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in key.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_0103);
    }
    h
}

/// Child count where the lazy hash index starts beating the linear fingerprint scan.
pub const LINEAR_SCAN_MAX: usize = 16;

/// A parsed document: shared raw bytes + one flat node arena + lazily built wide-object indexes.
pub struct TapeDoc {
    raw: Arc<[u8]>,
    /// Fused arena: node + key span + container literal end in one contiguous record.
    recs: Vec<Rec>,
    /// Per-object key indexes for wide objects, built on first lookup and cached here.
    wide_index: Mutex<Option<HashMap<u32, HashMap<u64, u32>>>>,
    /// Per-array child offset tables (node index -> [child0, child1, ...]), built on the FIRST
    /// indexed access of an array so repeated `arr[j]` reads become O(1) instead of O(j) walks.
    array_offsets: Mutex<Option<HashMap<u32, Arc<Vec<u32>>>>>,
    token: usize,
}

impl TapeDoc {
    /// Build the tape arena from strict JSON text (invalid input -> `Err`, never a panic).
    pub fn build(input: &[u8]) -> Result<Self, ParseErr> {
        let raw: Arc<[u8]> = Arc::from(input); // single allocation + copy
        let mut sink = TapeSink {
            recs: Vec::with_capacity(input.len() / 14),
            stack: Vec::new(),
        };
        crate::parse::run_parse(input, &mut sink)?;
        Ok(Self {
            raw,
            recs: sink.recs,
            wide_index: Mutex::new(None),
            array_offsets: Mutex::new(None),
            token: doc_token().fetch_add(1, Ordering::Relaxed),
        })
    }

    #[inline]
    pub fn raw(&self) -> &Arc<[u8]> {
        &self.raw
    }

    #[inline]
    pub fn node_count(&self) -> usize {
        self.recs.len()
    }

    #[inline]
    pub fn node(&self, idx: u32) -> &TapeNode {
        &self.recs[idx as usize].node
    }

    /// Raw source bytes of a node: container brackets `[start..end]`, string body, or number token.
    #[inline]
    pub fn slice(&self, idx: u32) -> &[u8] {
        let n = self.node(idx);
        &self.raw[n.start as usize..n.end as usize]
    }

    /// Full literal span of any node: containers use the side table (their `end` holds the child
    /// count instead of a byte offset), scalars reuse `slice()`.
    #[inline]
    pub fn raw_literal(&self, idx: u32) -> &[u8] {
        let n = self.node(idx);
        if n.is_container() {
            &self.raw[n.start as usize..self.recs[idx as usize].lit_end as usize]
        } else {
            self.slice(idx)
        }
    }

    /// Index just past this node's whole subtree (next sibling / end of arena for the root).
    #[inline]
    pub fn subtree_end(&self, idx: u32) -> u32 {
        self.node(idx).skip_to
    }

    /// Iterator over direct children of an Object/Array node.
    #[inline]
    pub fn children(&self, idx: u32) -> ChildIter<'_> {
        let n = self.node(idx);
        ChildIter {
            doc: self,
            cur: idx + 1,
            stop: n.skip_to,
            left: n.child_count(),
        }
    }

    /// Member key bytes for a child node (`None` when the node has no owning key).
    pub fn key_bytes(&self, child: u32) -> Option<Result<Cow<'_, str>, ParseErr>> {
        let span = self.recs[child as usize].key;
        if span == NO_KEY {
            return None;
        }
        let bytes = &self.raw[span.0 as usize..span.1 as usize];
        if (self.node(child).flags & FLAG_KEY_ESCAPE) != 0 {
            Some(decode_string_content(bytes).map(Cow::Owned))
        } else if crate::simd::is_ascii_fast(bytes) {
            // SAFETY: pure-ASCII bytes are valid UTF-8 by construction.
            Some(Ok(Cow::Borrowed(unsafe { std::str::from_utf8_unchecked(bytes) })))
        } else {
            Some(
                std::str::from_utf8(bytes)
                    .map(Cow::Borrowed)
                    .map_err(|_| ParseErr::new(span.0 as usize, "Invalid UTF-8 in key")),
            )
        }
    }

    #[inline]
    fn raw_key_is(&self, child: u32, fp: u16, key: &str) -> bool {
        let n = self.node(child);
        if n.key_fp != fp {
            return false;
        }
        let span = self.recs[child as usize].key;
        if span == NO_KEY {
            return false;
        }
        if (n.flags & FLAG_KEY_ESCAPE) != 0 {
            // Escaped keys decode then compare exactly.
            return match decode_string_content(&self.raw[span.0 as usize..span.1 as usize]) {
                Ok(s) => s == key,
                Err(_) => false,
            };
        }
        let kb = &self.raw[span.0 as usize..span.1 as usize];
        kb.len() == key.len() && kb == key.as_bytes()
    }

    /// Object member lookup with adaptive indexing.
    pub fn lookup(&self, obj: u32, key: &str) -> Option<u32> {
        let n = self.node(obj);
        debug_assert_eq!(n.kind, KIND_OBJECT);
        let count = n.child_count();
        if count == 0 {
            return None;
        }
        let fp = key_fingerprint(key);
        if count <= LINEAR_SCAN_MAX {
            // Linear scan keeps going so DUPLICATE keys resolve to the LAST occurrence (QuickJS
            // overwrite semantics), instead of short-circuiting on the first fingerprint hit.
            let mut found: Option<u32> = None;
            for c in self.children(obj) {
                if self.raw_key_is(c, fp, key) {
                    found = Some(c);
                }
            }
            return found;
        }
        let mut guard = self.wide_index.lock().expect("wide index lock");
        if guard.is_none() {
            *guard = Some(HashMap::new());
        }
        if let Some(inner) = guard.as_ref().and_then(|m| m.get(&obj)) {
            // Hash hits still require exact key verification (hash collisions are possible).
            return inner
                .get(&hash64(key))
                .copied()
                .filter(|c| self.raw_key_is(*c, fp, key));
        }
        let mut built = HashMap::with_capacity(count);
        for c in self.children(obj) {
            if let Some(k) = self.key_bytes(c) {
                if let Ok(t) = k {
                    built.insert(hash64(t.as_ref()), c);
                }
            }
        }
        let res = built
            .get(&hash64(key))
            .copied()
            .filter(|c| self.raw_key_is(*c, fp, key));
        guard.as_mut().expect("index map").insert(obj, built);
        res
    }

    /// Array element node by index.
    pub fn child_at(&self, arr: u32, index: usize) -> Option<u32> {
        let n = self.node(arr);
        debug_assert_eq!(n.kind, KIND_ARRAY);
        if index >= n.child_count() {
            return None;
        }
        // H25 one-slot reader memo (same idiom as doc(id)/peek_store): repeat indexed reads of
        // the SAME array hit a lock-free thread-local before touching the mutex/hash.
        ARRAY_MEMO.with(|m| {
            if let Some((tok, id, tab)) = &*m.borrow() {
                if *tok == self.token && *id == arr {
                    return tab.get(index).copied();
                }
            }
            None
        })
            .or_else(|| {
                {
                    let guard = self.array_offsets.lock().expect("array offsets lock");
                    if let Some(map) = guard.as_ref() {
                        if let Some(tab) = map.get(&arr) {
                            ARRAY_MEMO.with(|m| *m.borrow_mut() = Some((self.token, arr, tab.clone())));
                            return tab.get(index).copied();
                        }
                    }
                }
                let mut cur = arr + 1;
                for _ in 0..index {
                    cur = self.node(cur).skip_to;
                }
                // Build the full table for this array now that it is being indexed repeatedly.
                {
                    let mut guard = self.array_offsets.lock().expect("array offsets lock");
                    let map = guard.get_or_insert_with(HashMap::new);
                    if !map.contains_key(&arr) {
                        let count = n.child_count();
                        let mut tab = Vec::with_capacity(count);
                        let mut c = arr + 1;
                        for _ in 0..count {
                            tab.push(c);
                            c = self.node(c).skip_to;
                        }
                        map.insert(arr, Arc::new(tab));
                    }
                    if let Some(tab) = map.get(&arr) {
                        ARRAY_MEMO.with(|m| *m.borrow_mut() = Some((self.token, arr, tab.clone())));
                    }
                }
                Some(cur)
            })
    }

    /// Read a scalar node (clean string bodies borrow the raw document — zero copy).
    pub fn scalar(&self, idx: u32) -> Result<Scalar<'_>, ParseErr> {
        let n = self.node(idx);
        match n.kind {
            KIND_NULL => Ok(Scalar::Null),
            KIND_BOOL => Ok(Scalar::Bool(n.is_true())),
            KIND_INT => {
                let tok = self.token(idx)?;
                match parse_int_fast(tok.as_bytes()) {
                    Some(v) => Ok(Scalar::Int(v)),
                    // i64 overflow lands here exactly like QuickJS's float fallback.
                    None => Ok(Scalar::Float(tok.parse::<f64>().unwrap_or(f64::INFINITY))),
                }
            }
            KIND_FLOAT => {
                let tok = self.token(idx)?;
                tok.parse::<f64>()
                    .map(Scalar::Float)
                    .map_err(|_| ParseErr::new(n.start as usize, "Invalid number token"))
            }
            KIND_STRING => {
                let bytes = self.slice(idx);
                if (n.flags & FLAG_HAS_ESCAPE) != 0 {
                    Ok(Scalar::Escaped(decode_string_content(bytes)?))
                } else {
                    let s = if crate::simd::is_ascii_fast(bytes) {
                        // SAFETY: pure-ASCII bytes are valid UTF-8 by construction.
                        unsafe { std::str::from_utf8_unchecked(bytes) }
                    } else {
                        std::str::from_utf8(bytes)
                            .map_err(|_| ParseErr::new(n.start as usize, "Invalid UTF-8 in string"))?
                    };
                    Ok(Scalar::Clean(s))
                }
            }
            _ => Err(ParseErr::new(n.start as usize, "Not a scalar node")),
        }
    }

    fn token(&self, idx: u32) -> Result<&str, ParseErr> {
        let b = self.slice(idx);
        if crate::simd::is_ascii_fast(b) {
            // SAFETY: number tokens are ASCII by validated grammar.
            return Ok(unsafe { std::str::from_utf8_unchecked(b) });
        }
        std::str::from_utf8(b).map_err(|_| ParseErr::new(self.node(idx).start as usize, "Invalid token bytes"))
    }

    /// Enumerate an object node's member keys in document order. Duplicate keys appear ONCE at
    /// their first position (a JS object collapses them while keeping the last value).
    pub fn object_keys(&self, obj: u32) -> Vec<String> {
        let mut out = Vec::with_capacity(self.node(obj).child_count());
        for c in self.children(obj) {
            if let Some(Ok(k)) = self.key_bytes(c) {
                let s = k.into_owned();
                if !out.iter().any(|e| e == &s) {
                    out.push(s);
                }
            }
        }
        out
    }

    /// Member key text for a child node as an owned String (`None` when absent/undecodable).
    pub fn tape_key(&self, child: u32) -> Option<String> {
        self.key_bytes(child)?.ok().map(|k| k.into_owned())
    }
}

/// Iterator over direct children (advances through each child's `skip_to` subtree end).
pub struct ChildIter<'d> {
    doc: &'d TapeDoc,
    cur: u32,
    stop: u32,
    left: usize,
}

impl<'d> Iterator for ChildIter<'d> {
    type Item = u32;
    fn next(&mut self) -> Option<u32> {
        if self.left == 0 || self.cur >= self.stop {
            return None;
        }
        let idx = self.cur;
        self.left -= 1;
        self.cur = self.doc.node(idx).skip_to;
        Some(idx)
    }
}

/// Scalar view of a Tape node.
#[derive(Clone, Debug, PartialEq)]
pub enum Scalar<'a> {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// Unescaped string body borrowed straight from the raw document.
    Clean(&'a str),
    /// Escaped string body materialized on demand.
    Escaped(String),
}

impl<'a> Scalar<'a> {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Scalar::Clean(s) => Some(s),
            Scalar::Escaped(s) => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Scalar::Int(v) => Some(*v as f64),
            Scalar::Float(v) => Some(*v),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Scalar::Null)
    }
}

// ---------------------------------------------------------------------------
// Tape sink: pre-order arena construction with placeholder patching
// ---------------------------------------------------------------------------

struct Frame {
    idx: u32,
    /// Member key meta of the PARENT frame awaiting the value node just pushed.
    pending: Option<(u16, Span, bool)>,
}

/// Sink that appends nodes in pre-order while the shared strict parser validates the document.
/// One fused arena record: the 16-byte TapeNode plus its member-key span and the container
/// literal end in a single contiguous slot (one push, one cache-line touch per node).
#[repr(C)]
#[derive(Clone, Copy)]
struct Rec {
    node: TapeNode,
    key: Span,
    lit_end: u32,
}

/// Sink that appends nodes in pre-order while the shared strict parser validates the document.
pub struct TapeSink {
    recs: Vec<Rec>,
    stack: Vec<Frame>,
}

impl TapeSink {
    fn push(&mut self, kind: u8, flags: u8, span: Span) -> u32 {
        let idx = self.recs.len() as u32;
        self.recs.push(Rec {
            node: TapeNode {
            kind,
            flags,
            key_fp: 0,
            start: span.0,
            // Containers patch `end` to their child count later; scalars keep byte end offsets.
            end: if kind == KIND_OBJECT || kind == KIND_ARRAY { 0 } else { span.1 },
                skip_to: idx + 1,
            },
            key: NO_KEY,
            lit_end: span.1,
        });
        idx
    }

    /// Consume the parent frame's pending member key onto the node just pushed.
    fn apply_pending_key(&mut self, idx: u32) {
        if let Some(f) = self.stack.last_mut() {
            if let Some((fp, kspan, escaped)) = f.pending.take() {
                self.recs[idx as usize].node.key_fp = fp;
                self.recs[idx as usize].key = kspan;
                if escaped {
                    self.recs[idx as usize].node.flags |= FLAG_KEY_ESCAPE;
                }
            }
        }
    }

    fn close_container(&mut self, idx: u32, count: usize, span: Span) {
        let f = self.stack.pop().expect("balanced container frame");
        debug_assert_eq!(f.idx, idx);
        let next = self.recs.len() as u32;
        {
            let n = &mut self.recs[idx as usize].node;
            n.start = span.0;
            n.end = count as u32;
            // skip_to = index just past this subtree == next sibling (arena end for the root).
            n.skip_to = next;
            self.recs[idx as usize].lit_end = span.1;
        }
        self.apply_pending_key(idx);
    }
}

impl<'a> Sink<'a> for TapeSink {
    fn null(&mut self) {
        let idx = self.push(KIND_NULL, 0, (0, 0));
        self.apply_pending_key(idx);
    }

    fn boolean(&mut self, is_true: bool) {
        let idx = self.push(KIND_BOOL, if is_true { FLAG_BOOL_TRUE } else { 0 }, (0, 0));
        self.apply_pending_key(idx);
    }

    fn integer(&mut self, _value: i64, span: Span) {
        let idx = self.push(KIND_INT, 0, span);
        self.apply_pending_key(idx);
    }

    fn float(&mut self, _value: f64, span: Span) {
        let idx = self.push(KIND_FLOAT, 0, span);
        self.apply_pending_key(idx);
    }

    fn string(&mut self, _text: Cow<'a, str>, span: Span, escaped: bool) {
        let idx = self.push(KIND_STRING, if escaped { FLAG_HAS_ESCAPE } else { 0 }, span);
        self.apply_pending_key(idx);
    }

    fn array_begin(&mut self) {
        let idx = self.push(KIND_ARRAY, 0, (0, 0));
        // The placeholder is pushed BEFORE the new frame so a nested container member still gets
        // its parent's pending key metadata.
        self.apply_pending_key(idx);
        self.stack.push(Frame { idx, pending: None });
    }

    fn array_end(&mut self, count: usize, span: Span) {
        let idx = self.stack.last().expect("array frame").idx;
        self.close_container(idx, count, span);
    }

    fn object_begin(&mut self) {
        let idx = self.push(KIND_OBJECT, 0, (0, 0));
        self.apply_pending_key(idx);
        self.stack.push(Frame { idx, pending: None });
    }

    fn object_key(&mut self, key: Key<'a>) {
        if let Some(f) = self.stack.last_mut() {
            f.pending = Some((key.fp, key.span, key.escaped));
        }
    }

    fn object_end(&mut self, count: usize, span: Span) {
        let idx = self.stack.last().expect("object frame").idx;
        self.close_container(idx, count, span);
    }
}

#[cfg(test)]
mod tape_tests {
    use super::*;
    use crate::val::Val;

    #[test]
    fn tapenode_is_exactly_16_bytes_and_pod() {
        assert_eq!(std::mem::size_of::<TapeNode>(), 16);
        assert_eq!(std::mem::align_of::<TapeNode>(), 4);
        let n = TapeNode { kind: KIND_STRING, flags: FLAG_HAS_ESCAPE, key_fp: 7, start: 3, end: 9, skip_to: 10 };
        let c = n;
        assert_eq!(n, c);
        // C layout field order: kind, flags, key_fp, start, end, skip_to.
        let bytes = unsafe {
            std::slice::from_raw_parts((&n as *const TapeNode) as *const u8, 16)
        };
        assert_eq!(bytes[0], KIND_STRING);
        assert_eq!(bytes[1], FLAG_HAS_ESCAPE);
    }

    #[test]
    fn arena_is_preorder_with_o1_subtree_skip_links() {
        let src = r#"{"a":{"b":[1,2],"c":3},"d":4}"#;
        let doc = TapeDoc::build(src.as_bytes()).expect("build tape");
        let root = doc.node(0);
        assert_eq!(root.kind, KIND_OBJECT);
        assert_eq!(root.child_count(), 2);
        assert_eq!(doc.node_count(), 7); // obj,a-obj,b-arr,1,2,c,d
        let a = doc.lookup(0, "a").expect("a");
        assert_eq!(doc.node(a).kind, KIND_OBJECT);
        assert_eq!(doc.node(a).child_count(), 2);
        assert_eq!(doc.children(a).collect::<Vec<_>>(), vec![a + 1, a + 4]);
        // Skipping subtree `a` lands exactly on sibling `d`.
        assert_eq!(doc.subtree_end(a), doc.lookup(0, "d").expect("d"));
        assert_eq!(doc.subtree_end(0), doc.node_count() as u32);
    }

    #[test]
    fn narrow_object_uses_linear_fingerprint_scan_including_escaped_keys() {
        let src = r#"{"alpha":1,"\u00e9x":2,"dup":10,"dup":20,"missing":null}"#;
        let doc = TapeDoc::build(src.as_bytes()).expect("build");
        let root = 0;
        assert!(doc.node(root).child_count() <= LINEAR_SCAN_MAX);
        assert!(doc.lookup(root, "alpha").is_some());
        assert_eq!(doc.scalar(doc.lookup(root, "alpha").unwrap()).unwrap().as_f64(), Some(1.0));
        // Escaped key: fingerprint is computed on the DECODED text, verified after decoding.
        let ex = doc.lookup(root, "éx").expect("escaped key lookup");
        assert_eq!(doc.tape_key(ex).as_deref(), Some("éx"));
        // Duplicate keys resolve to the LAST occurrence like QuickJS.
        let dup = doc.lookup(root, "dup").expect("dup");
        assert_eq!(doc.scalar(dup).unwrap().as_f64(), Some(20.0));
        assert!(doc.lookup(root, "nope").is_none());
        // Fingerprints differ for same-length keys with different last bytes (no false hits).
        assert_ne!(key_fingerprint("ab"), key_fingerprint("ac"));
        assert_ne!(key_fingerprint(""), key_fingerprint("ab"));
    }

    #[test]
    fn wide_objects_build_and_cache_a_hash_index_on_first_lookup() {
        let mut src = String::from("{");
        for i in 0..64 {
            if i > 0 {
                src.push(',');
            }
            src.push_str(&format!("\"field_{i}\":{}", i * 10));
        }
        src.push('}');
        let doc = TapeDoc::build(src.as_bytes()).expect("wide build");
        assert!(doc.node(0).child_count() > LINEAR_SCAN_MAX);
        for i in 0..64usize {
            let key = format!("field_{i}");
            let c = doc.lookup(0, &key).expect("wide lookup");
            assert_eq!(doc.scalar(c).unwrap().as_f64(), Some((i * 10) as f64));
        }
        // Second access hits the cached index and still rejects absent keys.
        assert!(doc.lookup(0, "field_33").is_some());
        assert!(doc.lookup(0, "field_64").is_none());
        assert_eq!(doc.object_keys(0).len(), 64);
    }

    #[test]
    fn tape_scalars_match_the_dom_parser_on_the_same_document() {
        let src = r#"{"i":-7,"f":2.5,"s":"clean","e":"esc\n\u00e9\uD83D\uDE00","t":true,"fa":false,"n":null}"#;
        let dom = crate::parse::parse_dom(src.as_bytes()).expect("dom");
        let doc = TapeDoc::build(src.as_bytes()).expect("tape");
        for key in ["i", "f", "s", "e", "t", "fa", "n"] {
            let child = doc.lookup(0, key).expect("child");
            let s = doc.scalar(child).expect("scalar");
            let d = dom.get(key).expect("dom value");
            match (&s, d) {
                (Scalar::Int(a), Val::Int(b)) => assert_eq!(a, b),
                (Scalar::Float(a), Val::Float(b)) => assert_eq!(a, b),
                (Scalar::Clean(a), Val::Str(b)) => assert_eq!(*a, b.as_ref()),
                (Scalar::Escaped(a), Val::Str(b)) => assert_eq!(a.as_str(), b.as_ref()),
                (Scalar::Bool(a), Val::Bool(b)) => assert_eq!(a, b),
                (Scalar::Null, Val::Null) => {}
                _ => panic!("kind mismatch for {key}: {s:?} vs {d:?}"),
            }
        }
    }

    #[test]
    fn container_and_string_spans_are_byte_exact_for_passthrough() {
        let src = r#"{"arr":[1,2,{"deep":true}],"txt":"raw body","esc":"a\nb"}"#;
        let doc = TapeDoc::build(src.as_bytes()).expect("spans");
        let arr = doc.lookup(0, "arr").unwrap();
        assert_eq!(std::str::from_utf8(doc.raw_literal(arr)).unwrap(), "[1,2,{\"deep\":true}]");
        // Scalars keep byte end offsets inside TapeNode.end, containers use the side table.
        assert_eq!(doc.node(arr).child_count(), 3);
        let txt = doc.lookup(0, "txt").unwrap();
        assert_eq!(std::str::from_utf8(doc.slice(txt)).unwrap(), "raw body");
        assert_eq!(doc.node(txt).flags & FLAG_HAS_ESCAPE, 0);
        let esc = doc.lookup(0, "esc").unwrap();
        assert_ne!(doc.node(esc).flags & FLAG_HAS_ESCAPE, 0);
        assert_eq!(doc.scalar(esc).unwrap().as_str(), Some("a\nb"));
    }

    #[test]
    fn array_child_indexing_steps_over_nested_subtrees() {
        let src = r#"[[1,2],[3],"x",[]]"#;
        let doc = TapeDoc::build(src.as_bytes()).expect("array");
        assert_eq!(doc.node(0).child_count(), 4);
        for i in 0..4usize {
            let c = doc.child_at(0, i).expect("element");
            assert!(c < doc.subtree_end(0));
        }
        assert!(doc.child_at(0, 4).is_none());
        let first = doc.child_at(0, 0).unwrap();
        assert_eq!(doc.node(first).kind, KIND_ARRAY);
        assert_eq!(doc.scalar(doc.child_at(first, 1).unwrap()).unwrap().as_f64(), Some(2.0));
    }
}

