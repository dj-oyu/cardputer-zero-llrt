//! `cz-js` — Cardputer Zero LLRT CLI runner for plain ES modules (no bundler, no
//! `node_modules`). Supports `cardputer:*` builtin modules plus relative file imports
//! (`./hud.mjs`) directly from disk.

use std::io::Write;
use std::path::PathBuf;

use rquickjs::loader::{FileResolver, Loader, Resolver, ScriptLoader};
use rquickjs::{CatchResultExt, CaughtError, Context, Ctx, Error, Function, Module, Runtime};

const VERSION: &str = concat!("cardputer-zero-llrt ", env!("CARGO_PKG_VERSION"));

struct CzResolver {
    file: FileResolver,
}

impl CzResolver {
    fn new() -> Self {
        let mut file = FileResolver::default();
        file.add_path(".");
        // Accept both .js and .mjs script files on-device.
        for pattern in ["{}.js", "{}.mjs"] {
            file.add_pattern(pattern);
        }
        Self { file }
    }
}

impl Resolver for CzResolver {
    fn resolve<'js>(
        &mut self,
        _ctx: &Ctx<'js>,
        base: &str,
        name: &str,
        _attributes: Option<rquickjs::loader::ImportAttributes<'js>>,
    ) -> rquickjs::Result<String> {
        if name.starts_with("cardputer:") {
            return Ok(name.to_string());
        }
        if name.starts_with('.') {
            // Relative ESM import resolved against the importing script's directory using
            // std paths (works with absolute module names on-device).
            let base_path = std::path::Path::new(base);
            let dir = match base_path.parent() {
                Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
                _ => return Err(Error::new_resolving(base, name)),
            };
            let joined = dir.join(name);
            let normalized = normalize_path(&joined);
            if normalized.is_file() {
                return Ok(normalized.display().to_string());
            }
            // Try implicit extensions (.mjs/.js) like Node ESM-lite on-device.
            for ext in ["mjs", "js"] {
                let with_ext = normalized.with_extension(ext);
                if with_ext.is_file() {
                    return Ok(with_ext.display().to_string());
                }
            }
            return Err(Error::new_resolving(base, name));
        }
        // Bare specifiers: delegate to the file resolver search paths.
        self.file.resolve(_ctx, base, name, _attributes)
    }
}

/// Lexically normalize `..`/`.` components without touching the filesystem.
fn normalize_path(path: &std::path::Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                if !out.pop() {
                    out.push(Component::ParentDir.as_os_str());
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

struct CzLoader {
    script: ScriptLoader,
}

impl CzLoader {
    fn new() -> Self {
        let mut script = ScriptLoader::default();
        script.add_extension("mjs");
        script.add_extension("js");
        Self { script }
    }
}

impl Loader for CzLoader {
    fn load<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        name: &str,
        attributes: Option<rquickjs::loader::ImportAttributes<'js>>,
    ) -> rquickjs::Result<Module<'js, rquickjs::module::Declared>> {
        if let Some(idx) = cz_modules::MODULE_NAMES.iter().position(|m| *m == name) {
            let module = match idx {
                0 => Module::declare_def::<cz_modules::DisplayModule, _>(ctx.clone(), name)?,
                1 => Module::declare_def::<cz_modules::InputModule, _>(ctx.clone(), name)?,
                2 => Module::declare_def::<cz_modules::AppModule, _>(ctx.clone(), name)?,
                3 => Module::declare_def::<cz_modules::PowerModule, _>(ctx.clone(), name)?,
                _ => return Err(Error::new_loading(name)),
            };
            return Ok(module);
        }
        self.script.load(ctx, name, attributes)
    }
}

/// Bound Rust printer used by the JS `console` shim. Level 0/1 write to stdout, level >= 2
/// writes to stderr. All values are pre-stringified by the JS shim.
fn bind_console(ctx: &Ctx<'_>) -> rquickjs::Result<()> {
    let printer = Function::new(
        ctx.clone(),
        |level: i32, parts: Vec<String>| -> rquickjs::Result<()> {
            let line = parts.join(" ");
            if level >= 2 {
                std::io::stderr().write_all(line.as_bytes())?;
                std::io::stderr().write_all(b"\n")?;
            } else {
                std::io::stdout().write_all(line.as_bytes())?;
                std::io::stdout().write_all(b"\n")?;
            }
            Ok(())
        },
    )?;
    ctx.globals().set("__cz_print", printer)?;

    // console shim with LLRT-style stringification semantics.
    let shim = r#"
        (function () {
            function fmt(v) {
                if (typeof v === "string") return v;
                if (v === null) return "null";
                if (v === undefined) return "undefined";
                if (typeof v === "bigint") return String(v) + "n";
                if (typeof v === "object" || typeof v === "symbol") {
                    try { const j = JSON.stringify(v); return j === undefined ? String(v) : j; }
                    catch (_e) { return String(v); }
                }
                return String(v);
            }
            globalThis.console = {
                log:   (...a) => __cz_print(0, a.map(fmt)),
                warn:  (...a) => __cz_print(1, a.map(fmt)),
                error: (...a) => __cz_print(2, a.map(fmt)),
            };
        })();
    "#;
    ctx.eval::<(), _>(shim)?;
    Ok(())
}

/// Evaluate one ES module source under `name` (must be called inside a context scope).
fn eval_module(ctx: &Ctx<'_>, name: &str, source: &str) -> Result<(), String> {
    let promise = match Module::evaluate(ctx.clone(), name, source).catch(ctx) {
        Ok(p) => p,
        Err(caught) => return Err(render_caught(ctx, &caught)),
    };
    match promise.finish::<()>().catch(ctx) {
        Ok(()) => {}
        Err(caught) => return Err(render_caught(ctx, &caught)),
    }
    Ok(())
}

