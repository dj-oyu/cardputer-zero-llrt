//! Phase 9A: Node-compatible `globalThis.process` + `"node:process"` / `"process"` module.
//!
//! `cz-runner` calls [`install_process`] at startup with the FULL `std::env::args()` vector so
//! script CLI arguments appear in `process.argv` (`[exe, scriptPath, ...scriptArgs]`). The
//! `ProcessModule` definition builds the same shape from `std::env::args()` directly, so plain
//! `import process from "node:process"` works even on embedders that never installed the global.
//!
//! Surface (per spec):
//! * `process.env` — plain JS object populated from `std::env::vars()`.
//! * `process.argv` — `string[]`.
//! * `process.cwd()` — current directory as a UTF-8 string.
//! * `process.exit(code?)` — flushes stdout/stderr and terminates with integer code (default 0).
//! * `process.platform` — `std::env::consts::OS` ("linux" on CardputerZero/Debian).
//! * `process.arch` — `"arm64"` on aarch64, `"x64"` on x86_64, else `std::env::consts::ARCH`.

use rquickjs::{Array, Ctx, Exception, Function, Object, Result, Value};
use rquickjs::function::Opt;

/// Node-style architecture name for the target triple.
pub fn node_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => other,
    }
}

/// `process.env`: plain string-keyed/string-valued object straight from the OS environment.
fn build_env<'js>(ctx: &Ctx<'js>) -> Result<Object<'js>> {
    let obj = Object::new(ctx.clone())?;
    for (key, value) in std::env::vars() {
        obj.set(key.as_str(), value.as_str())?;
    }
    Ok(obj)
}

/// `process.argv`: `[exe, scriptPath, ...scriptArgs]` as passed by the runner.
fn build_argv<'js>(ctx: &Ctx<'js>, argv: &[String]) -> Result<Array<'js>> {
    let arr = Array::new(ctx.clone())?;
    for (i, arg) in argv.iter().enumerate() {
        arr.set(i, arg.as_str())?;
    }
    Ok(arr)
}

fn cwd_impl<'js>(ctx: &Ctx<'js>) -> Result<String> {
    std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .ok_or_else(|| Exception::throw_message(ctx, "could not determine current directory"))
}

/// `process.exit(code?)`: flush both stdio streams, then terminate the process for real
/// (Node semantics — nothing returns from this call).
fn exit_impl(code: Opt<f64>) -> rquickjs::Result<()> {
    use std::io::Write as _;
    std::io::stdout().flush().ok();
    std::io::stderr().flush().ok();
    let code = match code.0 {
        Some(n) if n.is_finite() => n.round() as i32,
        _ => 0,
    };
    std::process::exit(code);
}

/// Build the full `process` object shape shared by the global and the module default export.
pub fn build_process_object<'js>(ctx: &Ctx<'js>, argv: &[String]) -> Result<Object<'js>> {
    let obj = Object::new(ctx.clone())?;
    obj.set("env", build_env(ctx)?)?;
    obj.set("argv", build_argv(ctx, argv)?)?;
    obj.set(
        "cwd",
        Function::new(ctx.clone(), |ctx: Ctx<'js>| -> Result<String> { cwd_impl(&ctx) })?,
    )?;
    obj.set("exit", Function::new(ctx.clone(), exit_impl)?)?;
    obj.set("platform", std::env::consts::OS)?;
    obj.set("arch", node_arch())?;
    Ok(obj)
}

/// Install `globalThis.process` (idempotent: an existing object is left untouched).
pub fn install_process<'js>(ctx: &Ctx<'js>, argv: &[String]) -> Result<()> {
    let existing: rquickjs::Result<Value<'js>> = ctx.globals().get("process");
    if let Ok(v) = existing {
        if v.is_object() {
            return Ok(());
        }
    }
    let obj = build_process_object(ctx, argv)?;
    ctx.globals().set("process", obj)
}

/// `node:process` / `process` module definition: named exports AND a `default` object.
pub struct ProcessModule;

