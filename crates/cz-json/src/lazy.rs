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

    /// Run `f` over this node's edit list UNDER the lock (`None` when no edit touches `node`).
    #[inline]
    fn with_edits<R>(&self, node: u32, f: impl FnOnce(&Edit) -> R) -> Option<R> {
        let guard = self.edits.lock().expect("edits lock");
        guard.get(&node).map(f)
    }

    /// Latest override for `key` on `node`: clones ONLY the one needed payload while the guard is
    /// held (never a whole-`Vec` deep copy). `None` = untouched, `Some(None)` = tombstone.
    #[inline]
    fn latest(&self, node: u32, key: &str) -> Option<Option<Val<'static>>> {
        self.with_edits(node, |edit| latest_in_edit(edit, key).map(|v| v.clone())).flatten()
    }

    /// Presence-only probe that clones NOTHING: `Some(true)` = live CoW write, `Some(false)` =
    /// tombstone delete, `None` = no edit for this key.
    #[inline]
    fn edit_flag(&self, node: u32, key: &str) -> Option<bool> {
        self.with_edits(node, |edit| latest_in_edit(edit, key).map(|v| v.is_some())).flatten()
    }

    /// Last-wins member view for a dirty OBJECT node built with ONE lock acquisition: superseded
    /// writes are replaced in place (their payloads drop right away), so only surviving values are
    /// copied — exactly what serialization emits, never unrelated edits.
    fn member_view(&self, node: u32) -> Option<Vec<(String, Option<Val<'static>>)>> {
        self.with_edits(node, |edit| {
            let mut view: Vec<(String, Option<Val<'static>>)> = Vec::new();
            for (k, v) in edit.iter() {
                match view.iter_mut().find(|(ek, _)| ek == k) {
                    Some(slot) => slot.1 = v.clone(),
                    None => view.push((k.clone(), v.clone())),
                }
            }
            view
        })
    }

    /// Index-keyed last-wins view for a dirty ARRAY node: numeric compares replace the old
    /// `i.to_string()` lookups inside the tape-element loop.
    fn index_view(&self, node: u32) -> Option<Vec<(usize, Option<Val<'static>>)>> {
        self.with_edits(node, |edit| {
            let mut view: Vec<(usize, Option<Val<'static>>)> = Vec::new();
            for (k, v) in edit.iter() {
                if let Some(i) = parse_index(k) {
                    match view.iter_mut().find(|(ei, _)| *ei == i) {
                        Some(slot) => slot.1 = v.clone(),
                        None => view.push((i, v.clone())),
                    }
                }
            }
            view
        })
    }

    /// Read a member / element: overrides first, then the untouched tape.
    pub fn get(&self, node: u32, key: &str) -> Got<'_> {
        // Clean fast-path: an untouched document holds ZERO edits, so skip the `edits` mutex and
        // every edit lookup entirely and resolve straight from the immutable tape.
        if !self.is_clean() {
            if let Some(edit) = self.latest(node, key) {
                return match edit {
                    Some(v) => Got::Owned(v),
                    None => Got::Missing, // deleted field reads as absent
                };
            }
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

    /// Presence check used by the JS `in` trap (zero-lock clean fast path, zero payload clone).
    pub fn has(&self, node: u32, key: &str) -> bool {
        if !self.is_clean() {
            if let Some(live) = self.edit_flag(node, key) {
                return live;
            }
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
        // Clean documents never lock the mutex; a dirty node locks exactly ONCE for its view.
        let view = if self.is_clean() { None } else { self.member_view(node) };
        let arr_view = if self.is_clean() { None } else { self.index_view(node) };
        let mut out = Vec::with_capacity(n.child_count());
        if n.kind == KIND_OBJECT {
            for c in self.tape.children(node) {
                if let Some(Ok(k)) = self.tape.key_bytes(c) {
                    // Tombstone (Some(None)) hides a member; untouched/overridden both stay.
                    if !matches!(view_lookup(view.as_deref(), k.as_ref()), Some(None)) {
                        out.push(k.into_owned());
                    }
                }
            }
            if let Some(list) = &view {
                // Additions append after document order; the last-wins view is already deduped,
                // and deleted-then-readded keys came through the tape scan above.
                for (k, v) in list.iter() {
                    if v.is_some() && self.tape.lookup(node, k).is_none() {
                        out.push(k.clone());
                    }
                }
            }
        } else if n.kind == KIND_ARRAY {
            let count = n.child_count();
            let mut live: Vec<usize> = Vec::with_capacity(count);
            match &arr_view {
                // Untouched array: every index survives without any edit probing.
                None => live.extend(0..count),
                Some(list) => {
                    let removed = tombstone_indices(list);
                    for i in 0..count {
                        if !removed.contains(&i) {
                            live.push(i);
                        }
                    }
                    // Additions beyond the tape length join in ascending numeric order.
                    for (i, v) in list.iter() {
                        if v.is_some() && *i >= count && !live.contains(i) {
                            live.push(*i);
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
            // Zero-reparse passthrough: containers emit their exact original byte span.
            let n = self.tape.node(node);
            if n.is_container() {
                out.extend_from_slice(self.raw_slice(node));
                return;
            }
            // Scalars go through the shared token writer: null/bool literals carry NO source
            // span in the tape, so a blind raw copy used to emit nothing for them.
            write_scalar_token(&self.tape, out, node);
            return;
        }
        let n = self.tape.node(node);
        // ONE lock acquisition for this dirty node: the last-wins view is built while the guard is
        // held, then released BEFORE any recursion so child `subtree_is_clean` never re-locks the
        // same mutex (std::sync::Mutex is not reentrant).
        let view = self.member_view(node);
        match n.kind {
            KIND_OBJECT => {
                out.push(b'{');
                let mut first = true;
                for c in self.tape.children(node) {
                    if let Some(Ok(k)) = self.tape.key_bytes(c) {
                        // SINGLE view lookup per member instead of two `latest()` calls:
                        // tombstone hides it, an override emits the CoW payload, else recurse.
                        match view_lookup(view.as_deref(), k.as_ref()) {
                            Some(None) => continue, // deleted member vanishes
                            Some(Some(val)) => {
                                if !first {
                                    out.push(b',');
                                }
                                first = false;
                                write_key(out, &self.tape, c, k.as_ref());
                                out.push(b':');
                                serialize_val(val, out);
                            }
                            None => {
                                if !first {
                                    out.push(b',');
                                }
                                first = false;
                                write_key(out, &self.tape, c, k.as_ref());
                                out.push(b':');
                                self.serialize(c, out);
                            }
                        }
                    }
                }
                // Newly added members append after the original document order (last write wins,
                // so a re-set key is emitted exactly once).
                if let Some(list) = &view {
                    for (k, v) in list.iter() {
                        if let Some(val) = v {
                            if self.tape.lookup(node, k).is_none() {
                                if !first {
                                    out.push(b',');
                                }
                                first = false;
                                crate::simd::write_escaped_str(out, k);
                                out.push(b':');
                                serialize_val(val, out);
                            }
                        }
                    }
                }
                out.push(b'}');
            }
            KIND_ARRAY => {
                out.push(b'[');
                let count = n.child_count();
                let edits = self.index_view(node).unwrap_or_default();
                // Effective length includes appended indices beyond the tape count.
                let mut len = count;
                for (i, v) in edits.iter() {
                    if v.is_some() && i + 1 > len {
                        len = i + 1;
                    }
                }
                for pos in 0..len {
                    if pos > 0 {
                        out.push(b',');
                    }
                    let mut done = false;
                    if pos < count {
                        let child = self.tape.child_at(node, pos).expect("tape element");
                        // Numeric index compare: no `pos.to_string()` allocation inside the loop.
                        match idx_lookup(&edits, pos) {
                            Some(None) => {
                                out.extend_from_slice(b"null"); // holes serialize as null
                                done = true;
                            }
                            Some(Some(v)) => {
                                serialize_val(v, out);
                                done = true;
                            }
                            None => {}
                        }
                        if !done {
                            self.serialize(child, out);
                        }
                    } else {
                        // Appended element: latest edit wins.
                        match idx_lookup(&edits, pos) {
                            Some(Some(v)) => serialize_val(v, out),
                            _ => out.extend_from_slice(b"null"),
                        }
                    }
                }
                out.push(b']');
            }
            _ => {
                // Scalars inside an edited parent come through `get`/overrides, never here.
                write_scalar_token(&self.tape, out, node);
            }
        }
    }
}

/// Emit one scalar node's JSON text. String bodies copy their original (already escaped) source
/// span between quotes; number tokens copy verbatim; null / true / false have zero-length tape
/// spans, so they render straight from the tagged scalar value.
fn write_scalar_token(tape: &TapeDoc, out: &mut Vec<u8>, node: u32) {
    let n = tape.node(node);
    if n.kind == crate::tape::KIND_STRING {
        out.push(b'"');
        out.extend_from_slice(tape.raw_literal(node));
        out.push(b'"');
        return;
    }
    let raw = tape.raw_literal(node);
    if !raw.is_empty() {
        out.extend_from_slice(raw);
        return;
    }
    match tape.scalar(node) {
        Ok(Scalar::Null) => out.extend_from_slice(b"null"),
        Ok(Scalar::Bool(true)) => out.extend_from_slice(b"true"),
        Ok(Scalar::Bool(false)) => out.extend_from_slice(b"false"),
        Ok(Scalar::Int(i)) => crate::simd::write_i64(out, i),
        Ok(Scalar::Float(f)) => crate::simd::write_f64_json(out, f),
        Ok(Scalar::Clean(s)) => crate::simd::write_escaped_str(out, s),
        Ok(Scalar::Escaped(s)) => crate::simd::write_escaped_str(out, s.as_ref()),
        Err(_) => out.extend_from_slice(b"null"),
    }
}

fn write_key(out: &mut Vec<u8>, tape: &TapeDoc, child: u32, key: &str) {
    // Unescaped keys copy straight from the source bytes (the hot path allocates nothing); escaped
    // keys go through `write_escaped_str`, which emits its own surrounding quotes — no temporary
    // `Vec<u8>` re-escaping buffer like before.
    let flags = tape.node(child).flags;
    if (flags & crate::tape::FLAG_KEY_ESCAPE) == 0 {
        if let Some(Ok(k)) = tape.key_bytes(child) {
            out.push(b'"');
            // Borrowed original body bytes are already JSON-escaped-clean.
            out.extend_from_slice(k.as_ref().as_bytes());
            out.push(b'"');
            return;
        }
    }
    crate::simd::write_escaped_str(out, key);
}

/// Latest entry for `key` inside a raw edit list, read BY REFERENCE (later pushes win).
#[inline]
fn latest_in_edit<'a>(edit: &'a Edit, key: &str) -> Option<&'a Option<Val<'static>>> {
    edit.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// Lookup in a deduped last-wins member view (`None` when the key is untouched by edits).
#[inline]
fn view_lookup<'a>(
    view: Option<&'a [(String, Option<Val<'static>>)]>,
    key: &str,
) -> Option<&'a Option<Val<'static>>> {
    view?.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// Lookup in a deduped last-wins index view with a plain numeric compare.
#[inline]
fn idx_lookup<'a>(
    view: &'a [(usize, Option<Val<'static>>)],
    i: usize,
) -> Option<&'a Option<Val<'static>>> {
    view.iter().rev().find(|(ei, _)| *ei == i).map(|(_, v)| v)
}

/// Array indices carrying tombstones (deleted slots) from an index view.
fn tombstone_indices(view: &[(usize, Option<Val<'static>>)]) -> Vec<usize> {
    view.iter().filter(|(_, v)| v.is_none()).map(|(i, _)| *i).collect()
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

/// Look up a registered document by id. A one-slot thread-local memo skips the global registry
/// mutex + vector scan when the same document is accessed repeatedly (hot JSON.parse loops).
pub fn doc(id: u32) -> Option<Arc<LazyDoc>> {
    thread_local! {
        static MEMO: std::cell::RefCell<Option<(u32, Arc<LazyDoc>)>> = const { std::cell::RefCell::new(None) };
    }
    if let Some(hit) = MEMO.with(|m| {
        let g = m.borrow();
        match &*g {
            Some((mid, arc)) if *mid == id => Some(arc.clone()),
            _ => None,
        }
    }) {
        return Some(hit);
    }
    let reg = registry().lock().expect("lazy registry lock");
    let found = reg.get(id as usize).cloned();
    if let Some(arc) = &found {
        MEMO.with(|m| *m.borrow_mut() = Some((id, arc.clone())));
    }
    found
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
    fn clean_document_reads_take_the_zero_lock_tape_path_and_stay_pristine() {
        let doc = LazyDoc::build(SMALL.as_bytes()).expect("clean build");
        // A big batch of untouched reads never creates or touches CoW edit state.
        for i in 0..400 {
            assert!(doc.has(0, "meta"));
            assert!(matches!(doc.get(0, "pad"), Got::Scalar(_)));
            let items = doc.tape.lookup(0, "items").expect("items");
            let _ = doc.own_keys(items);
            let _ = doc.get(items, "0");
            if i % 10 == 0 {
                assert!(
                    matches!(doc.get(0, "type"), Got::Scalar(Scalar::Clean("batch"))),
                    "clean tape read must stay immediate"
                );
            }
        }
        // Untouched means: no edits registered, still clean, byte-exact passthrough serialize.
        let guard = doc.edits.lock().expect("edits lock");
        assert!(guard.is_empty(), "clean reads must never create edits");
        drop(guard);
        assert!(doc.is_clean());
        let mut out = Vec::new();
        doc.serialize(0, &mut out);
        assert_eq!(std::str::from_utf8(&out).unwrap(), SMALL);
    }

    #[test]
    fn repeated_cow_writes_last_win_and_added_members_serialize_exactly_once() {
        let doc = LazyDoc::build(SMALL.as_bytes()).expect("dup build");
        doc.set(0, "added", Val::Int(1));
        doc.set(0, "added", Val::Str(Cow::Owned("final".into()))); // later write wins
        doc.set(0, "count", Val::Int(2));
        doc.set(0, "count", Val::Int(3));
        assert_eq!(doc.own_keys(0), vec!["type", "meta", "items", "pad", "added", "count"]);
        let mut out = Vec::new();
        doc.serialize(0, &mut out);
        let text = std::str::from_utf8(&out).unwrap();
        assert_eq!(text.matches(r#""added":"final""#).count(), 1, "{text}");
        assert_eq!(text.matches(r#""count":3"#).count(), 1, "{text}");
        assert!(!text.contains(r#""added":1"#), "superseded value must not leak: {text}");
        match doc.get(0, "added") {
            Got::Owned(Val::Str(s)) => assert_eq!(s.as_ref(), "final"),
            other => panic!("last write must win, got {other:?}"),
        }
        // Delete-then-readd keeps the ORIGINAL document position (tape scan) with the new value.
        doc.del(0, "pad");
        doc.set(0, "pad", Val::Bool(false));
        // The readded key keeps its ORIGINAL tape position; other live members stay put.
        assert_eq!(doc.own_keys(0), vec!["type", "meta", "items", "pad", "added", "count"]);
        let mut again = Vec::new();
        doc.serialize(0, &mut again);
        let text2 = std::str::from_utf8(&again).unwrap();
        assert_eq!(text2.matches(r#""pad":false"#).count(), 1, "{text2}");
    }

    #[test]
    fn array_edits_resolve_through_numeric_indexes_not_string_keys() {
        let src = r#"[10,20,30]"#;
        let doc = LazyDoc::build(src.as_bytes()).expect("numeric build");
        doc.set(0, "0", Val::Int(11)); // overwrite existing slot
        doc.del(0, "0");
        doc.set(0, "0", Val::Int(12)); // re-set wins over the tombstone
        doc.set(0, "4", Val::Int(44));
        doc.del(0, "3"); // tombstone beyond the tape length never extends serialization
        let mut out = Vec::new();
        doc.serialize(0, &mut out);
        assert_eq!(std::str::from_utf8(&out).unwrap(), r#"[12,20,30,null,44]"#);
        assert_eq!(doc.own_keys(0), vec!["0", "1", "2", "4"]);
        // Non-index keys are ignored by the numeric view entirely.
        doc.set(0, "not-an-index", Val::Int(7));
        let mut again = Vec::new();
        doc.serialize(0, &mut again);
        assert_eq!(std::str::from_utf8(&again).unwrap(), r#"[12,20,30,null,44]"#);
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

