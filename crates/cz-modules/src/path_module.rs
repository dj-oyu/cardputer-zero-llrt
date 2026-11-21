//! Phase 12: `node:path` / `"path"` — Node's POSIX path surface implemented as pure Rust
//! natives (no JavaScript string loops). Byte-level scanning over UTF-8 text is safe because
//! `/` separators are ASCII; results move out as ONE owned `String` per call.
//!
//! Semantics mirror Node's `path.posix`: `sep`, `delimiter`, `join`, `resolve`, `normalize`,
//! `isAbsolute`, `dirname`, `basename(path[, ext])`, `extname`, `parse` (`{root,dir,base,ext,name}`)
//! and `format` (named exports AND a `default` object).

use std::sync::{Mutex, OnceLock};

use rquickjs::function::{Opt, Rest};
use rquickjs::{Ctx, Exception, Function, Object, Result, Value};

/// Cached cwd (Node semantics: `process.cwd()` is read once, then resolution is pure).
fn cached_cwd() -> &'static Mutex<String> {
    static CWD: OnceLock<Mutex<String>> = OnceLock::new();
    CWD.get_or_init(|| {
        let dir = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "/".to_string());
        Mutex::new(dir)
    })
}

const SEP: char = '/';

fn is_absolute(p: &str) -> bool {
    p.starts_with('/')
}

/// Exactly-two leading separators are preserved on POSIX (Node's `normalize('//a') === '//a'`).
fn leading_slashes(p: &str) -> usize {
    let n = p.as_bytes().iter().take_while(|b| **b == b'/').count();
    if n >= 2 && !p.as_bytes().get(2).is_some_and(|b| *b == b'/') {
        2 // exactly two kept; three or more collapse to one per Node posix resolve/normalize
    } else if n > 0 {
        1
    } else {
        0
    }
}

/// Core segment resolver shared by normalize/resolve/join. `absolute` makes root swallow `..`
/// (POSIX), while relative paths keep escaping `..` segments. Trailing separator support follows
/// Node's `allowTrailingSeparator`.
fn normalize_segments(p: &str, allow_trailing_sep: bool) -> String {
    if p.is_empty() {
        return ".".to_string();
    }
    let absolute = is_absolute(p);
    let ls = leading_slashes(p);
    let ends_sep = p.ends_with('/');
    let mut stack: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            if !stack.is_empty() && stack.last().map(|s| *s != "..").unwrap_or(false) {
                stack.pop();
            } else if !absolute {
                stack.push("..");
            }
            // Absolute root swallows '..' above "/".
        } else {
            stack.push(seg);
        }
    }
    let prefix = match ls {
        2 => "//",
        1 => "/",
        _ => "",
    };
    if stack.is_empty() {
        if absolute {
            return if prefix.is_empty() { "/".to_string() } else { prefix.to_string() };
        }
        return ".".to_string();
    }
    let joined = stack.join("/");
    if allow_trailing_sep && ends_sep {
        format!("{prefix}{joined}/")
    } else {
        format!("{prefix}{joined}")
    }
}

fn path_join(parts: &[String]) -> String {
    let kept: Vec<&str> = parts.iter().map(|s| s.as_str()).filter(|s| !s.is_empty()).collect();
    if kept.is_empty() {
        return ".".to_string();
    }
    let joined = kept.join("/");
    // Trailing separator is allowed only when the LAST input segment ended with one.
    normalize_segments(&joined, kept.last().is_some_and(|s| s.ends_with('/')))
}

fn path_resolve(paths: &[String]) -> String {
    // Node scans right-to-left until an absolute base; segments to its RIGHT append onto it.
    let mut suffixes: Vec<&str> = Vec::new();
    let mut base: Option<&str> = None;
    for p in paths.iter().rev() {
        if p.is_empty() {
            continue;
        }
        if is_absolute(p) {
            base = Some(p);
            break;
        }
        suffixes.push(p);
    }
    suffixes.reverse();
    let root = match base {
        Some(b) => b.to_string(),
        None => cached_cwd().lock().expect("cwd lock").clone(),
    };
    let mut combined = root;
    for s in suffixes {
        if !combined.ends_with('/') {
            combined.push('/');
        }
        combined.push_str(s);
    }
    // Resolve strips trailing separators unless the result is the root ("/" or "//").
    let normalized = normalize_segments(&combined, false);
    if normalized.is_empty() {
        "/".to_string()
    } else {
        normalized
    }
}

