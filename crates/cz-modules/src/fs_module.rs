//! `node:fs` (also resolvable as `"fs"`) — minimal synchronous filesystem surface for serving
//! static HTML/CSS/JS assets in bundler-free on-device web apps.
//!
//! Rust primitives return plain descriptor objects (never JS values stored across contexts); the
//! JS wrappers below throw real `Error` objects with Node-style `ENOENT`/`EISDIR` messages when a
//! primitive reports failure.

use std::path::Path;

use rquickjs::{Array, Ctx, Function, Object, Result, Value};

use crate::{vec_to_uint8array, with_borrowed_bytes};

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
                // Phase 10C: the utf8 return needs NO second buffer — skip the `bytes` Uint8Array
                // allocation entirely (the shim only reads `d.bytes` in binary mode).
                obj.set("bytes", Value::new_null(ctx.clone()))?;
            } else {
                obj.set("text", Value::new_null(ctx.clone()))?;
                // Binary mode moves the read file's ownership straight into the typed array.
                obj.set("bytes", vec_to_uint8array(ctx, data)?)?;
            }
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

/// Phase 12 write/append primitive: `data` is a string OR a buffer kind. Typed-array payloads
/// are written through a BORROWED `&[u8]` view (no `.to_vec()`); strings move their UTF-8 bytes
/// straight into the syscall (no second copy).
fn js_fs_write<'js>(ctx: &Ctx<'js>, path: String, data: Value<'js>, append: bool) -> Result<Object<'js>> {
    let obj = Object::new(ctx.clone())?;
    let result = if let Some(s) = data.as_string() {
        let bytes = s.to_string()?.into_bytes();
        if append {
            use std::io::Write;
            match std::fs::OpenOptions::new().create(true).append(true).open(Path::new(&path)) {
                Ok(mut f) => f.write_all(&bytes),
                Err(e) => Err(e),
            }
        } else {
            std::fs::write(Path::new(&path), &bytes)
        }
    } else {
        with_borrowed_bytes(&data, |bytes| -> std::io::Result<()> {
            if append {
                use std::io::Write;
                let mut f = std::fs::OpenOptions::new().create(true).append(true).open(Path::new(&path))?;
                f.write_all(bytes)
            } else {
                std::fs::write(Path::new(&path), bytes)
            }
        })?
    };
    match result {
        Ok(()) => {
            obj.set("ok", true)?;
            obj.set("message", Value::new_null(ctx.clone()))?;
        }
        Err(e) => {
            obj.set("ok", false)?;
            obj.set("message", describe_io(if append { "append" } else { "open" }, &path, &e))?;
        }
    }
    Ok(obj)
}

/// `mkdirSync` primitive (`recursive` switches to `create_dir_all`).
fn js_fs_mkdir<'js>(ctx: &Ctx<'js>, path: String, recursive: bool) -> Result<Object<'js>> {
    let obj = Object::new(ctx.clone())?;
    let res = if recursive {
        std::fs::create_dir_all(Path::new(&path))
    } else {
        std::fs::create_dir(Path::new(&path))
    };
    match res {
        Ok(()) => {
            obj.set("ok", true)?;
            obj.set("message", Value::new_null(ctx.clone()))?;
        }
        Err(e) => {
            obj.set("ok", false)?;
            let msg = if e.kind() == std::io::ErrorKind::AlreadyExists {
                format!("EEXIST: file already exists, mkdir '{path}'")
            } else {
                describe_io("mkdir", &path, &e)
            };
            obj.set("message", msg)?;
        }
    }
    Ok(obj)
}

