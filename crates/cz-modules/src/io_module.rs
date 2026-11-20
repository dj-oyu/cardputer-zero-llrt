//! `cardputer:io` — safe port discovery plus guarded I2C / UART helpers.
//!
//! [`cz_hal::io`] scans the resolved device directory (`CARDPUTER_DEV_DIR`, default `/dev`) for
//! `i2c-*`, `spidev*`, `gpiochip*` and `ttyS*`/`ttyAMA*`/`serial*` entries, and reserves I2C
//! address `0x55` (the onboard BQ27220 power gauge — writing it can brick the fuel meter).
//! UART reads/writes map real `io::Error`s to catchable JS `Error` objects; every returned buffer
//! is a copied `Uint8Array`.

use std::path::Path;

use cz_hal::io::{self, MAX_UART_READ};
use rquickjs::{Array, Ctx, Exception, Function, Object, Result, TypedArray, Value};

use crate::bytes_from_value;

/// Clamp a JS double into a non-negative integer byte count.
fn to_usize(v: f64) -> usize {
    if !v.is_finite() || v < 0.0 {
        return 0;
    }
    let r = v.round();
    if r > i64::MAX as f64 {
        usize::MAX
    } else {
        r as usize
    }
}

fn to_i64_addr(v: f64) -> i64 {
    if !v.is_finite() {
        return -1;
    }
    let r = v.round();
    if r < i64::MIN as f64 {
        i64::MIN
    } else if r > i64::MAX as f64 {
        i64::MAX
    } else {
        r as i64
    }
}

fn str_array<'js>(ctx: &Ctx<'js>, items: &[String]) -> Result<Array<'js>> {
    let arr = Array::new(ctx.clone())?;
    for (i, s) in items.iter().enumerate() {
        arr.set(i, s.as_str())?;
    }
    Ok(arr)
}

fn build_ports<'js>(ctx: &Ctx<'js>, p: &io::Ports) -> Result<Object<'js>> {
    let obj = Object::new(ctx.clone())?;
    obj.set("i2c", str_array(ctx, &p.i2c)?)?;
    obj.set("spi", str_array(ctx, &p.spi)?)?;
    obj.set("gpio", str_array(ctx, &p.gpio)?)?;
    obj.set("uart", str_array(ctx, &p.uart)?)?;
    Ok(obj)
}

/// Accept either a string or a `Uint8Array` payload for UART writes.
fn data_bytes<'js>(value: &Value<'js>) -> Result<Vec<u8>> {
    if let Some(s) = value.as_string() {
        let text = s.to_string().unwrap_or_default();
        return Ok(text.into_bytes());
    }
    bytes_from_value(value)
}

/// `cardputer:io` module definition (named exports plus a `default` object).
pub struct IoModule;

impl rquickjs::module::ModuleDef for IoModule {
    fn declare<'js>(decl: &rquickjs::module::Declarations<'js>) -> Result<()> {
        for name in ["listPorts", "isReservedI2cAddr", "uartWrite", "uartRead", "default"] {
            decl.declare(name)?;
        }
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &rquickjs::module::Exports<'js>) -> Result<()> {
        let list_fn = Function::new(ctx.clone(), |ctx: Ctx<'js>| -> Result<Object<'js>> {
            build_ports(&ctx, &io::list_ports())
        })?;

        let reserved_fn = Function::new(ctx.clone(), |addr: f64| -> bool {
            io::is_reserved_i2c_addr(to_i64_addr(addr))
        })?;

        let write_fn = Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, device_path: String, data: Value<'js>| -> Result<usize> {
                let bytes = data_bytes(&data)?;
                io::uart_write(Path::new(&device_path), &bytes).map_err(|e| {
                    Exception::throw_message(
                        &ctx,
                        &format!("uartWrite({device_path}) failed: {}", describe_io(&e)),
                    )
                })
            },
        )?;

        let read_fn = Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, device_path: String, max_bytes: f64| -> Result<Value<'js>> {
                // NaN sentinel from the shim means "read up to the HAL cap".
                let cap = if max_bytes.is_finite() {
                    to_usize(max_bytes)
                } else {
                    MAX_UART_READ
                };
                let bytes = io::uart_read(Path::new(&device_path), cap).map_err(|e| {
                    Exception::throw_message(
                        &ctx,
                        &format!("uartRead({device_path}) failed: {}", describe_io(&e)),
                    )
                })?;
                let ta = TypedArray::<u8>::new_copy(ctx, &bytes[..])?;
                Ok(ta.into_value())
            },
        )?;

        // Optional maxBytes must arrive as a required scalar: normalize undefined/null here.
        let shim = r#"
            (function () {
                if (globalThis.__czIoInstalled) return;
                globalThis.__czIoUartRead = function (path, maxBytes) {
                    const n = (typeof maxBytes === "number" && isFinite(maxBytes)) ? maxBytes : NaN;
                    return __czIoUartReadNative(typeof path === "string" ? path : String(path), n);
                };
                globalThis.__czIoUartWrite = function (path, data) {
                    return __czIoUartWriteNative(typeof path === "string" ? path : String(path), data);
                };
                globalThis.__czIoInstalled = true;
            })();
        "#;
        ctx.eval::<(), _>(shim)?;
        ctx.globals().set("__czIoUartReadNative", read_fn.clone())?;
        ctx.globals().set("__czIoUartWriteNative", write_fn.clone())?;

        let js_read: Function<'js> = ctx.globals().get("__czIoUartRead")?;
        let js_write: Function<'js> = ctx.globals().get("__czIoUartWrite")?;

        exports.export("listPorts", list_fn.clone())?;
        exports.export("isReservedI2cAddr", reserved_fn.clone())?;
        exports.export("uartWrite", js_write.clone())?;
        exports.export("uartRead", js_read.clone())?;

        let default_obj = Object::new(ctx.clone())?;
        default_obj.set("listPorts", list_fn)?;
        default_obj.set("isReservedI2cAddr", reserved_fn)?;
        default_obj.set("uartWrite", js_write)?;
        default_obj.set("uartRead", js_read)?;
        exports.export("default", default_obj)?;
        Ok(())
    }
}

