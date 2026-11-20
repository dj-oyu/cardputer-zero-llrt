//! Layer 2 support — the transparent Lazy runtime: an immutable `Arc<[u8]>` document plus the
//! 16-byte Tape arena, with Copy-on-Write edits kept in a sparse `BTreeMap` keyed by the owning
//! container node index. Nothing materializes until touched, mutations never rewrite the tape, and
//! serialization slices raw bytes for every untouched subtree.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::parse::ParseErr;
use crate::tape::{KIND_ARRAY, KIND_OBJECT, Scalar, TapeDoc};
use crate::val::Val;

/// The size cutoff where `JSON.parse` switches from the fast DOM path to Lazy handles.
pub const LAZY_THRESHOLD_BYTES: usize = 4096;

/// A Copy-on-Write member edit: `Some(val)` = set/add, `None` = delete (tombstone).
type Edit = Vec<(String, Option<Val<'static>>)>;

/// A live Lazy document: shared raw bytes + tape arena + sparse CoW edits.
pub struct LazyDoc {
    pub tape: TapeDoc,
    /// Registry id assigned by [`register`] (`u32::MAX` before registration); child handles need
    /// it so nested Lazy proxies can address the same document.
    pub id: AtomicU32,
    /// Any mutation flips this; clean documents keep byte-exact zero-reparse passthrough.
    dirty: AtomicBool,
    /// node index -> ordered member edits (later entries win).
    edits: Mutex<BTreeMap<u32, Edit>>,
}

impl LazyDoc {
    /// Parse strictly and build the Lazy document (no JS objects touched).
    pub fn build(input: &[u8]) -> Result<Arc<Self>, ParseErr> {
        let tape = TapeDoc::build(input)?;
        Ok(Arc::new(Self {
            tape,
            id: AtomicU32::new(u32::MAX),
            dirty: AtomicBool::new(false),
            edits: Mutex::new(BTreeMap::new()),
        }))
    }

    #[inline]
    pub fn is_clean(&self) -> bool {
        !self.dirty.load(Ordering::SeqCst)
    }

    #[inline]
    pub fn raw_bytes(&self) -> usize {
        self.tape.raw().len()
    }

    #[inline]
    pub fn node_count(&self) -> usize {
        self.tape.node_count()
    }

    /// Exact original bytes for a node (zero-reparse `JSON.stringify` passthrough source).
    #[inline]
    pub fn raw_slice(&self, node: u32) -> &[u8] {
        self.tape.raw_literal(node)
    }

    /// True when no CoW edit touches any node inside this subtree.
    pub fn subtree_is_clean(&self, node: u32) -> bool {
        if self.is_clean() {
            return true;
        }
        let stop = self.tape.subtree_end(node);
        let guard = self.edits.lock().expect("edits lock");
        // BTreeMap range over [node, subtree_end) — tiny and ordered.
        !guard
            .range(node..stop)
            .any(|(_, edits)| !edits.is_empty())
    }

    fn edits_of(&self, node: u32) -> Option<Edit> {
        let guard = self.edits.lock().expect("edits lock");
        guard.get(&node).cloned()
    }

    /// Latest edit for `key` on `node` (later pushes win over earlier ones).
    fn latest(&self, node: u32, key: &str) -> Option<Option<Val<'static>>> {
        let list = self.edits_of(node)?;
        list.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    /// Read a member / element: overrides first, then the untouched tape.
    pub fn get(&self, node: u32, key: &str) -> Got<'_> {
        if let Some(edit) = self.latest(node, key) {
            return match edit {
                Some(v) => Got::Owned(v),
                None => Got::Missing, // deleted field reads as absent
            };
        }
        let n = self.tape.node(node);
        if n.kind == KIND_OBJECT {
            match self.tape.lookup(node, key) {
                Some(child) => self.got_from(child),
                None => Got::Missing,
            }
        } else if n.kind == KIND_ARRAY {
            match parse_index(key) {
                Some(i) => match self.tape.child_at(node, i) {
                    Some(child) => self.got_from(child),
                    None => Got::Missing,
                },
                None => Got::Missing,
            }
        } else {
            Got::Missing
        }
    }

    fn got_from(&self, child: u32) -> Got<'_> {
        let cn = self.tape.node(child);
        if cn.is_container() {
            Got::Subtree(child)
        } else {
            match self.tape.scalar(child) {
                Ok(s) => Got::Scalar(s),
                Err(_) => Got::Missing,
            }
        }
    }

    /// Presence check used by the JS `in` trap.
    pub fn has(&self, node: u32, key: &str) -> bool {
        if let Some(edit) = self.latest(node, key) {
            return edit.is_some();
        }
        let n = self.tape.node(node);
        if n.kind == KIND_OBJECT {
            self.tape.lookup(node, key).is_some()
        } else if n.kind == KIND_ARRAY {
            match parse_index(key) {
                Some(i) => i < n.child_count(),
                None => false,
            }
        } else {
            false
        }
    }

    /// Enumerate own keys in document order (deletions removed, additions appended; array
    /// elements come out in ascending numeric order like a real JS array).
    pub fn own_keys(&self, node: u32) -> Vec<String> {
        let n = self.tape.node(node);
        let mut out = Vec::with_capacity(n.child_count());
        if n.kind == KIND_OBJECT {
            for c in self.tape.children(node) {
                if let Some(Ok(k)) = self.tape.key_bytes(c) {
                    let present = match self.latest(node, k.as_ref()) {
                        Some(None) => false, // deleted
                        _ => true,
                    };
                    if present {
                        out.push(k.into_owned());
                    }
                }
            }
            self.append_added_keys(node, &mut out, false);
        } else if n.kind == KIND_ARRAY {
            let count = n.child_count();
            let mut live: Vec<usize> = (0..count)
                .filter(|i| match self.latest(node, &i.to_string()) {
                    Some(None) => false,
                    _ => true,
                })
                .collect();
            // Additions beyond the tape length join in ascending numeric order.
            if let Some(list) = self.edits_of(node) {
                for (k, v) in list.iter() {
                    if v.is_some() {
                        if let Some(i) = parse_index(k) {
                            if i >= count && !live.contains(&i) {
                                live.push(i);
                            }
                        }
                    }
                }
            }
            live.sort_unstable();
            for i in live {
                out.push(i.to_string());
            }
        }
        out
    }

    fn append_added_keys(&self, node: u32, out: &mut Vec<String>, _object: bool) {
        if let Some(list) = self.edits_of(node) {
            for (k, v) in list.iter() {
                if v.is_some() && !out.iter().any(|e| e == k) && !self.tape.lookup(node, k).is_some() {
                    out.push(k.clone());
                }
                // Deleted-then-readded keys already appear via the tape scan above.
            }
        }
    }

    /// Copy-on-Write member write (never touches the immutable tape).
    pub fn set(&self, node: u32, key: &str, val: Val<'static>) {
        {
            let mut guard = self.edits.lock().expect("edits lock");
            guard.entry(node).or_default().push((key.to_string(), Some(val)));
        }
        self.dirty.store(true, Ordering::SeqCst);
    }

    /// Copy-on-Write delete; returns whether the key existed beforehand.
    pub fn del(&self, node: u32, key: &str) -> bool {
        let existed = self.has(node, key);
        {
            let mut guard = self.edits.lock().expect("edits lock");
            guard.entry(node).or_default().push((key.to_string(), None));
        }
        self.dirty.store(true, Ordering::SeqCst);
        existed
    }

    /// Compact JSON serialization of (possibly mutated) subtree `node`: untouched subtrees are
    /// copied straight from the raw buffer, edited ones merge tape + CoW overrides.
    pub fn serialize(&self, node: u32, out: &mut Vec<u8>) {
        if self.subtree_is_clean(node) {
            // Zero-reparse passthrough: emit the exact original bytes for containers.
            let n = self.tape.node(node);
            if n.is_container() {
                out.extend_from_slice(self.raw_slice(node));
                return;
            }
            // Scalars: wrap string bodies in quotes, copy other tokens verbatim.
            if n.kind == crate::tape::KIND_STRING {
                out.push(b'"');
            }
            out.extend_from_slice(self.raw_slice(node));
            if n.kind == crate::tape::KIND_STRING {
                out.push(b'"');
            }
            return;
        }
        let n = self.tape.node(node);
        match n.kind {
            KIND_OBJECT => {
                out.push(b'{');
                let mut first = true;
                for c in self.tape.children(node) {
                    if let Some(Ok(k)) = self.tape.key_bytes(c) {
                        if matches!(self.latest(node, k.as_ref()), Some(None)) {
                            continue; // deleted member vanishes
                        }
                        if !first {
                            out.push(b',');
                        }
                        first = false;
                        write_key(out, &self.tape, c, k.as_ref());
                        out.push(b':');
                        match self.latest(node, k.as_ref()) {
                            Some(Some(v)) => serialize_val(&v, out),
                            _ => self.serialize(c, out),
                        }
                    }
                }
                // Newly added members append after the original document order.
                if let Some(list) = self.edits_of(node) {
                    let keys = self.own_keys(node);
                    for (k, v) in list.iter() {
                        if v.is_some() && self.tape.lookup(node, k).is_none() {
                            if !first {
                                out.push(b',');
                            }
                            first = false;
                            crate::simd::write_escaped_str(out, k);
                            out.push(b':');
                            serialize_val(v.as_ref().expect("added member has a value"), out);
                        }
                        let _ = &keys;
                    }
                }
                out.push(b'}');
            }
            KIND_ARRAY => {
                out.push(b'[');
                let count = n.child_count();
                let mut edits: Vec<(usize, Option<Val<'static>>)> = Vec::new();
                if let Some(list) = self.edits_of(node) {
                    for (k, v) in list.iter() {
                        if let Some(i) = parse_index(k) {
                            edits.push((i, v.clone()));
                        }
                    }
                }
                // Effective length includes appended indices beyond the tape count.
                let mut len = count;
                for (i, v) in edits.iter() {
                    if v.is_some() && i + 1 > len {
                        len = i + 1;
                    }
                }
                for pos in 0..len {                    if pos > 0 {
                        out.push(b',');
                    }
                    let mut done = false;
                    if pos < count {
                        let child = self.tape.child_at(node, pos).expect("tape element");
                        match self.latest(node, &pos.to_string()) {
                            Some(None) => {
                                out.extend_from_slice(b"null"); // holes serialize as null
                                done = true;
                            }
                            Some(Some(v)) => {
                                serialize_val(&v, out);
                                done = true;
                            }
                            None => {}
                        }
                        if !done {
                            self.serialize(child, out);
                        }
                    } else {
                        // Appended element: latest edit wins.
                        let latest = edits
                            .iter()
                            .rev()
                            .find(|(i, _)| *i == pos)
                            .and_then(|(_, v)| v.clone());
                        match latest {
                            Some(v) => serialize_val(&v, out),
                            None => out.extend_from_slice(b"null"),
                        }
                    }
                }
                out.push(b']');
            }
            _ => {
                // Scalars inside an edited parent come through `get`/overrides, never here.
                let n2 = self.tape.node(node);
                if n2.kind == crate::tape::KIND_STRING {
                    out.push(b'"');
                }
                out.extend_from_slice(self.raw_slice(node));
                if n2.kind == crate::tape::KIND_STRING {
                    out.push(b'"');
                }
            }
        }
    }
}