/// `readdirSync` primitive: entries collected and sorted in Rust (no JS sort loop).
fn js_fs_readdir<'js>(ctx: &Ctx<'js>, path: String) -> Result<Object<'js>> {
    let obj = Object::new(ctx.clone())?;
    match std::fs::read_dir(Path::new(&path)) {
        Ok(entries) => {
            let mut names: Vec<String> = Vec::new();
            for ent in entries {
                match ent {
                    Ok(e) => names.push(e.file_name().to_string_lossy().into_owned()),
                    Err(err) => {
                        obj.set("ok", false)?;
                        obj.set("names", Array::new(ctx.clone())?)?;
                        obj.set("message", describe_io("scandir", &path, &err))?;
                        return Ok(obj);
                    }
                }
            }
            names.sort();
            let arr = Array::new(ctx.clone())?;
            for (i, n) in names.iter().enumerate() {
                arr.set(i, n.as_str())?;
            }
            obj.set("ok", true)?;
            obj.set("names", arr)?;
            obj.set("message", Value::new_null(ctx.clone()))?;
        }
        Err(e) => {
            obj.set("ok", false)?;
            obj.set("names", Array::new(ctx.clone())?)?;
            obj.set("message", describe_io("scandir", &path, &e))?;
        }
    }
    Ok(obj)
}

/// `unlinkSync` primitive.
fn js_fs_unlink<'js>(ctx: &Ctx<'js>, path: String) -> Result<Object<'js>> {
    let obj = Object::new(ctx.clone())?;
    match std::fs::remove_file(Path::new(&path)) {
        Ok(()) => {
            obj.set("ok", true)?;
            obj.set("message", Value::new_null(ctx.clone()))?;
        }
        Err(e) => {
            obj.set("ok", false)?;
            obj.set("message", describe_io("unlink", &path, &e))?;
        }
    }
    Ok(obj)
}

/// `rmSync(path, { recursive?, force? })` primitive.
fn js_fs_rm<'js>(ctx: &Ctx<'js>, path: String, recursive: bool, force: bool) -> Result<Object<'js>> {
    let obj = Object::new(ctx.clone())?;
    let p = Path::new(&path);
    let res = if recursive {
        // remove_dir_all also handles plain files and missing subtrees when forced.
        std::fs::remove_dir_all(p)
    } else {
        std::fs::remove_file(p).or_else(|e| {
            if e.kind() == std::io::ErrorKind::IsADirectory {
                std::fs::remove_dir(p)
            } else {
                Err(e)
            }
        })
    };
    match res {
        Ok(()) => {
            obj.set("ok", true)?;
            obj.set("message", Value::new_null(ctx.clone()))?;
        }
        Err(e) => {
            if force && e.kind() == std::io::ErrorKind::NotFound {
                obj.set("ok", true)?;
                obj.set("message", Value::new_null(ctx.clone()))?;
            } else {
                obj.set("ok", false)?;
                obj.set("message", describe_io("rm", &path, &e))?;
            }
        }
    }
    Ok(obj)
}

