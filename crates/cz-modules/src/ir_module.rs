//! `cardputer:ir` — NEC protocol encoder/decoder plus LIRC raw transmit.
//!
//! The exact NEC timing math lives in [`cz_hal::ir`] (`9000`/`4500` leader, 32 LSB-first bits as
//! `addr`, `~addr`, `cmd`, `~cmd`, `562 µs` bit pulse with `562`/`1687 µs` spaces and a `562 µs`
//! trailing stop pulse = 67 timings ending on a pulse, i.e. Linux `LIRC_MODE_PULSE`). This module
//! renders those frames as a real JS `Uint32Array` and writes little-endian `u32` words to
//! `CARDPUTER_IR_TX_PATH` (default `/dev/lirc0`). TX failures throw a catchable JS `Error` rather
//! than crashing the runtime.

use cz_hal::ir::{self, NEC_TIMINGS};
use rquickjs::{Array, Ctx, Exception, Function, Object, Result, TypedArray, Value};

/// Clamp a JS double into the `0..=255` byte range, masking anything else into one byte.
fn to_byte(v: f64) -> u8 {
    if !v.is_finite() {
        return 0;
    }
    let r = v.round();
    if r < 0.0 {
        ((r as i64) & 0xff) as u8
    } else if r > 255.0 {
        (r as u32 & 0xff) as u8
    } else {
        r as u8
    }
}

fn to_u32_timing(v: f64) -> u32 {
    if !v.is_finite() {
        return 0;
    }
    let r = v.round();
    if r < 0.0 {
        0
    } else if r > u32::MAX as f64 {
        u32::MAX
    } else {
        r as u32
    }
}

/// Extract raw pulse timings from a `Uint32Array` OR a plain JavaScript number Array.
/// Non-numeric inputs throw a catchable JS TypeError (never a panic).
fn pulses_from_value<'js>(ctx: &Ctx<'js>, value: &Value<'js>) -> Result<Vec<u32>> {
    if let Ok(ta) = TypedArray::<u32>::from_value(value.clone()) {
        // SAFETY: the slice is copied out immediately; no JS runs while it is held.
        return Ok(unsafe { ta.as_slice() }.to_vec());
    }
    if let Ok(arr) = Array::from_value(value.clone()) {
        let len = arr.len();
        let mut out = Vec::with_capacity(len);
        for i in 0..len {
            out.push(to_u32_timing(arr.get::<f64>(i).unwrap_or(0.0)));
        }
        return Ok(out);
    }
    Err(Exception::throw_type(
        ctx,
        "expects a Uint32Array or an Array of numbers",
    ))
}

/// Build a frame object (or `null` when the stream was unusable).
fn build_frame<'js>(ctx: &Ctx<'js>, f: Option<ir::NecFrame>) -> Result<Value<'js>> {
    match f {
        None => Ok(Value::new_null(ctx.clone())),
        Some(frame) => {
            let obj = Object::new(ctx.clone())?;
            obj.set("address", u32::from(frame.address))?;
            obj.set("command", u32::from(frame.command))?;
            obj.set("valid", frame.valid)?;
            Ok(obj.into_value())
        }
    }
}

/// `cardputer:ir` module definition (named exports plus a `default` object).
pub struct IrModule;