fn write_key(out: &mut Vec<u8>, tape: &TapeDoc, child: u32, key: &str) {
    out.push(b'"');
    // Unescaped keys can be copied straight from the source bytes; escaped ones re-escape.
    let flags = tape.node(child).flags;
    if (flags & crate::tape::FLAG_KEY_ESCAPE) == 0 {
        if let Some(Ok(k)) = tape.key_bytes(child) {
            // Borrowed original body bytes are already JSON-escaped-clean.
            let raw = k.as_ref().as_bytes();
            out.extend_from_slice(raw);
        } else {
            crate::simd::write_escaped_str(&mut *out, key);
            return;
        }
    } else {
        let mut tmp = Vec::new();
        crate::simd::write_escaped_str(&mut tmp, key);
        // tmp already includes the surrounding quotes; strip them since we pushed one.
        out.extend_from_slice(&tmp[1..tmp.len() - 1]);
    }
    out.push(b'"');
}

/// Serialize an owned `Val` (a CoW-written value tree).
pub fn serialize_val(v: &Val<'static>, out: &mut Vec<u8>) {
    crate::val::write_val(out, v);
}

/// The result of a Lazy read: missing, an immediate scalar, an untouched subtree (child handle)
/// or a Copy-on-Write owned value.
#[derive(Clone, Debug)]
pub enum Got<'a> {
    Missing,
    Scalar(Scalar<'a>),
    Subtree(u32),
    Owned(Val<'static>),
}

/// Parse an array index key ("0", "42"); rejects anything else.
pub fn parse_index(key: &str) -> Option<usize> {
    if key.is_empty() || key.len() > 18 {
        return None;
    }
    // Reject leading zeros except the single "0" (ECMA array-index semantics).
    if key.len() > 1 && key.starts_with('0') {
        return None;
    }
    let b = key.as_bytes();
    if !b.iter().all(|c| c.is_ascii_digit()) {
        return None;
    }
    key.parse::<usize>().ok()
}

// ---------------------------------------------------------------------------
// Process-wide handle registry (ids are indices into this vector)
// ---------------------------------------------------------------------------

fn registry() -> &'static Mutex<Vec<Arc<LazyDoc>>> {
    static REG: OnceLock<Mutex<Vec<Arc<LazyDoc>>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(Vec::new()))
}