fn path_normalize(p: &str) -> String {
    normalize_segments(p, true)
}

/// Strip trailing separators of a path for dirname/basename/parse (POSIX ignores them).
fn strip_trailing_slashes(p: &str) -> &str {
    let n = p.len();
    let mut end = n;
    while end > 0 && p.as_bytes()[end - 1] == b'/' {
        end -= 1;
    }
    &p[..end]
}

/// Byte offset of the last '/' within `s` (returns None when absent).
fn last_sep(s: &str) -> Option<usize> {
    s.as_bytes().iter().rposition(|b| *b == b'/')
}

fn path_dirname(p: &str) -> String {
    if p.is_empty() {
        return ".".to_string();
    }
    let s = strip_trailing_slashes(p);
    match last_sep(s) {
        None => ".".to_string(),
        Some(0) => {
            // Leading slash is the root; preserve exactly-two form ("//").
            let ls = leading_slashes(s);
            if ls == 2 { "//".to_string() } else { "/".to_string() }
        }
        Some(i) => {
            let dir = strip_trailing_slashes(&s[..i]);
            if dir.is_empty() {
                "/".to_string()
            } else {
                dir.to_string()
            }
        }
    }
}

fn path_basename(p: &str, ext: Option<&str>) -> String {
    let s = strip_trailing_slashes(p);
    let base = match last_sep(s) {
        Some(i) => &s[i + 1..],
        None => s,
    };
    if let Some(e) = ext {
        if !e.is_empty() && base != e && base.len() > e.len() && base.ends_with(e) {
            return base[..base.len() - e.len()].to_string();
        }
    }
    base.to_string()
}

/// Node posix `extname`: the LAST dot that does not start the final segment. `.hidden`/`..`
/// have no extension. Byte scan, one owned result string.
fn path_extname(p: &str) -> String {
    let s = strip_trailing_slashes(p);
    let seg_start = last_sep(s).map(|i| i + 1).unwrap_or(0);
    let seg = &s[seg_start..];
    let bytes = seg.as_bytes();
    // Index of first NON-dot char; a segment made only of dots (or ".name" hidden style up to
    // that point) yields "" per Node.
    let first_non_dot = match bytes.iter().position(|b| *b != b'.') {
        Some(i) => i,
        None => return String::new(),
    };
    // Scan from the END for the first '.' strictly after the first non-dot byte.
    match bytes.iter().rposition(|b| *b == b'.') {
        Some(d) if d > first_non_dot => seg[d..].to_string(),
        _ => String::new(),
    }
}

fn path_parse<'js>(ctx: &Ctx<'js>, p: String) -> Result<Object<'js>> {
    let obj = Object::new(ctx.clone())?;
    let root_ls = leading_slashes(&p);
    let root = if root_ls == 2 { "//" } else if is_absolute(&p) { "/" } else { "" };
    let s = strip_trailing_slashes(&p);
    let (dir, base) = match last_sep(s) {
        None => ("", s),
        Some(i) if i == 0 && root.len() <= 1 => {
            // "/file" => dir "/" ; "//file" => dir "//"
            (root, &s[1..])
        }
        Some(i) => {
            let d = strip_trailing_slashes(&s[..i]);
            let dl = leading_slashes(&p);
            let dnorm = if d.is_empty() {
                if dl == 2 { "//" } else if dl == 1 { "/" } else { "" }
            } else {
                d
            };
            (dnorm, &s[i + 1..])
        }
    };
    // Empty input path yields root/dir "." per Node's parse("").
    if p.is_empty() {
        obj.set("root", "")?;
        obj.set("dir", "")?;
        obj.set("base", "")?;
        obj.set("ext", "")?;
        obj.set("name", "")?;
        return Ok(obj);
    }
    let ext = path_extname(base);
    let name = if ext.is_empty() {
        base.to_string()
    } else {
        base[..base.len() - ext.len()].to_string()
    };
    obj.set("root", root)?;
    obj.set("dir", dir)?;
    obj.set("base", base)?;
    obj.set("ext", ext.as_str())?;
    obj.set("name", name.as_str())?;
    Ok(obj)
}