/// Node-style error rendering (keeps the `ENOENT`-style code visible for debugging).
fn describe_io(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => format!("ENOENT: {}", e),
        std::io::ErrorKind::PermissionDenied => format!("EACCES: {}", e),
        std::io::ErrorKind::IsADirectory => format!("EISDIR: {}", e),
        other => format!("{other}: {e}"),
    }
}

#[cfg(test)]
mod io_module_tests {
    use super::*;
    use rquickjs::{CatchResultExt, CaughtError, Context, Module, Runtime};

    fn env_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    use std::sync::OnceLock;

    /// Render a caught JS/Rust error the same way `cz-js` does (message plus stack).
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
            return "unknown script error".into()
        }
        text
    }

    /// Evaluate a script and return its `__R` tag. A JavaScript throw is surfaced as
    /// `THROWN:<detail>` so assertions can distinguish real failures from wrong expectations.
    fn run_script(source: &str) -> String {
        let rt = Runtime::new().unwrap();
        let ctx = Context::full(&rt).unwrap();
        let out = ctx.with(|ctx| -> Result<String> {
            Module::declare_def::<IoModule, _>(ctx.clone(), "cardputer:io")?;
            let rendered = match Module::evaluate(ctx.clone(), "io-test.mjs", source).catch(&ctx) {
                Ok(p) => match p.finish::<()>().catch(&ctx) {
                    Ok(()) => None,
                    Err(e) => Some(render_caught(&ctx, &e)),
                },
                Err(e) => Some(render_caught(&ctx, &e)),
            };
            if let Some(detail) = rendered {
                return Ok(format!("THROWN:{detail}"));
            }
            ctx.globals().get::<_, String>("__R").or_else(|_| Ok("NO_R".to_string()))
        })
        .expect("runtime error");
        while let Ok(true) = rt.execute_pending_job() {}
        if out.starts_with("THROWN:") {
            panic!("io module script threw: {out}");
        }
        out
    }

    struct DevDir {
        path: std::path::PathBuf,
    }

    impl DevDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("cz-io-mod-{}-{}", std::process::id(), name));
            std::fs::create_dir_all(&path).expect("mk dev dir");
            Self { path }
        }
        fn touch(&self, names: &[&str]) {
            for n in names {
                std::fs::write(self.path.join(n), "").expect("touch");
            }
        }
    }

    impl Drop for DevDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.path).ok();
        }
    }

    #[test]
    fn list_ports_shape_and_reserved_guard_via_module() {
        let _lock = env_test_lock();
        let d = DevDir::new("ports");
        d.touch(&["i2c-0", "i2c-1", "spidev0.1", "gpiochip0", "gpiochip1", "ttyS0", "serial0"]);
        std::env::set_var("CARDPUTER_DEV_DIR", &d.path);

        let out = run_script(
            r#"
            import * as io from "cardputer:io";
            import ioDef from "cardputer:io";
            for (const n of ["listPorts","isReservedI2cAddr","uartWrite","uartRead"]) {
                if (!(n in io)) throw new Error("missing named " + n);
                if (!(n in ioDef)) throw new Error("missing default " + n);
            }
            const p = io.listPorts();
            if (p.i2c.length !== 2 || p.spi.length !== 1 || p.gpio.length !== 2 || p.uart.length !== 2) {
                throw new Error("ports " + JSON.stringify(p));
            }
            if (JSON.stringify(p.i2c) !== JSON.stringify(["i2c-0","i2c-1"])) throw new Error("i2c order");
            if (io.isReservedI2cAddr(0x55) !== true) throw new Error("BQ27220 must be reserved");
            if (io.isReservedI2cAddr(85) !== true) throw new Error("decimal 85 must be reserved");
            if (io.isReservedI2cAddr(0x3c) !== false) throw new Error("0x3c must be free");
            if (io.isReservedI2cAddr(NaN) !== false) throw new Error("NaN addr must be free");
            globalThis.__R = "IO_PORTS_MOD_OK";
            "#,
        );
        assert_eq!(out, "IO_PORTS_MOD_OK");

        // Missing dev dir must still return empty arrays rather than throwing.
        std::env::set_var("CARDPUTER_DEV_DIR", "/tmp/cz-io-mod-no-such-dir");
        let empty = run_script(
            r#"
            import io from "cardputer:io";
            const p = io.listPorts();
            if (p.i2c.length !== 0 || p.uart.length !== 0) throw new Error("empty lists expected");
            globalThis.__R = "IO_EMPTY_MOD_OK";
            "#,
        );
        assert_eq!(empty, "IO_EMPTY_MOD_OK");
        std::env::remove_var("CARDPUTER_DEV_DIR");
    }

    #[test]
    fn uart_write_and_read_roundtrip_strings_and_bytes_with_exact_counts() {
        let d = DevDir::new("uart");
        let node = d.path.join("ttyS0");
        std::fs::write(&node, "").expect("seed uart");

        let out = run_script(
            &format!(
                r#"
                import * as io from "cardputer:io";
                const n = io.uartWrite({:?}, "AT+OK\r\n");
                if (n !== 7) throw new Error("write len " + n);
                const back = io.uartRead({:?}, 64);
                if (!(back instanceof Uint8Array)) throw new Error("must be Uint8Array");
                if (back.length !== 7 || back[0] !== 0x41) throw new Error("read mismatch " + back.length);
                // Byte payloads round-trip too. UART writes are NOT truncating (a real tty has no
                // truncate notion): a shorter rewrite patches the head and keeps the old tail.
                const b2 = io.uartWrite({:?}, new Uint8Array([1, 2, 3, 254]));
                if (b2 !== 4) throw new Error("byte write len " + b2);
                const all = io.uartRead({:?}); // omitted maxBytes -> read up to the 64 KiB cap
                if (all.length !== 7) throw new Error("non-truncating read len " + all.length);
                if (all[0] !== 1 || all[3] !== 254) throw new Error("head patch mismatch");
                if (io.uartRead({:?}, 2).length !== 2) throw new Error("partial read");
                globalThis.__R = "IO_UART_MOD_OK";
                "#,
                node.display().to_string(),
                node.display().to_string(),
                node.display().to_string(),
                node.display().to_string(),
                node.display().to_string()
            ),
        );
        assert_eq!(out, "IO_UART_MOD_OK");
    }

    #[test]
    fn uart_read_caps_at_64k_and_io_errors_surface_as_catchable_js_errors() {
        let d = DevDir::new("cap");
        let big = d.path.join("ttyS1");
        std::fs::write(&big, vec![0xAB_u8; 70_000]).expect("seed big fixture");

        let capped = run_script(
            &format!(
                r#"
                import * as io from "cardputer:io";
                const got = io.uartRead({:?}, 999999);
                if (!(got instanceof Uint8Array)) throw new Error("must be Uint8Array");
                if (got.length !== 65536) throw new Error("cap broken: " + got.length);
                if (got[0] !== 0xAB) throw new Error("payload mismatch");
                globalThis.__R = "IO_CAP_MOD_OK";
                "#,
                big.display().to_string()
            ),
        );
        assert_eq!(capped, "IO_CAP_MOD_OK");

        // Reading a device that cannot be opened throws a real JS Error (catchable, no crash).
        let missing = d.path.join("no-dir").join("ttyS9");
        let err_check = run_script(
            &format!(
                r#"
                import * as io from "cardputer:io";
                let msg = "NO_THROW";
                try {{ io.uartRead({:?}, 32); }} catch (e) {{
                    msg = String(e && e.message ? e.message : e);
                }}
                if (!msg.includes("uartRead")) throw new Error("error text was " + msg);
                if (msg === "NO_THROW") throw new Error("missing device must throw");
                globalThis.__R = "IO_ERR_MOD_OK";
                "#,
                missing.display().to_string()
            ),
        );
        assert_eq!(err_check, "IO_ERR_MOD_OK");
    }
}