/// Drain pending QuickJS jobs. Call only when no context scope is active.
fn drain_jobs(rt: &Runtime) {
    while let Ok(true) = rt.execute_pending_job() {}
}

/// Render a caught JS/Rust error with message and stack where available.
fn render_caught(ctx: &Ctx<'_>, caught: &CaughtError<'_>) -> String {
    let _ = ctx;
    let text = format!("{caught}");
    if let CaughtError::Exception(ex) = caught {
        let msg = ex.message().unwrap_or_else(|| "unknown exception".into());
        let stack = ex.stack().unwrap_or_default();
        if stack.is_empty() || stack.contains(&msg) {
            return msg;
        }
        return format!("{msg}\n{stack}");
    }
    if text.trim().is_empty() {
        return "unknown script error".into();
    }
    text
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 2 && args[1] == "--version" {
        println!("{VERSION}");
        return;
    }

    let (name, source) = if args.len() >= 3 && args[1] == "-e" {
        ("-e".to_string(), args[2].clone())
    } else if args.len() >= 2 && !args[1].starts_with('-') {
        let path = PathBuf::from(&args[1]);
        let abs = path.canonicalize().unwrap_or(path.clone());
        let text = match std::fs::read_to_string(&abs) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("cz-js: cannot read {}: {e}", abs.display());
                std::process::exit(1);
            }
        };
        (abs.display().to_string(), text)
    } else {
        eprintln!("usage: cz-js [--version] [-e <code> | <script.mjs>]");
        std::process::exit(1);
    };

    let rt = Runtime::new().expect("runtime");
    rt.set_loader(CzResolver::new(), CzLoader::new());
    let ctx = Context::full(&rt).expect("context");

    let run_result = ctx.with(|ctx| -> Result<(), String> {
        bind_console(&ctx).map_err(|e| format!("{e}"))?;
        eval_module(&ctx, &name, &source)
    });
    // Drain any remaining QuickJS jobs outside the context scope.
    drain_jobs(&rt);

    match run_result {
        Ok(()) => std::process::exit(0),
        Err(msg) => {
            eprintln!("cz-js: {msg}");
            std::process::exit(1);
        }
    }
}


#[cfg(test)]
mod runner_tests {
    use super::*;

    #[test]
    fn version_string_shape() {
        assert!(VERSION.starts_with("cardputer-zero-llrt "));
        assert!(VERSION.contains("0.1.0"));
    }

    #[test]
    fn console_and_module_eval_end_to_end() {
        let rt = Runtime::new().unwrap();
        rt.set_loader(CzResolver::new(), CzLoader::new());
        let ctx = Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            bind_console(&ctx).unwrap();
            let src = r#"
                import * as app from "cardputer:app";
                const info = app.info();
                console.log("RUNNER_APP_OK", info.device === "M5Stack CardputerZero", 42, null);
                globalThis.__ok = info.runtime === "cardputer-zero-llrt";
            "#;
            eval_module(&ctx, "inline.mjs", src).expect("eval should pass");
            let ok: bool = ctx.globals().get("__ok").unwrap();
            assert!(ok);
        });
    }

    #[test]
    fn thrown_error_propagates_as_string() {
        let rt = Runtime::new().unwrap();
        rt.set_loader(CzResolver::new(), CzLoader::new());
        let ctx = Context::full(&rt).unwrap();
        let res = ctx.with(|ctx| {
            bind_console(&ctx).unwrap();
            eval_module(&ctx, "boom.mjs", r#"throw new Error("runner_boom_test");"#)
        });
        drain_jobs(&rt);
        let err = res.expect_err("throwing module must produce Err");
        assert!(err.contains("runner_boom_test"), "error text was: {err}");
    }

    #[test]
    fn relative_mjs_imports_resolve_from_script_dir() {
        let dir = std::env::temp_dir().join(format!("cz-runner-rel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let lib = dir.join("lib.mjs");
        std::fs::write(&lib, "export const twice = (x) => x * 2;\n").expect("write lib");
        let main = dir.join("main.mjs");
        std::fs::write(
            &main,
            "import { twice } from \"./lib.mjs\";\nglobalThis.__ok = twice(21) === 42;\n",
        )
        .expect("write main");

        let rt = Runtime::new().unwrap();
        rt.set_loader(CzResolver::new(), CzLoader::new());
        let ctx = Context::full(&rt).unwrap();
        let abs = main.canonicalize().unwrap();
        ctx.with(|ctx| {
            let src = std::fs::read_to_string(&abs).unwrap();
            eval_module(&ctx, &abs.display().to_string(), &src).expect("relative import eval");
            let ok: bool = ctx.globals().get("__ok").unwrap();
            assert!(ok, "relative ./lib.mjs import must work");
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unresolved_import_produces_error_text() {
        let rt = Runtime::new().unwrap();
        rt.set_loader(CzResolver::new(), CzLoader::new());
        let ctx = Context::full(&rt).unwrap();
        let res = ctx.with(|ctx| {
            bind_console(&ctx).unwrap();
            eval_module(
                &ctx,
                "miss.mjs",
                "import { nope } from \"./definitely-missing-module.mjs\";\nglobalThis.x = nope;\n",
            )
        });
        drain_jobs(&rt);
        let err = res.expect_err("missing module import must error");
        assert!(err.contains("definitely-missing-module"), "err was: {err}");
    }

    // Compile-time usage guard for the console value plumbing types.
    const _CONSOLE_TYPES_OK: usize = std::mem::size_of::<rquickjs::Array<'static>>();
}
