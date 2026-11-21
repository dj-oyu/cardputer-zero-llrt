//! Phase 13: `node:os` / `"os"` — the lightweight Node OS surface.
//!
//! Every export is a pure Rust native: `platform`/`arch` are compile-time `cfg` strings, `EOL` is
//! one static string, `tmpdir`/`homedir` read `std::env`, `hostname` fills a stack buffer via
//! `libc::gethostname` (no heap until the final lossy conversion), `cpus` mirrors
//! `std::thread::available_parallelism()` (one Array of `{ model, speed }` records), and `uptime`
//! scans `/proc/uptime` without lowercasing or splitting anything into many `String`s (guidelines §1).

use std::io::Read;

use rquickjs::{Array, Ctx, Function, Object, Result};

/// Node `os.platform()` spelling for this build target.
fn platform_name() -> &'static str {
    match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

/// Node `os.arch()` spelling (`arm64` on the CardputerZero Cortex-A53).
fn arch_name() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        "arm" => "arm",
        "riscv64" => "riscv64",
        other => other,
    }
}

/// `$HOME`, falling back to `/root` (the Node convention on Unix when unset).
fn homedir() -> String {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() => h,
        _ => "/root".to_string(),
    }
}

/// `libc::gethostname` into a stack buffer; NUL-terminated, lossy, with `"localhost"` as the
/// documented fallback if the syscall refuses or answers empty.
fn hostname() -> String {
    let mut buf = [0 as libc::c_char; 256];
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr(), buf.len()) };
    if rc != 0 {
        return "localhost".to_string();
    }
    // Find the first NUL without copying anything out of the stack array first.
    let bytes = &buf[..];
    let end = bytes.iter().position(|&c| c == 0).unwrap_or(bytes.len());
    let text = String::from_utf8_lossy(unsafe {
        std::slice::from_raw_parts(bytes.as_ptr() as *const u8, end)
    });
    if text.is_empty() {
        "localhost".to_string()
    } else {
        text.into_owned()
    }
}

/// Nominal CPU model label for the CardputerZero (and a generic one for other builds); `speed` is
/// MHz, matching Node's record shape.
fn cpu_model_speed() -> (&'static str, u32) {
    match std::env::consts::ARCH {
        "aarch64" => ("Cortex-A53", 1000),
        "x86_64" => ("x86_64 compatible processor", 1000),
        _ => ("generic processor", 0),
    }
}

/// Whole seconds of system uptime from `/proc/uptime`'s first field (one tiny read, no splits).
fn uptime_secs() -> f64 {
    let mut file = match std::fs::File::open("/proc/uptime") {
        Ok(f) => f,
        Err(_) => return 0.0,
    };
    let mut buf = [0u8; 64];
    let n = match file.read(&mut buf[..]) {
        Ok(n) => n,
        Err(_) => return 0.0,
    };
    let text = String::from_utf8_lossy(&buf[..n]);
    // Scan up to the first whitespace; parse as a float and floor to whole seconds (Node shape).
    let token = text.trim_start().split(|c: char| c == ' ' || c == '\t').next().unwrap_or("");
    token.parse::<f64>().map(|s| s.floor()).unwrap_or(0.0)
}

/// `os.cpus()` — one entry per `available_parallelism()` logical CPU.
fn cpus_impl<'js>(ctx: &Ctx<'js>) -> Result<Array<'js>> {
    let n = std::thread::available_parallelism().map(|p| p.get()).unwrap_or(1);
    let (model, speed) = cpu_model_speed();
    let arr = Array::new(ctx.clone())?;
    for i in 0..n {
        let rec = Object::new(ctx.clone())?;
        rec.set("model", model)?;
        rec.set("speed", speed)?;
        arr.set(i, rec)?;
    }
    Ok(arr)
}

/// `node:os` / `"os"`: named exports AND a `default` object (same natives both ways).
pub struct OsModule;