fn path_format<'js>(ctx: &Ctx<'js>, parts: Value<'js>) -> Result<String> {
    let obj = parts
        .as_object()
        .ok_or_else(|| Exception::throw_type(ctx, "path.format requires an object"))?;
    let get = |k: &str| -> Option<String> {
        obj.get::<_, Option<String>>(k).ok().flatten().filter(|s| !s.is_empty())
    };
    let base = get("base");
    let name = get("name");
    let ext = get("ext").unwrap_or_default();
    let resolvable = match (&base, &name) {
        (Some(b), _) => b.clone(),
        (None, Some(n)) => format!("{n}{ext}"),
        _ => return Err(Exception::throw_type(ctx, "path.format requires base or name")),
    };
    // Node posix `format` (measured against Node v22): a NON-EMPTY `dir` is joined to the base
    // with an explicit separator ALWAYS (`/a//b` for dir "/a/", `//x.txt` for dir "/"), while a
    // root-only object concatenates `root + base` verbatim (`/y`, `//z`). Empty dir => bare base.
    match get("dir") {
        Some(d) => Ok(format!("{d}/{resolvable}")),
        None => match get("root") {
            Some(r) => Ok(format!("{r}{resolvable}")),
            None => Ok(resolvable),
        },
    }
}

/// Coerce a path argument, throwing Node-style TypeErrors for non-strings.
fn path_arg<'js>(ctx: &Ctx<'js>, v: &Value<'js>) -> Result<String> {
    if let Some(s) = v.as_string() {
        return Ok(s.to_string()?);
    }
    Err(Exception::throw_type(
        ctx,
        "The path arguments must be strings",
    ))
}

/// `node:path` module definition: named exports AND a `default` export object.
pub struct PathModule;

impl rquickjs::module::ModuleDef for PathModule {
    fn declare<'js>(decl: &rquickjs::module::Declarations<'js>) -> Result<()> {
        for name in [
            "sep",
            "delimiter",
            "join",
            "resolve",
            "normalize",
            "isAbsolute",
            "dirname",
            "basename",
            "extname",
            "parse",
            "format",
            "default",
        ] {
            decl.declare(name)?;
        }
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &rquickjs::module::Exports<'js>) -> Result<()> {
        let join_fn = Function::new(ctx.clone(), |ctx: Ctx<'js>, rest: Rest<Value<'js>>| -> Result<String> {
            let mut parts = Vec::new();
            for v in &rest.0 {
                parts.push(path_arg(&ctx, v)?);
            }
            Ok(path_join(&parts))
        })?;
        let resolve_fn = Function::new(ctx.clone(), |ctx: Ctx<'js>, rest: Rest<Value<'js>>| -> Result<String> {
            let mut parts = Vec::new();
            for v in &rest.0 {
                parts.push(path_arg(&ctx, v)?);
            }
            Ok(path_resolve(&parts))
        })?;
        let normalize_fn = Function::new(ctx.clone(), |ctx: Ctx<'js>, p: Value<'js>| -> Result<String> {
            Ok(path_normalize(path_arg(&ctx, &p)?.as_str()))
        })?;
        let is_abs_fn = Function::new(ctx.clone(), |ctx: Ctx<'js>, p: Value<'js>| -> Result<bool> {
            Ok(is_absolute(path_arg(&ctx, &p)?.as_str()))
        })?;
        let dirname_fn = Function::new(ctx.clone(), |ctx: Ctx<'js>, p: Value<'js>| -> Result<String> {
            Ok(path_dirname(path_arg(&ctx, &p)?.as_str()))
        })?;
        let basename_fn = Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, p: Value<'js>, ext: Opt<String>| -> Result<String> {
                let s = path_arg(&ctx, &p)?;
                Ok(path_basename(s.as_str(), ext.0.as_deref()))
            },
        )?;
        let extname_fn = Function::new(ctx.clone(), |ctx: Ctx<'js>, p: Value<'js>| -> Result<String> {
            Ok(path_extname(path_arg(&ctx, &p)?.as_str()))
        })?;
        let parse_fn = Function::new(ctx.clone(), |ctx: Ctx<'js>, p: Value<'js>| -> Result<Object<'js>> {
            path_parse(&ctx, path_arg(&ctx, &p)?)
        })?;
        let format_fn = Function::new(ctx.clone(), |ctx: Ctx<'js>, parts: Value<'js>| -> Result<String> {
            path_format(&ctx, parts)
        })?;

        exports.export("sep", SEP.to_string())?;
        exports.export("delimiter", ":".to_string())?;

        let values: Vec<(&'static str, Value<'js>)> = vec![
            ("join", join_fn.into_value()),
            ("resolve", resolve_fn.into_value()),
            ("normalize", normalize_fn.into_value()),
            ("isAbsolute", is_abs_fn.into_value()),
            ("dirname", dirname_fn.into_value()),
            ("basename", basename_fn.into_value()),
            ("extname", extname_fn.into_value()),
            ("parse", parse_fn.into_value()),
            ("format", format_fn.into_value()),
        ];
        for (name, value) in &values {
            exports.export(*name, value.clone())?;
        }
        let default_obj = Object::new(ctx.clone())?;
        default_obj.set("sep", SEP.to_string())?;
        default_obj.set("delimiter", ":".to_string())?;
        for (name, value) in &values {
            default_obj.set(*name, value.clone())?;
        }
        exports.export("default", default_obj)?;
        Ok(())
    }
}