/// `renameSync` primitive.
fn js_fs_rename<'js>(ctx: &Ctx<'js>, old_path: String, new_path: String) -> Result<Object<'js>> {
    let obj = Object::new(ctx.clone())?;
    match std::fs::rename(Path::new(&old_path), Path::new(&new_path)) {
        Ok(()) => {
            obj.set("ok", true)?;
            obj.set("message", Value::new_null(ctx.clone()))?;
        }
        Err(e) => {
            obj.set("ok", false)?;
            obj.set(
                "message",
                describe_io(&format!("rename '{old_path}' -> '{new_path}'"), &new_path, &e),
            )?;
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

    // ---- Phase 12 sync expansion -------------------------------------------------
    function dataCheck(d) {
        if (typeof d === "string" || d instanceof Uint8Array || d instanceof ArrayBuffer) return d;
        throw new TypeError("data must be a string, Uint8Array or ArrayBuffer");
    }
    globalThis.__czFsWriteFileSync = function (path, data) {
        const d = __czFsWrite(String(path), dataCheck(data), false);
        if (!d.ok) throw new Error(d.message);
    };
    globalThis.__czFsAppendFileSync = function (path, data) {
        const d = __czFsWrite(String(path), dataCheck(data), true);
        if (!d.ok) throw new Error(d.message);
    };
    globalThis.__czFsMkdirSync = function (path, options) {
        const d = __czFsMkdir(String(path), !!(options && options.recursive));
        if (!d.ok) throw new Error(d.message);
    };
    globalThis.__czFsReaddirSync = function (path) {
        const d = __czFsReaddir(String(path));
        if (!d.ok) throw new Error(d.message);
        return d.names;
    };
    globalThis.__czFsUnlinkSync = function (path) {
        const d = __czFsUnlink(String(path));
        if (!d.ok) throw new Error(d.message);
    };
    globalThis.__czFsRmSync = function (path, options) {
        const o = options || {};
        const d = __czFsRm(String(path), o.recursive === true, o.force === true);
        if (!d.ok) throw new Error(d.message);
    };
    globalThis.__czFsRenameSync = function (oldPath, newPath) {
        const d = __czFsRename(String(oldPath), String(newPath));
        if (!d.ok) throw new Error(d.message);
    };

    // ---- fs.promises (async wrappers over the same sync primitives; throws inside an
    // async function become rejected Promises via the runner's job drain) --------------
    globalThis.__czFsPromises = {
        readFile: async function (path, encoding) { return __czFsReadFileSync(path, encoding); },
        writeFile: async function (path, data) { __czFsWriteFileSync(path, data); },
        appendFile: async function (path, data) { __czFsAppendFileSync(path, data); },
        mkdir: async function (path, options) { __czFsMkdirSync(path, options); },
        readdir: async function (path) { return __czFsReaddirSync(path); },
        stat: async function (path) { return __czFsStatSync(path); },
        unlink: async function (path) { __czFsUnlinkSync(path); },
        rm: async function (path, options) { __czFsRmSync(path, options); },
        rename: async function (oldPath, newPath) { __czFsRenameSync(oldPath, newPath); },
        access: async function (path) {
            const p = String(path);
            if (!__czFsExists(p)) throw new Error("ENOENT: no such file or directory, access '" + p + "'");
        }
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
    ctx.globals().set(
        "__czFsWrite",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, path: String, data: Value<'js>, append: bool| -> Result<Object<'js>> {
                js_fs_write(&ctx, path, data, append)
            },
        )?,
    )?;
    ctx.globals().set(
        "__czFsMkdir",
        Function::new(ctx.clone(), |ctx: Ctx<'js>, path: String, recursive: bool| -> Result<Object<'js>> {
            js_fs_mkdir(&ctx, path, recursive)
        })?,
    )?;
    ctx.globals().set(
        "__czFsReaddir",
        Function::new(ctx.clone(), |ctx: Ctx<'js>, path: String| -> Result<Object<'js>> {
            js_fs_readdir(&ctx, path)
        })?,
    )?;
    ctx.globals().set(
        "__czFsUnlink",
        Function::new(ctx.clone(), |ctx: Ctx<'js>, path: String| -> Result<Object<'js>> {
            js_fs_unlink(&ctx, path)
        })?,
    )?;
    ctx.globals().set(
        "__czFsRm",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, path: String, recursive: bool, force: bool| -> Result<Object<'js>> {
                js_fs_rm(&ctx, path, recursive, force)
            },
        )?,
    )?;
    ctx.globals().set(
        "__czFsRename",
        Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, old_path: String, new_path: String| -> Result<Object<'js>> {
                js_fs_rename(&ctx, old_path, new_path)
            },
        )?,
    )?;
    ctx.eval::<(), _>(FS_SHIM)?;
    Ok(())
}

/// Names exported by both `node:fs` (sync surface + `promises`) and their alias.
pub const FS_EXPORT_NAMES: &[&str] = &[
    "readFileSync",
    "existsSync",
    "statSync",
    "writeFileSync",
    "appendFileSync",
    "mkdirSync",
    "readdirSync",
    "unlinkSync",
    "rmSync",
    "renameSync",
    "promises",
];