impl rquickjs::module::ModuleDef for IrModule {
    fn declare<'js>(decl: &rquickjs::module::Declarations<'js>) -> Result<()> {
        for name in ["encodeNec", "decodeNec", "sendRaw", "sendNec", "default"] {
            decl.declare(name)?;
        }
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &rquickjs::module::Exports<'js>) -> Result<()> {
        let encode_fn = Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, address: f64, command: f64| -> Result<TypedArray<'js, u32>> {
                let pulses = ir::encode_nec(to_byte(address), to_byte(command));
                TypedArray::<u32>::new_copy(ctx, &pulses[..])
            },
        )?;

        let decode_fn = Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, pulses: Value<'js>| -> Result<Value<'js>> {
                let raw = pulses_from_value(&ctx, &pulses)?;
                build_frame(&ctx, ir::decode_nec(&raw))
            },
        )?;

        let send_raw_fn = Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, pulses: Value<'js>| -> Result<usize> {
                let raw = pulses_from_value(&ctx, &pulses)?;
                ir::send_raw(&raw).map_err(|e| {
                    Exception::throw_message(&ctx, &format!("ir sendRaw failed: {e}"))
                })
            },
        )?;

        let send_nec_fn = Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, address: f64, command: f64| -> Result<usize> {
                ir::send_nec(to_byte(address), to_byte(command)).map_err(|e| {
                    Exception::throw_message(&ctx, &format!("ir sendNec failed: {e}"))
                })
            },
        )?;

        // Sanity guard the exported frame size is exactly what HAL promises (67 timings).
        debug_assert_eq!(NEC_TIMINGS, 67);

        exports.export("encodeNec", encode_fn.clone())?;
        exports.export("decodeNec", decode_fn.clone())?;
        exports.export("sendRaw", send_raw_fn.clone())?;
        exports.export("sendNec", send_nec_fn.clone())?;

        let default_obj = Object::new(ctx.clone())?;
        default_obj.set("encodeNec", encode_fn)?;
        default_obj.set("decodeNec", decode_fn)?;
        default_obj.set("sendRaw", send_raw_fn)?;
        default_obj.set("sendNec", send_nec_fn)?;
        exports.export("default", default_obj)?;
        Ok(())
    }
}

#[cfg(test)]
mod ir_module_tests {
    use super::*;
    use rquickjs::{Context, Module, Runtime};

    fn env_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    use std::sync::OnceLock;

    fn run_script(source: &str) -> String {
        let rt = Runtime::new().unwrap();
        let ctx = Context::full(&rt).unwrap();
        let out = ctx.with(|ctx| -> String {
            Module::declare_def::<IrModule, _>(ctx.clone(), "cardputer:ir").unwrap();
            let p = Module::evaluate(ctx.clone(), "ir-test.mjs", source).unwrap();
            p.finish::<()>().unwrap();
            ctx.globals().get::<_, String>("__R").unwrap_or_else(|_| "NO_R".into())
        });
        while let Ok(true) = rt.execute_pending_job() {}
        out
    }

    struct TxDir {
        dir: std::path::PathBuf,
        path: std::path::PathBuf,
    }

