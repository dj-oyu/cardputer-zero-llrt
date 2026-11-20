//! Owned/borrowed JSON value tree used by the fast `< 4096` byte parse path and by Copy-on-Write
//! overrides inside the Lazy runtime. Clean string runs borrow directly from the input buffer
/// (`Cow::Borrowed`) so a small-document parse allocates nothing for unescaped text.

use std::borrow::Cow;

use crate::simd::{write_escaped_str, write_f64_json, write_i64};

/// Byte range inside the source document: `[start, end)`.
pub type Span = (u32, u32);

/// A JSON value. Borrowed variants point straight into the parsed input; `Val<'static>` is owned.
#[derive(Clone, Debug)]
pub enum Val<'a> {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Cow<'a, str>),
    Arr(Vec<Val<'a>>),
    Obj(Vec<(Cow<'a, str>, Val<'a>)>),
}

impl<'a> Val<'a> {
    pub fn is_null(&self) -> bool {
        matches!(self, Val::Null)
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Val::Str(s) => Some(s.as_ref()),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Val::Int(v) => Some(*v as f64),
            Val::Float(v) => Some(*v),
            _ => None,
        }
    }

    /// Object member lookup by key. Duplicate keys resolve to the LAST occurrence, matching
    /// QuickJS's object-overwrite semantics when a parsed DOM becomes JS properties.
    pub fn get(&self, key: &str) -> Option<&Val<'a>> {
        match self {
            Val::Obj(fields) => fields.iter().rev().find(|(k, _)| k.as_ref() == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Val::Arr(items) => items.len(),
            Val::Obj(fields) => fields.len(),
            _ => 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Deep-convert into an owned value (`Val<'static>`) for the Lazy override store.
    pub fn to_owned_val(&self) -> Val<'static> {
        match self {
            Val::Null => Val::Null,
            Val::Bool(b) => Val::Bool(*b),
            Val::Int(v) => Val::Int(*v),
            Val::Float(v) => Val::Float(*v),
            Val::Str(s) => Val::Str(Cow::Owned(s.as_ref().to_string())),
            Val::Arr(items) => Val::Arr(items.iter().map(|v| v.to_owned_val()).collect()),
            Val::Obj(fields) => {
                Val::Obj(fields.iter().map(|(k, v)| (Cow::Owned(k.as_ref().to_string()), v.to_owned_val())).collect())
            }
        }
    }

    /// Compact ECMA-262 `JSON.stringify` text for this value tree.
    pub fn to_json_vec(&self) -> Vec<u8> {
        let mut out = Vec::new();
        write_val(&mut out, self);
        out
    }

    pub fn to_json_string(&self) -> String {
        // JSON serialization only ever emits ASCII.
        String::from_utf8(self.to_json_vec()).expect("json output is ascii")
    }
}

/// Append the compact JSON form of a value tree (NEON/SWAR escaped strings + fast integers).
pub fn write_val(out: &mut Vec<u8>, v: &Val<'_>) {
    match v {
        Val::Null => out.extend_from_slice(b"null"),
        Val::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        Val::Int(i) => write_i64(out, *i),
        Val::Float(f) => write_f64_json(out, *f),
        Val::Str(s) => write_escaped_str(out, s.as_ref()),
        Val::Arr(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_val(out, item);
            }
            out.push(b']');
        }
        Val::Obj(fields) => {
            out.push(b'{');
            for (i, (k, val)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_escaped_str(out, k.as_ref());
                out.push(b':');
                write_val(out, val);
            }
            out.push(b'}');
        }
    }
}