/// Names exported by `node:fs/promises` / `"fs/promises"`.
pub const FS_PROMISES_NAMES: &[&str] = &[
    "readFile",
    "writeFile",
    "appendFile",
    "mkdir",
    "readdir",
    "stat",
    "unlink",
    "rm",
    "rename",
    "access",
];

/// `node:fs` module definition: named exports AND a `default` export object.
pub struct FsModule;

impl rquickjs::module::ModuleDef for FsModule {
    fn declare<'js>(decl: &rquickjs::module::Declarations<'js>) -> Result<()> {
        for name in FS_EXPORT_NAMES {
            decl.declare(*name)?;
        }
        decl.declare("default")?;
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &rquickjs::module::Exports<'js>) -> Result<()> {
        install_fs(ctx)?;
        let promises = ctx.globals().get::<_, Object>("__czFsPromises")?;
        let default_obj = Object::new(ctx.clone())?;
        for name in FS_EXPORT_NAMES {
            let value: Value<'js> = if *name == "promises" {
                promises.clone().into_value()
            } else {
                // readFileSync -> __czFsReadFileSync (capitalized first letter).
                let mut global_name = String::from("__czFs");
                let bytes = name.as_bytes();
                global_name.push((bytes[0] as char).to_ascii_uppercase());
                global_name.push_str(&name[1..]);
                ctx.globals().get::<_, Function>(global_name.as_str())?.into_value()
            };
            exports.export(*name, value.clone())?;
            default_obj.set(*name, value)?;
        }
        exports.export("default", default_obj)?;
        Ok(())
    }
}

/// `node:fs/promises` / `"fs/promises"`: Promise-returning mirrors of the sync primitives.
pub struct FsPromisesModule;