impl rquickjs::module::ModuleDef for OsModule {
    fn declare<'js>(decl: &rquickjs::module::Declarations<'js>) -> Result<()> {
        for name in ["platform", "arch", "EOL", "tmpdir", "homedir", "hostname", "cpus", "uptime", "default"] {
            decl.declare(name)?;
        }
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &rquickjs::module::Exports<'js>) -> Result<()> {
        let platform_fn = Function::new(ctx.clone(), || -> String { platform_name().to_string() })?;
        let arch_fn = Function::new(ctx.clone(), || -> String { arch_name().to_string() })?;
        let tmpdir_fn = Function::new(ctx.clone(), || -> String {
            std::env::temp_dir().display().to_string()
        })?;
        let homedir_fn = Function::new(ctx.clone(), || -> String { homedir() })?;
        let hostname_fn = Function::new(ctx.clone(), || -> String { hostname() })?;
        let cpus_fn = Function::new(ctx.clone(), |ctx: Ctx<'js>| -> Result<Array<'js>> { cpus_impl(&ctx) })?;
        let uptime_fn = Function::new(ctx.clone(), || -> f64 { uptime_secs() })?;

        exports.export("platform", platform_fn.clone())?;
        exports.export("arch", arch_fn.clone())?;
        exports.export("EOL", "\n")?;
        exports.export("tmpdir", tmpdir_fn.clone())?;
        exports.export("homedir", homedir_fn.clone())?;
        exports.export("hostname", hostname_fn.clone())?;
        exports.export("cpus", cpus_fn.clone())?;
        exports.export("uptime", uptime_fn.clone())?;

        let default_obj = Object::new(ctx.clone())?;
        default_obj.set("platform", platform_fn)?;
        default_obj.set("arch", arch_fn)?;
        default_obj.set("EOL", "\n")?;
        default_obj.set("tmpdir", tmpdir_fn)?;
        default_obj.set("homedir", homedir_fn)?;
        default_obj.set("hostname", hostname_fn)?;
        default_obj.set("cpus", cpus_fn)?;
        default_obj.set("uptime", uptime_fn)?;
        exports.export("default", default_obj)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_specifiers_resolve_to_the_os_kind() {
        assert_eq!(crate::resolve_builtin("node:os"), Some(crate::ModuleKind::Os));
        assert_eq!(crate::resolve_builtin("os"), Some(crate::ModuleKind::Os));
        assert!(crate::MODULE_NAMES.contains(&"node:os"));
        assert!(crate::MODULE_NAMES.contains(&"os"));
    }

    #[test]
    fn natives_are_total_and_non_empty() {
        assert!(!platform_name().is_empty());
        assert!(!arch_name().is_empty());
        assert!(!std::env::temp_dir().display().to_string().is_empty());
        assert!(!homedir().is_empty());
        assert!(!hostname().is_empty(), "hostname must never be empty");
        let n = std::thread::available_parallelism().map(|p| p.get()).unwrap_or(1);
        assert!(n >= 1, "cpus length");
        // /proc/uptime is Linux-only; the fallback (0.0) stays finite everywhere else.
        assert!(uptime_secs().is_finite());
    }

    #[test]
    fn module_default_and_named_exports_match_shape() {
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            crate::declare_builtin(&ctx, crate::ModuleKind::Os, "node:os").unwrap();
            let p = rquickjs::Module::evaluate(
                ctx.clone(),
                "os-shape-test.mjs",
                r#"
                    import osMod from "node:os";
                    import { platform, EOL, tmpdir, homedir, hostname, cpus, uptime } from "node:os";
                    globalThis.__OSOK =
                        platform() === osMod.platform() && EOL === "\n" && osMod.EOL === "\n" &&
                        typeof tmpdir() === "string" && tmpdir().length > 0 &&
                        typeof homedir() === "string" && homedir().length > 0 &&
                        typeof hostname() === "string" && hostname().length > 0 &&
                        Array.isArray(cpus()) && cpus().length >= 1 &&
                        typeof cpus()[0].model === "string" && typeof cpus()[0].speed === "number" &&
                        typeof uptime() === "number" && osMod.uptime() === uptime();
                "#,
            )
            .unwrap();
            p.finish::<()>().unwrap();
            let ok: bool = ctx.globals().get("__OSOK").unwrap_or(false);
            assert!(ok, "node:os export shape");
        });
        while let Ok(true) = rt.execute_pending_job() {}
    }
}