impl rquickjs::module::ModuleDef for ProcessModule {
    fn declare<'js>(decl: &rquickjs::module::Declarations<'js>) -> Result<()> {
        for name in ["env", "argv", "cwd", "exit", "platform", "arch", "default"] {
            decl.declare(name)?;
        }
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &rquickjs::module::Exports<'js>) -> Result<()> {
        let argv: Vec<String> = std::env::args().collect();
        let proc = build_process_object(ctx, &argv)?;

        let env: Value = proc.get("env")?;
        let argv_v: Value = proc.get("argv")?;
        let cwd: Value = proc.get("cwd")?;
        let exit: Value = proc.get("exit")?;
        let platform: Value = proc.get("platform")?;
        let arch: Value = proc.get("arch")?;
        for (name, value) in [
            ("env", env),
            ("argv", argv_v),
            ("cwd", cwd),
            ("exit", exit),
            ("platform", platform),
            ("arch", arch),
        ] {
            exports.export(name, value)?;
        }
        exports.export("default", proc)?;
        Ok(())
    }
}

#[cfg(test)]
mod process_tests {
    use super::*;
    use rquickjs::{Context, Module, Runtime};

    fn run_process_script(source: &str) -> String {
        let rt = Runtime::new().unwrap();
        let ctx = Context::full(&rt).unwrap();
        let out = ctx.with(|ctx| -> String {
            Module::declare_def::<ProcessModule, _>(ctx.clone(), "node:process").unwrap();
            Module::declare_def::<ProcessModule, _>(ctx.clone(), "process").unwrap();
            let p = Module::evaluate(ctx.clone(), "process-test.mjs", source).unwrap();
            p.finish::<()>().unwrap();
            ctx.globals().get::<_, String>("__R").unwrap_or_else(|_| "NO_RESULT".into())
        });
        while let Ok(true) = rt.execute_pending_job() {}
        out
    }

    #[test]
    fn node_arch_maps_target_triples() {
        // The sandbox/device targets are aarch64 (arm64) and x86_64 (x64); the fallback returns
        // std::env::consts::ARCH verbatim. Assert the mapping is self-consistent with the host.
        let arch = node_arch();
        let base = std::env::consts::ARCH;
        assert!(
            arch == base || arch == "arm64" || arch == "x64",
            "unexpected arch mapping {base}->{arch}"
        );
        if base == "aarch64" {
            assert_eq!(arch, "arm64");
        } else if base == "x86_64" {
            assert_eq!(arch, "x64");
        }
    }

    #[test]
    fn process_module_named_and_default_exports_shape() {
        let r = run_process_script(
            r#"
            import proc from "node:process";
            import * as ns from "process";
            if (typeof proc.platform !== "string" || proc.platform.length === 0) throw new Error("platform");
            if (typeof proc.arch !== "string") throw new Error("arch");
            if (typeof proc.cwd !== "function" || typeof proc.exit !== "function") throw new Error("fns");
            if (!Array.isArray(proc.argv) || proc.argv.length < 1) throw new Error("argv");
            if (typeof proc.argv[0] !== "string") throw new Error("argv[0] must be a string");
            if (typeof proc.env !== "object" || proc.env === null) throw new Error("env object");
            for (const n of ["env", "argv", "cwd", "exit", "platform", "arch"]) {
                if (!(n in ns)) throw new Error("named missing " + n);
            }
            if (proc.platform !== ns.platform || proc.arch !== ns.arch) throw new Error("alias mismatch");
            globalThis.__R = "PROCESS_SHAPE_OK";
        "#,
        );
        assert_eq!(r, "PROCESS_SHAPE_OK");
    }