impl rquickjs::module::ModuleDef for FsPromisesModule {
    fn declare<'js>(decl: &rquickjs::module::Declarations<'js>) -> Result<()> {
        for name in FS_PROMISES_NAMES {
            decl.declare(*name)?;
        }
        decl.declare("default")?;
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &rquickjs::module::Exports<'js>) -> Result<()> {
        install_fs(ctx)?;
        let promises = ctx.globals().get::<_, Object>("__czFsPromises")?;
        for name in FS_PROMISES_NAMES {
            exports.export(*name, promises.get::<_, Value>(*name)?)?;
        }
        exports.export("default", promises)?;
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
    fn phase12_sync_write_append_mkdir_readdir_rename_unlink_rm() {
        let dir = tmpdir();
        let src = format!(
            r#"
            import fs from "node:fs";
            const base = {:?};
            // Hermetic start: clear any state from earlier runs of this test.
            fs.rmSync(base + "/x", {{ recursive: true, force: true }});
            fs.rmSync(base + "/dup", {{ recursive: true, force: true }});
            const sub = base + "/x/y";
            fs.mkdirSync(sub, {{ recursive: true }});
            fs.writeFileSync(base + "/w.txt", "hello");
            fs.appendFileSync(base + "/w.txt", "-world");
            if (fs.readFileSync(base + "/w.txt", "utf8") !== "hello-world") throw new Error("append");
            fs.writeFileSync(base + "/b.bin", new Uint8Array([1, 2, 3]));
            if (fs.readFileSync(base + "/b.bin").length !== 3) throw new Error("bin write");
            fs.mkdirSync(base + "/dup");
            let dupErr = null;
            try {{ fs.mkdirSync(base + "/dup"); }} catch (e) {{ dupErr = e; }}
            if (!dupErr || !String(dupErr.message).includes("EEXIST")) throw new Error("EEXIST expected");
            const names = fs.readdirSync(base + "/dup");
            if (names.length !== 0) throw new Error("empty dir");
            fs.renameSync(base + "/b.bin", base + "/b2.bin");
            fs.unlinkSync(base + "/b2.bin");
            fs.rmSync(sub, {{ recursive: true, force: true }});
            if (fs.existsSync(sub)) throw new Error("rm recursive");
            fs.rmSync(base + "/missing-xyz", {{ force: true }});
            let rmErr = null;
            try {{ fs.rmSync(base + "/missing-xyz2"); }} catch (e) {{ rmErr = e; }}
            if (!rmErr || !String(rmErr.message).includes("ENOENT")) throw new Error("rm force=false ENOENT");
            globalThis.__R = "FS_SYNC12_OK";
        "#,
            dir.display().to_string()
        );
        assert_eq!(run_script(&src), "FS_SYNC12_OK");
    }

    #[test]
    fn promises_surface_and_fs_promises_aliases() {
        // Chained-promise form (no top-level await): the module evaluates synchronously,
        // then all microtasks settle via `execute_pending_job` OUTSIDE the active scope.
        let dir = tmpdir();
        let rt = Runtime::new().unwrap();
        let ctx = Context::full(&rt).unwrap();
        let body = format!(
            r#"
            import fs from "node:fs";
            import fsp from "node:fs/promises";
            import fsp2 from "fs/promises";
            const base = {:?};
            const named = [typeof fsp.readFile, typeof fsp.writeFile, typeof fsp.access];
            if (named.join(",") !== "function,function,function") throw new Error("named exports");
            // Hermetic start: wipe any leftovers from previous runs before recreating.
            fsp.rm(base + "/pp", {{ recursive: true, force: true }})
                .then(() => fsp.mkdir(base + "/pp/deep", {{ recursive: true }}))
                .then(() => fsp.writeFile(base + "/pp/a.txt", "text-A"))
                .then(() => fsp.appendFile(base + "/pp/a.txt", "!"))
                .then(() => fsp.readFile(base + "/pp/a.txt", "utf8"))
                .then((t) => {{ if (t !== "text-A!") throw new Error("pf rw"); return fsp2.readFile(base + "/pp/a.txt"); }})
                .then((bin) => {{
                    if (!(bin instanceof Uint8Array) || bin.length !== 7) throw new Error("pf bin");
                    return fsp.readdir(base + "/pp");
                }})
                .then((ents) => {{
                    // Sorted readdir sees both the file and the recursive mkdir's subdir.
                    if (ents.join(",") !== "a.txt,deep") throw new Error("pf readdir " + ents.join(","));
                    return fsp.stat(base + "/pp/a.txt");
                }})
                .then((st) => {{
                    if (!st.isFile() || st.size !== 7) throw new Error("pf stat");
                    return fsp.rename(base + "/pp/a.txt", base + "/pp/b.txt");
                }})
                .then(() => fsp.access(base + "/pp/b.txt"))
                .then(() => fsp.access(base + "/nope-here").then(() => Promise.reject(new Error("must reject"))))
                .catch((e) => {{
                    if (!String(e && e.message).includes("ENOENT")) throw e;
                    return fsp.unlink(base + "/pp/b.txt");
                }})
                .then(() => fs.promises.rm(base + "/pp", {{ recursive: true, force: true }}))
                .then(() => {{
                    if (fs.existsSync(base + "/pp")) throw new Error("p rm");
                    globalThis.__R = "FS_PROMISES_OK";
                }})
                .catch((e) => {{ globalThis.__R = "FAIL: " + (e && e.message ? e.message : String(e)); }});
        "#,
            dir.display().to_string()
        );
        ctx.with(|ctx| -> () {
            Module::declare_def::<FsModule, _>(ctx.clone(), "node:fs").unwrap();
            Module::declare_def::<FsPromisesModule, _>(ctx.clone(), "node:fs/promises").unwrap();
            Module::declare_def::<FsPromisesModule, _>(ctx.clone(), "fs/promises").unwrap();
            let p = Module::evaluate(ctx.clone(), "fsp-test.mjs", body.as_str()).unwrap();
            p.finish::<()>().unwrap();
        });
        while let Ok(true) = rt.execute_pending_job() {}
        let out = ctx.with(|ctx| -> String {
            ctx.globals().get::<_, String>("__R").ok().unwrap_or_else(|| "MISSING".into())
        });
        assert_eq!(out, "FS_PROMISES_OK");
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
