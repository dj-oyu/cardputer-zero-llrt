//! `node:fs` (also resolvable as `"fs"`) — minimal synchronous filesystem surface for serving
//! static HTML/CSS/JS assets in bundler-free on-device web apps.
//!
//! Rust primitives return plain descriptor objects (never JS values stored across contexts); the
//! JS wrappers below throw real `Error` objects with Node-style `ENOENT`/`EISDIR` messages when a
//! primitive reports failure.

use std::path::Path;

use rquickjs::{Ctx, Function, Object, Result, TypedArray, Value};

/// Read a file into a descriptor: `{ ok, text, bytes, message }`.
fn js_fs_read<'js>(ctx: &Ctx<'js>, path: String, want_text: bool) -> Result<Object<'js>> {
    let obj = Object::new(ctx.clone())?;
    match std::fs::read(Path::new(&path)) {
        Ok(data) => {
            obj.set("ok", true)?;
            obj.set("size", data.len() as f64)?;
            if want_text {
                // Text mode is lossy-tolerant like Node's default utf8 decoding.
                obj.set("text", String::from_utf8_lossy(&data).as_ref())?;
            } else {
                obj.set("text", Value::new_null(ctx.clone()))?;
            }
            let ta = TypedArray::<u8>::new_copy(ctx.clone(), &data[..])?;
            obj.set("bytes", ta)?;
            obj.set("message", Value::new_null(ctx.clone()))?;
        }
        Err(e) => {
            obj.set("ok", false)?;
            obj.set("size", 0f64)?;
            obj.set("text", Value::new_null(ctx.clone()))?;
            obj.set("bytes", Value::new_null(ctx.clone()))?;
            obj.set("message", describe_io("open", &path, &e))?;
        }
    }
    Ok(obj)
}

/// `existsSync` primitive.
fn js_fs_exists(path: String) -> bool {
    Path::new(&path).exists()
}

/// `statSync` primitive: `{ ok, size, isFile, isDirectory, message }`.
fn js_fs_stat<'js>(ctx: &Ctx<'js>, path: String) -> Result<Object<'js>> {
    let obj = Object::new(ctx.clone())?;
    match std::fs::metadata(Path::new(&path)) {
        Ok(md) => {
            obj.set("ok", true)?;
            obj.set("size", md.len() as f64)?;
            obj.set("isFile", md.is_file())?;
            obj.set("isDirectory", md.is_dir())?;
            obj.set("message", Value::new_null(ctx.clone()))?;
        }
        Err(e) => {
            obj.set("ok", false)?;
            obj.set("size", 0f64)?;
            obj.set("isFile", false)?;
            obj.set("isDirectory", false)?;
            obj.set("message", describe_io("stat", &path, &e))?;
        }
    }
    Ok(obj)
}

/// Node-style error text (`ENOENT: no such file or directory, open '<path>'`), fallback to the
/// raw io error display for unusual failures.
fn describe_io(syscall: &str, path: &str, e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => {
            format!("ENOENT: no such file or directory, {syscall} '{path}'")
        }
        std::io::ErrorKind::PermissionDenied => {
            format!("EACCES: permission denied, {syscall} '{path}'")
        }
        std::io::ErrorKind::IsADirectory => {
            format!("EISDIR: illegal operation on a directory, {syscall} '{path}'")
        }
        _ => format!("{syscall} '{path}' failed: {e}"),
    }
}

/// JS wrappers implementing the Node-compatible semantics on top of the primitives.
const FS_SHIM: &str = r#"
(function () {
    function isTextEncoding(enc) {
        if (typeof enc === "string") {
            const e = enc.toLowerCase();
            return e === "utf8" || e === "utf-8";
        }
        if (enc && typeof enc === "object") return isTextEncoding(enc.encoding);
        return false;
    }

    globalThis.__czFsReadFileSync = function (path, encoding) {
        const d = __czFsRead(String(path), isTextEncoding(encoding));
        if (!d.ok) throw new Error(d.message);
        // utf8/utf-8 yields a string, otherwise the raw bytes as Uint8Array (Node-like).
        return isTextEncoding(encoding) ? d.text : d.bytes;
    };
    globalThis.__czFsExistsSync = function (path) { return __czFsExists(String(path)) === true; };
    globalThis.__czFsStatSync = function (path) {
        const d = __czFsStat(String(path));
        if (!d.ok) throw new Error(d.message);
        return {
            size: d.size,
            isFile: () => d.isFile === true,
            isDirectory: () => d.isDirectory === true,
        };
    };
})();
"#;