    impl TxDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("cz-ir-mod-{}-{}", std::process::id(), name));
            std::fs::create_dir_all(&dir).expect("mk tx dir");
            Self {
                dir: dir.clone(),
                path: dir.join("lirc0"),
            }
        }
    }

    impl Drop for TxDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    #[test]
    fn encode_nec_exports_a_67_element_uint32array_with_leader_and_trailer() {
        let out = run_script(
            r#"
            import * as ir from "cardputer:ir";
            import irDef from "cardputer:ir";
            for (const n of ["encodeNec","decodeNec","sendRaw","sendNec"]) {
                if (!(n in ir)) throw new Error("missing named " + n);
                if (!(n in irDef)) throw new Error("missing default " + n);
            }
            const p = ir.encodeNec(0x04, 0x08);
            if (!(p instanceof Uint32Array)) throw new Error("must be Uint32Array, got " + Object.prototype.toString.call(p));
            if (p.length !== 67) throw new Error("length " + p.length);
            if (p[0] !== 9000 || p[1] !== 4500 || p[66] !== 562) throw new Error("shape " + [p[0], p[1], p[66]]);
            // Odd count means the frame ends on a pulse: every even index is a 562us pulse.
            for (let i = 2; i < 66; i += 2) if (p[i] !== 562) throw new Error("bit pulse at " + i);
            if (ir.encodeNec(-1, 300).length !== 67) throw new Error("clamped inputs must still encode");
            globalThis.__R = "IR_ENC_MOD_OK";
            "#,
        );
        assert_eq!(out, "IR_ENC_MOD_OK");
    }

    #[test]
    fn decode_roundtrip_accepts_plain_arrays_and_flags_bad_frames() {
        let out = run_script(
            r#"
            import ir from "cardputer:ir";
            const p = ir.encodeNec(0x04, 0x08);
            const d = ir.decodeNec(p);
            if (!d || d.valid !== true || d.address !== 0x04 || d.command !== 0x08) {
                throw new Error("typed-array roundtrip " + JSON.stringify(d));
            }
            const plain = Array.from(p);
            const d2 = ir.decodeNec(plain);
            if (!d2 || d2.address !== 0x04 || d2.command !== 0x08) throw new Error("plain array roundtrip");
            // Broken complement -> decodes but reports valid === false.
            const bad = Array.from(p);
            bad[2 + 1 * 2 + 1] = bad[2 + 1 * 2 + 1] === 1687 ? 562 : 1687;
            const d3 = ir.decodeNec(bad);
            if (!d3 || d3.valid === true) throw new Error("complement check broken");
            // Junk inputs are rejected as null instead of throwing.
            if (ir.decodeNec([9000]) !== null) throw new Error("short frame must be null");
            if (ir.decodeNec([]) !== null) throw new Error("empty frame must be null");
            if (ir.decodeNec([1, 1]) !== null) throw new Error("bad leader must be null");
            let threw = false;
            try { ir.decodeNec("not-pulses"); } catch (_e) { threw = true; }
            if (!threw) throw new Error("non-numeric input must throw a TypeError");
            globalThis.__R = "IR_DEC_MOD_OK";
            "#,
        );
        assert_eq!(out, "IR_DEC_MOD_OK");
    }

    #[test]
    fn send_nec_and_send_raw_write_little_endian_u32_frames_to_env_path() {
        let _lock = env_test_lock();
        let t = TxDir::new("tx");
        std::env::set_var("CARDPUTER_IR_TX_PATH", &t.path);

        let out = run_script(
            r#"
            import * as ir from "cardputer:ir";
            const n = ir.sendNec(0x12, 0x34);
            if (n !== 67) throw new Error("sendNec count " + n);
            const m = ir.sendRaw(ir.encodeNec(0xAA, 0xBB));
            if (m !== 67) throw new Error("sendRaw count " + m);
            const partial = ir.sendRaw([9000, 4500, 562]);
            if (partial !== 3) throw new Error("partial count " + partial);
            globalThis.__R = "IR_TX_MOD_OK";
            "#,
        );
        assert_eq!(out, "IR_TX_MOD_OK");
        let raw = std::fs::read(&t.path).expect("tx bytes");
        assert_eq!(raw.len(), 3 * 4, "truncate semantics: last send wins");
        assert_eq!(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]), 9000);

        // Rewriting a full frame proves little-endian ordering end-to-end.
        run_script(
            r#"
            import ir from "cardputer:ir";
            if (ir.sendNec(0x04, 0x08) !== 67) throw new Error("reframe");
            globalThis.__R = "IR_REFRAME_OK";
            "#,
        );
        let full = std::fs::read(&t.path).expect("full tx");
        assert_eq!(full.len(), 67 * 4);
        assert_eq!(u32::from_le_bytes([full[0], full[1], full[2], full[3]]), 9000);
        assert_eq!(u32::from_le_bytes([full[4], full[5], full[6], full[7]]), 4500);

        // An unreachable TX device throws a catchable JS Error (no crash, no silent zero).
        std::env::set_var("CARDPUTER_IR_TX_PATH", "/tmp/cz-ir-mod-missing-dir/lirc0");
        let threw = run_script(
            r#"
            import * as ir from "cardputer:ir";
            let seen = "NO_THROW";
            let count = -1;
            try { count = ir.sendNec(0x12, 0x34); } catch (e) { seen = String(e && e.message ? e.message : e); }
            if (count !== -1) throw new Error("missing device must not report success");
            if (!seen.includes("sendNec failed")) throw new Error("error text was " + seen);
            globalThis.__R = "IR_TX_THROW_OK";
            "#,
        );
        assert_eq!(threw, "IR_TX_THROW_OK");
        std::env::remove_var("CARDPUTER_IR_TX_PATH");
    }
}