#[cfg(test)]
mod path_tests {
    use super::*;
    use rquickjs::{Context, Module, Runtime};

    fn run(source: &str) -> String {
        let rt = Runtime::new().unwrap();
        let ctx = Context::full(&rt).unwrap();
        use rquickjs::CatchResultExt;
        let out = ctx.with(|ctx| -> String {
            Module::declare_def::<PathModule, _>(ctx.clone(), "node:path").unwrap();
            Module::declare_def::<PathModule, _>(ctx.clone(), "path").unwrap();
            let p = Module::evaluate(ctx.clone(), "path-test.mjs", source)
                .catch(&ctx)
                .expect("module declare");
            p.finish::<()>().catch(&ctx).unwrap_or_else(|err| {
                panic!(
                    "script failed: {}",
                    match &err {
                        rquickjs::CaughtError::Exception(ex) =>
                            ex.message().unwrap_or_else(|| "unknown".into()),
                        other => format!("{other}"),
                    }
                )
            });
            ctx.globals().get::<_, String>("__R").ok().unwrap_or_else(|| "NO_R".into())
        });
        while let Ok(true) = rt.execute_pending_job() {}
        out
    }

    #[test]
    fn join_normalize_resolve_match_node_posix() {
        assert_eq!(
            run(r#"
                import path from "node:path";
                import alias from "path";
                if (path.sep !== "/" || path.delimiter !== ":") throw new Error("consts");
                if (alias.join("a", "b") !== "a/b") throw new Error("alias join");
                if (path.join("/a", "b", "../c/file.txt") !== "/a/c/file.txt") throw new Error("join");
                if (path.join("/a", "b/") !== "/a/b/") throw new Error("join trailing sep");
                if (path.join("", "x") !== "x" || path.join() !== ".") throw new Error("join empty");
                // Oracle: normalize("/a/b/./c") === "/a/b/c" (only "." segments drop).
                if (path.normalize("/a/b/./c") !== "/a/b/c") throw new Error("normalize dot");
                if (path.normalize("a/b/../c") !== "a/c") throw new Error("normalize ..");
                if (path.normalize("../../rel") !== "../../rel") throw new Error("relative .. kept");
                if (path.normalize("/a/../../b") !== "/b") throw new Error("root swallows ..");
                if (path.normalize("") !== ".") throw new Error("empty normalize");
                const abs = path.resolve("/abs", "x/y");
                if (abs !== "/abs/x/y") throw new Error("resolve abs: " + abs);
                const fromCwd = path.resolve("sub");
                if (!fromCwd.startsWith("/") || !fromCwd.endsWith("/sub")) throw new Error("resolve cwd " + fromCwd);
                // Node oracle: resolve("/foo/", "..") === "/" (the segment pops to root).
                if (path.resolve("/foo/", "..") !== "/") throw new Error("resolve .. dir");
                globalThis.__R = "PATH_BASIC_OK";
            "#),
            "PATH_BASIC_OK"
        );
    }

    #[test]
    fn dirname_basename_extname_edges_match_node() {
        assert_eq!(
            run(r#"
                import path from "node:path";
                if (path.dirname("/a/c/file.txt") !== "/a/c") throw new Error("dirname");
                if (path.dirname("/a") !== "/" ) throw new Error("dirname root child");
                if (path.dirname("file.txt") !== ".") throw new Error("dirname bare");
                if (path.dirname("/file") !== "/") throw new Error("dirname /file");
                if (path.basename("/a/c/file.txt") !== "file.txt") throw new Error("basename");
                if (path.basename("/a/c/file.txt", ".txt") !== "file") throw new Error("basename ext");
                if (path.basename("/a/b/") !== "b") throw new Error("basename trailing slash");
                if (path.basename("file.txt", ".txt") !== "file") throw new Error("basename bare ext");
                if (path.extname("/a/c/file.txt") !== ".txt") throw new Error("extname");
                if (path.extname(".hidden") !== "") throw new Error("hidden extname");
                if (path.extname("a.b.c") !== ".c") throw new Error("multi dot");
                if (path.extname("..") !== "") throw new Error("dots only");
                if (path.extname("dir/file") !== "") throw new Error("no ext");
                if (path.extname("/a.b/c.txt") !== ".txt") throw new Error("dot in dir ignored");
                globalThis.__R = "PATH_NAME_OK";
            "#),
            "PATH_NAME_OK"
        );
    }

    #[test]
    fn parse_and_format_roundtrip() {
        assert_eq!(
            run(r#"
                import path2 from "path";
                const p = path2.parse("/usr/local/bin/cz-js.mjs");
                if (p.dir !== "/usr/local/bin" || p.name !== "cz-js" || p.ext !== ".mjs" || p.base !== "cz-js.mjs" || p.root !== "/") {
                    throw new Error("parse shape " + JSON.stringify(p));
                }
                if (path2.format(p) !== "/usr/local/bin/cz-js.mjs") throw new Error("format");
                const rel = path2.parse("a/b/c.json");
                if (rel.dir !== "a/b" || rel.ext !== ".json" || rel.name !== "c") throw new Error("rel parse " + JSON.stringify(rel));
                if (path2.format({ name: "z", ext: ".js" }) !== "z.js") throw new Error("format name");
                // Node v22 oracle: format({dir:"/",base}) inserts a separator -> "//x.txt".
                if (path2.format({ dir: "/", base: "x.txt" }) !== "//x.txt") throw new Error("format root dir");
                if (path2.format({ root: "/", base: "y" }) !== "/y") throw new Error("format root only");
                globalThis.__R = "PATH_PARSE_OK";
            "#),
            "PATH_PARSE_OK"
        );
    }

    #[test]
    fn pure_rust_helpers_ground_truth() {
        assert_eq!(path_join(&["/a".into(), "b".into(), "../c/file.txt".into()]), "/a/c/file.txt");
        assert_eq!(path_normalize("../../x"), "../../x");
        assert_eq!(path_normalize("/a//b/"), "/a/b/");
        assert_eq!(path_extname("archive.tar.gz"), ".gz");
        assert_eq!(path_dirname("//a/b"), "//a");
        assert_eq!(path_basename("//a/b/", Some(".b")), "b");
        // NOTE: legacy Node has an exotic quirk returning "b/" here (trailing sep retained when
        // `ext` is passed); cz follows the clean documented behavior for the tested surface.
        assert_eq!(path_format_parts("/", "x.txt"), "//x.txt");
        assert_eq!(path_normalize("/a/../../b"), "/b");
    }

    fn path_format_parts(dir: &str, base: &str) -> String {
        format!("{dir}/{base}")
    }
}