/// Register fs primitives + shim on a context (idempotent, safe alongside `node:http` glue).
pub fn install_fs<'js>(ctx: &Ctx<'js>) -> Result<()> {
    ctx.globals().set(
        "__czFsRead",
        Function::new(ctx.clone(), |ctx: Ctx<'js>, path: String, want_text: bool| -> Result<Object<'js>> {
            js_fs_read(&ctx, path, want_text)
        })?,
    )?;
    ctx.globals().set("__czFsExists", Function::new(ctx.clone(), js_fs_exists)?)?;
    ctx.globals().set(
        "__czFsStat",
        Function::new(ctx.clone(), |ctx: Ctx<'js>, path: String| -> Result<Object<'js>> {
            js_fs_stat(&ctx, path)
        })?,
    )?;
    ctx.eval::<(), _>(FS_SHIM)?;
    Ok(())
}

/// `node:fs` module definition: named exports AND a `default` export object.
pub struct FsModule;

impl rquickjs::module::ModuleDef for FsModule {
    fn declare<'js>(decl: &rquickjs::module::Declarations<'js>) -> Result<()> {
        for name in ["readFileSync", "existsSync", "statSync", "default"] {
            decl.declare(name)?;
        }
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &rquickjs::module::Exports<'js>) -> Result<()> {
        install_fs(ctx)?;
        let read_fn = ctx.globals().get::<_, Function>("__czFsReadFileSync")?;
        let exists_fn = ctx.globals().get::<_, Function>("__czFsExistsSync")?;
        let stat_fn = ctx.globals().get::<_, Function>("__czFsStatSync")?;
        exports.export("readFileSync", read_fn.clone())?;
        exports.export("existsSync", exists_fn.clone())?;
        exports.export("statSync", stat_fn.clone())?;
        let default_obj = Object::new(ctx.clone())?;
        default_obj.set("readFileSync", read_fn)?;
        default_obj.set("existsSync", exists_fn)?;
        default_obj.set("statSync", stat_fn)?;
        exports.export("default", default_obj)?;
        Ok(())
    }
}

#[cfg(test)]
mod fs_tests {
    use super::*;
    use rquickjs::{Context, Module, Runtime};

    fn run_script(source: &str) -> String {
        let rt = Runtime::new().unwrap();
        let ctx = Context::full(&rt).unwrap();
        let out = ctx.with(|ctx| -> String {
            Module::declare_def::<FsModule, _>(ctx.clone(), "node:fs").unwrap();
            let p = Module::evaluate(ctx.clone(), "fs-test.mjs", source).unwrap();
            // Jobs must be pumped outside the active scope (rquickjs reentrancy rule).
            p.finish::<()>().unwrap();
            ctx.globals().get::<_, String>("__R").unwrap_or_else(|_| "NO_R".into())
        });
        while let Ok(true) = rt.execute_pending_job() {}
        out
    }