    #[test]
    fn install_process_global_exposes_env_argv_and_cwd() {
        // Serialize against other env-touching tests (process-wide environment).
        let _lock = crate::module_tests::test_lock();
        std::env::set_var("CZ_P9A_PROCESS_PROBE", "edge-cloud");
        let rt = Runtime::new().unwrap();
        let ctx = Context::full(&rt).unwrap();
        let out = ctx.with(|ctx| -> String {
            let argv = vec![
                "/usr/bin/cz-js".to_string(),
                "/app/gateway.mjs".to_string(),
                "--flag9a".to_string(),
                "extra".to_string(),
            ];
            install_process(&ctx, &argv).unwrap();
            // Idempotency: stamp a sentinel member on the installed object; a second install
            // must leave that exact object in place (no replacement, no throw).
            ctx.eval::<(), _>("globalThis.process.__czSentinel = 7;").unwrap();
            install_process(&ctx, &["replaced.exe".to_string()]).unwrap();
            ctx.eval::<String, _>(
                r#"(function () {
                    const envOk = process.env.CZ_P9A_PROCESS_PROBE === "edge-cloud";
                    const argvOk = process.argv.length === 4 && process.argv[1] === "/app/gateway.mjs"
                        && process.argv.includes("--flag9a") && process.argv[0] === "/usr/bin/cz-js";
                    const cwdOk = typeof process.cwd() === "string" && process.cwd().length > 0;
                    const platOk = process.platform === stdOS && process.arch === stdArch;
                    const exitFn = typeof process.exit === "function";
                    return "ENV=" + envOk + " ARGV=" + argvOk + " CWD=" + cwdOk + " EXIT=" + exitFn + " SAME=" + (process.__czSentinel === 7);
                })();"#
                .replace("stdOS", &format!("{:?}", std::env::consts::OS))
                .replace("stdArch", &format!("{:?}", node_arch())),
            )
            .unwrap()
        });
        while let Ok(true) = rt.execute_pending_job() {}
        assert_eq!(out, "ENV=true ARGV=true CWD=true EXIT=true SAME=true");
        std::env::remove_var("CZ_P9A_PROCESS_PROBE");
    }

    #[test]
    fn module_env_matches_process_env_for_probed_var() {
        let _lock = crate::module_tests::test_lock();
        std::env::set_var("CZ_P9A_MOD_ENV", "sigv4");
        let r = run_process_script(
            r#"
            import proc from "node:process";
            if (proc.env.CZ_P9A_MOD_ENV !== "sigv4") throw new Error("module env missing probe var");
            if (typeof proc.env.PATH === "string" || Object.keys(proc.env).length > 0) {}
            else throw new Error("env empty");
            globalThis.__R = "MOD_ENV_OK";
        "#,
        );
        assert_eq!(r, "MOD_ENV_OK");
        std::env::remove_var("CZ_P9A_MOD_ENV");
    }

    #[test]
    fn process_exit_is_a_function_never_invoked_in_tests() {
        // Invoking exit() would terminate the whole test binary; guard semantics only:
        // the property exists, is callable, and lives on both the global and module shapes.
        let rt = Runtime::new().unwrap();
        let ctx = Context::full(&rt).unwrap();
        let out = ctx.with(|ctx| -> String {
            install_process(&ctx, &["cz-js".to_string(), "t.mjs".to_string()]).unwrap();
            Module::declare_def::<ProcessModule, _>(ctx.clone(), "node:process").unwrap();
            let p = Module::evaluate(
                ctx.clone(),
                "exit-shape.mjs",
                r#"import p from "node:process";
                   globalThis.__K = typeof p.exit === "function" && typeof globalThis.process.exit === "function";"#,
            )
            .unwrap();
            p.finish::<()>().unwrap();
            let ok: bool = ctx.globals().get("__K").unwrap_or(false);
            if ok { "EXIT_SHAPE_OK" } else { "NO" }.to_string()
        });
        while let Ok(true) = rt.execute_pending_job() {}
        assert_eq!(out, "EXIT_SHAPE_OK");
    }

    #[test]
    fn specifiers_resolve_and_declare_through_builtin_registry() {
        let rt = Runtime::new().unwrap();
        let ctx = Context::full(&rt).unwrap();
        ctx.with(|c| {
            assert_eq!(
                crate::resolve_builtin("node:process"),
                Some(crate::ModuleKind::Process)
            );
            assert_eq!(crate::resolve_builtin("process"), Some(crate::ModuleKind::Process));
            assert_eq!(
                crate::resolve_builtin("node:crypto"),
                Some(crate::ModuleKind::Crypto)
            );
            assert_eq!(crate::resolve_builtin("crypto"), Some(crate::ModuleKind::Crypto));
            for spec in ["node:crypto", "crypto", "node:process", "process"] {
                let kind = crate::resolve_builtin(spec).unwrap();
                crate::declare_builtin(&c, kind, spec).unwrap();
                assert!(
                    crate::MODULE_NAMES.contains(&spec),
                    "MODULE_NAMES missing {spec}"
                );
            }
        });
        while let Ok(true) = rt.execute_pending_job() {}
    }
}