/// Register a document and return its handle id (never drops: ids stay valid for JS handles).
pub fn register(doc: Arc<LazyDoc>) -> u32 {
    let mut reg = registry().lock().expect("lazy registry lock");
    let id = (reg.len()) as u32;
    doc.id.store(id, Ordering::SeqCst);
    reg.push(doc);
    id
}

/// Look up a registered document by id.
pub fn doc(id: u32) -> Option<Arc<LazyDoc>> {
    let reg = registry().lock().expect("lazy registry lock");
    reg.get(id as usize).cloned()
}

/// How many Lazy documents are currently registered (diagnostics/tests).
pub fn registered_count() -> usize {
    let reg = registry().lock().expect("lazy registry lock");
    reg.len()
}

#[cfg(test)]
mod lazy_tests {
    use super::*;
    use std::borrow::Cow;

    const SMALL: &str = r#"{"type":"batch","meta":{"seq":77,"src":"cz"},"items":[{"a":1},{"b":2}],"pad":"x"}"#;

    #[test]
    fn clean_documents_serialize_as_byte_exact_zero_reparse_passthroughs() {
        let doc = LazyDoc::build(SMALL.as_bytes()).expect("build");
        assert!(doc.is_clean());
        let mut out = Vec::new();
        doc.serialize(0, &mut out);
        assert_eq!(std::str::from_utf8(&out).unwrap(), SMALL);
        // A nested clean subtree slices the exact original bytes too.
        let meta = doc.tape.lookup(0, "meta").expect("meta");
        let mut sub = Vec::new();
        doc.serialize(meta, &mut sub);
        assert_eq!(std::str::from_utf8(&sub).unwrap(), r#"{"seq":77,"src":"cz"}"#);
        assert!(doc.subtree_is_clean(meta));
    }

    #[test]
    fn copy_on_write_set_reads_back_and_marks_the_document_dirty() {
        let doc = LazyDoc::build(SMALL.as_bytes()).expect("build");
        let meta = doc.tape.lookup(0, "meta").expect("meta");
        doc.set(meta, "seq", Val::Int(999));
        assert!(!doc.is_clean());
        match doc.get(meta, "seq") {
            Got::Owned(Val::Int(v)) => assert_eq!(v, 999),
            other => panic!("expected owned override, got {other:?}"),
        }
        // Untouched siblings still resolve straight from the immutable tape.
        assert!(matches!(doc.get(0, "type"), Got::Scalar(Scalar::Clean("batch"))));
    }

    #[test]
    fn deletions_hide_members_then_additions_append_at_the_end_of_own_keys() {
        let doc = LazyDoc::build(SMALL.as_bytes()).expect("build");
        assert!(doc.del(0, "pad"));
        doc.set(0, "fresh", Val::Bool(true));
        assert_eq!(doc.own_keys(0), vec!["type", "meta", "items", "fresh"]);
        assert!(matches!(doc.get(0, "pad"), Got::Missing));
        assert!(!doc.has(0, "pad"));
        assert!(doc.has(0, "fresh"));
        // Deleting a key that never existed reports false (JS still returns true for delete).
        assert!(!doc.del(0, "ghost"));
    }

    #[test]
    fn mutated_documents_reserialize_with_overrides_and_clean_sibling_slices() {
        let doc = LazyDoc::build(SMALL.as_bytes()).expect("build");
        let meta = doc.tape.lookup(0, "meta").expect("meta");
        doc.set(meta, "seq", Val::Int(999));
        let mut out = Vec::new();
        doc.serialize(0, &mut out);
        let text = std::str::from_utf8(&out).unwrap();
        assert!(text.contains(r#""seq":999"#), "{text}");
        assert!(text.contains(r#""src":"cz"#), "{text}");
        // The untouched items array keeps its original bytes verbatim.
        let items = doc.tape.lookup(0, "items").expect("items");
        assert!(doc.subtree_is_clean(items));
        let mut arr = Vec::new();
        doc.serialize(items, &mut arr);
        assert_eq!(std::str::from_utf8(&arr).unwrap(), r#"[{"a":1},{"b":2}]"#);
    }

    #[test]
    fn deleted_array_slots_serialize_as_null_and_appends_extend_the_length() {
        let src = r#"[1,2,"tail"]"#;
        let doc = LazyDoc::build(src.as_bytes()).expect("array build");
        doc.del(0, "1");
        doc.set(0, "5", Val::Str(Cow::Owned("appended".into())));
        let mut out = Vec::new();
        doc.serialize(0, &mut out);
        assert_eq!(std::str::from_utf8(&out).unwrap(), r#"[1,null,"tail",null,null,"appended"]"#);
        // own_keys for arrays enumerates surviving indices ascending.
        assert_eq!(doc.own_keys(0), vec!["0", "2", "5"]);
        assert!(doc.has(0, "4") == false);
        assert!(matches!(doc.get(0, "length"), Got::Missing));
    }

    #[test]
    fn subtree_cleanliness_tracks_only_paths_touching_edits() {
        let doc = LazyDoc::build(SMALL.as_bytes()).expect("build");
        let items = doc.tape.lookup(0, "items").expect("items");
        let item0 = doc.tape.child_at(items, 0).expect("item0");
        doc.set(item0, "a", Val::Int(50));
        assert!(!doc.subtree_is_clean(item0));
        assert!(!doc.subtree_is_clean(items));
        assert!(!doc.subtree_is_clean(0));
        let meta = doc.tape.lookup(0, "meta").expect("meta");
        assert!(doc.subtree_is_clean(meta), "unrelated sibling must stay clean");
    }


    #[test]
    fn container_reads_yield_subtree_handles_scalars_stay_immediate() {
        let doc = LazyDoc::build(SMALL.as_bytes()).expect("build");
        match doc.get(0, "meta") {
            Got::Subtree(idx) => assert_eq!(idx, doc.tape.lookup(0, "meta").unwrap()),
            other => panic!("container must return a subtree handle, got {other:?}"),
        }
        match doc.get(0, "type") {
            Got::Scalar(Scalar::Clean(s)) => assert_eq!(s, "batch"),
            other => panic!("scalar must be immediate, got {other:?}"),
        }
        assert!(matches!(doc.get(0, "nope"), Got::Missing));
    }

    #[test]
    fn registry_roundtrips_ids_and_keeps_documents_shared() {
        let before = registered_count();
        let d1 = LazyDoc::build(SMALL.as_bytes()).expect("registry build");
        let id = register(d1.clone());
        let got = crate::lazy::doc(id).expect("registered doc lookup");
        assert!(Arc::ptr_eq(&d1, &got));
        assert_eq!(registered_count(), before + 1);
        assert!(crate::lazy::doc(id + 9_999).is_none());
        assert!(got.raw_bytes() == SMALL.len());
        assert!(got.node_count() > 0);
    }
}