    fn tmpdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cz-fs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        dir
    }

    #[test]
    fn read_file_sync_utf8_returns_string_and_bytes_mode_returns_uint8array() {
        let dir = tmpdir();
        let html = dir.join("index.html");
        std::fs::write(&html, "<!doctype html><h1>CardputerZero</h1>").expect("write html");
        let src = r#"
            import fs from "node:fs";
            const p = __PATH__;
            const text = fs.readFileSync(p, "utf8");
            if (typeof text !== "string") throw new Error("utf8 mode must return string");
            if (!text.includes("CardputerZero")) throw new Error("content lost");
            const alias = fs.readFileSync(p, "UTF-8");
            if (alias !== text) throw new Error("UTF-8 alias mismatch");
            const opts = fs.readFileSync(p, { encoding: "utf-8" });
            if (opts !== text) throw new Error("{ encoding } form mismatch");
            const raw = fs.readFileSync(p);
            if (!(raw instanceof Uint8Array)) throw new Error("no-encoding mode must yield Uint8Array");
            if (raw.length !== 37) throw new Error("byte length wrong: " + raw.length); // exact UTF-8 length
            if (raw[0] !== 60 || raw[1] !== 33) throw new Error("byte content wrong");
            const base64ish = fs.readFileSync(p, { encoding: "base64" });
            if (!(base64ish instanceof Uint8Array)) throw new Error("unknown encoding falls back to bytes");
            globalThis.__R = "FS_READ_OK";
        "#.replace("__PATH__", &format!("{:?}", html.display().to_string()));
        assert_eq!(run_script(&src), "FS_READ_OK");
    }

    #[test]
    fn exists_sync_and_stat_sync_shape_for_files_and_directories() {
        let dir = tmpdir();
        let file = dir.join("stat-target.json");
        std::fs::write(&file, "{\"a\":42}").expect("write json");
        let sub = dir.join("nested-dir");
        std::fs::create_dir_all(&sub).expect("mkdir");
        let src = r#"
            import fs from "node:fs";
            const f = __FILE__;
            const d = __DIR__;
            if (fs.existsSync(f) !== true) throw new Error("file should exist");
            if (fs.existsSync(d) !== true) throw new Error("dir should exist");
            if (fs.existsSync("/tmp/cz-definitely-missing-file-xyz") !== false) throw new Error("missing must be false");
            const st = fs.statSync(f);
            if (st.size !== 8) throw new Error("size wrong " + st.size); // {"a":42} is 8 bytes
            if (st.isFile() !== true || st.isDirectory() !== false) throw new Error("file flags wrong");
            const dst = fs.statSync(d);
            if (dst.isDirectory() !== true || dst.isFile() !== false) throw new Error("dir flags wrong");
            if (typeof st.size !== "number") throw new Error("size must be number");
            globalThis.__R = "FS_EXISTS_STAT_OK";
        "#
        .replace("__FILE__", &format!("{:?}", file.display().to_string()))
        .replace("__DIR__", &format!("{:?}", sub.display().to_string()));
        assert_eq!(run_script(&src), "FS_EXISTS_STAT_OK");
    }

    #[test]
    fn missing_file_and_missing_stat_throw_real_errors() {
        let src = r#"
            import fs from "node:fs";
            import * as ns from "node:fs";
            for (const n of ["readFileSync","existsSync","statSync"]) {
                if (!(n in ns)) throw new Error("missing named export " + n);
                if (!(n in fs)) throw new Error("missing default member " + n);
            }
            const miss = "/tmp/cz-missing-definitely-not-here";
            let readErr = null, statErr = null;
            try { fs.readFileSync(miss, "utf8"); } catch (e) { readErr = e; }
            try { fs.statSync(miss); } catch (e) { statErr = e; }
            if (!(readErr instanceof Error)) throw new Error("read must throw a real Error");
            if (!String(readErr.message).includes("ENOENT")) throw new Error("ENOENT expected: " + readErr.message);
            if (!String(readErr.message).includes(miss)) throw new Error("path must appear in message");
            if (!(statErr instanceof Error)) throw new Error("stat must throw a real Error");
            if (!String(statErr.message).includes("ENOENT")) throw new Error("stat ENOENT expected");
            globalThis.__R = "FS_THROW_OK";
        "#;
        assert_eq!(run_script(src), "FS_THROW_OK");
    }

    #[test]
    fn directory_read_surfaces_error_without_crashing() {
        let dir = tmpdir();
        let sub = dir.join("as-read-target");
        std::fs::create_dir_all(&sub).expect("mkdir");
        let src = r#"
            import fs from "node:fs";
            let err = null;
            try { fs.readFileSync(__DIR__, "utf8"); } catch (e) { err = e; }
            if (!(err instanceof Error)) throw new Error("directory read must throw");
            // Message is either EISDIR-style or the raw io description; both are acceptable as long
            // as the path and a failure marker are present.
            const m = String(err.message);
            if (!m.includes(__DIR__) && !m.includes("failed")) throw new Error("message shape: " + m);
            globalThis.__R = "FS_DIR_READ_ERR";
        "#.replace("__DIR__", &format!("{:?}", sub.display().to_string()));
        assert_eq!(run_script(&src), "FS_DIR_READ_ERR");
    }
}
