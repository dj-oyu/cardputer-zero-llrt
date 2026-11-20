//! `cardputer:imu` — BMI270 accelerometer/gyroscope + BMM150 magnetometer (Linux IIO sysfs).
//!
//! The heavy lifting lives in [`cz_hal::imu`]; this module only renders its snapshot into plain
//! JS descriptor objects (named exports AND a `default` object). Missing hardware degrades to
//! `{ available: false, accel: null, gyro: null, mag: null }` — importing the module never touches
//! the IIO root unless `devices()`/`read()` are called, and nothing ever panics.

use cz_hal::imu::{self, Vec3};
use rquickjs::{Array, Ctx, Function, Object, Result, Value};

fn build_vec3<'js>(ctx: &Ctx<'js>, v: &Vec3) -> Result<Object<'js>> {
    let obj = Object::new(ctx.clone())?;
    obj.set("x", v.x)?;
    obj.set("y", v.y)?;
    obj.set("z", v.z)?;
    Ok(obj)
}

fn opt_vec3<'js>(ctx: &Ctx<'js>, v: Option<Vec3>) -> Result<Value<'js>> {
    match v {
        Some(inner) => Ok(build_vec3(ctx, &inner)?.into_value()),
        None => Ok(Value::new_null(ctx.clone())),
    }
}

fn build_reading<'js>(ctx: &Ctx<'js>, r: &imu::ImuReading) -> Result<Object<'js>> {
    let obj = Object::new(ctx.clone())?;
    obj.set("available", r.available)?;
    obj.set("accel", opt_vec3(ctx, r.accel.clone())?)?;
    obj.set("gyro", opt_vec3(ctx, r.gyro.clone())?)?;
    obj.set("mag", opt_vec3(ctx, r.mag.clone())?)?;
    Ok(obj)
}

/// `cardputer:imu` module definition (named exports plus a `default` object).
pub struct ImuModule;

impl rquickjs::module::ModuleDef for ImuModule {
    fn declare<'js>(decl: &rquickjs::module::Declarations<'js>) -> Result<()> {
        for name in ["devices", "read", "default"] {
            decl.declare(name)?;
        }
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &rquickjs::module::Exports<'js>) -> Result<()> {
        let devices_fn = Function::new(ctx.clone(), |ctx: Ctx<'js>| -> Result<Array<'js>> {
            let names = imu::devices();
            let arr = Array::new(ctx.clone())?;
            for (i, n) in names.iter().enumerate() {
                arr.set(i, n.as_str())?;
            }
            Ok(arr)
        })?;

        let read_fn = Function::new(ctx.clone(), |ctx: Ctx<'js>| -> Result<Object<'js>> {
            build_reading(&ctx, &imu::read())
        })?;

        exports.export("devices", devices_fn.clone())?;
        exports.export("read", read_fn.clone())?;

        let default_obj = Object::new(ctx.clone())?;
        default_obj.set("devices", devices_fn)?;
        default_obj.set("read", read_fn)?;
        exports.export("default", default_obj)?;
        Ok(())
    }
}

#[cfg(test)]
mod imu_module_tests {
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
            Module::declare_def::<ImuModule, _>(ctx.clone(), "cardputer:imu").unwrap();
            let p = Module::evaluate(ctx.clone(), "imu-test.mjs", source).unwrap();
            p.finish::<()>().unwrap();
            ctx.globals().get::<_, String>("__R").unwrap_or_else(|_| "NO_R".into())
        });
        while let Ok(true) = rt.execute_pending_job() {}
        out
    }

    struct TempIio {
        path: std::path::PathBuf,
    }

    impl TempIio {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("cz-imu-mod-{}-{}", std::process::id(), name));
            fs_mkdir(&path);
            Self { path }
        }
        fn device(&self, idx: usize, name: &str, attrs: &[(&str, &str)]) {
            let d = path_join(&self.path, &format!("iio:device{idx}"));
            fs_mkdir(&d);
            fs_write(&path_join(&d, "name"), format!("{name}\n"));
            for (f, v) in attrs {
                fs_write(&path_join(&d, f), format!("{v}\n"));
            }
        }
    }

    fn fs_mkdir(p: &std::path::Path) {
        std::fs::create_dir_all(p).expect("mkdir");
    }
    fn fs_write(p: &std::path::Path, s: String) {
        std::fs::write(p, s).expect("write");
    }
    fn path_join(base: &std::path::Path, child: &str) -> std::path::PathBuf {
        base.join(child)
    }

    impl Drop for TempIio {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.path).ok();
        }
    }

    #[test]
    fn imu_devices_and_scaled_read_via_js_module() {
        let _lock = env_test_lock();
        let t = TempIio::new("js-read");
        t.device(0, "m5ioe1", &[]);
        t.device(1, "bmi270", &[
            ("in_accel_x_raw", "1000"),
            ("in_accel_y_raw", "-500"),
            ("in_accel_z_raw", "16384"),
            ("in_accel_scale", "0.00059855"),
            ("in_anglvel_x_raw", "100"),
            ("in_anglvel_y_raw", "-200"),
            ("in_anglvel_z_raw", "0"),
            ("in_anglvel_scale", "0.001065"),
        ]);
        t.device(2, "bmm150", &[
            ("in_magn_x_raw", "40"),
            ("in_magn_y_raw", "-20"),
            ("in_magn_z_raw", "80"),
            ("in_magn_scale", "0.005"),
        ]);
        std::env::set_var("CARDPUTER_IIO_DIR", &t.path);

        let out = run_script(
            r#"
            import * as imu from "cardputer:imu";
            import imuDef from "cardputer:imu";
            for (const n of ["devices","read"]) {
                if (!(n in imu)) throw new Error("missing named export " + n);
                if (!(n in imuDef)) throw new Error("missing default export " + n);
            }
            const devs = imu.devices();
            if (!Array.isArray(devs) || devs.join(",") !== "bmi270,bmm150,m5ioe1") {
                throw new Error("devices " + JSON.stringify(devs));
            }
            const r = imu.read();
            if (r.available !== true) throw new Error("not available");
            if (Math.abs(r.accel.z - 16384 * 0.00059855) > 0.01) throw new Error("accel.z " + r.accel.z);
            if (Math.abs(r.gyro.y - (-200 * 0.001065)) > 0.001) throw new Error("gyro.y " + r.gyro.y);
            if (Math.abs(r.mag.z - 0.4) > 0.001) throw new Error("mag.z " + r.mag.z);
            globalThis.__R = "IMU_MOD_OK";
            "#,
        );
        assert_eq!(out, "IMU_MOD_OK");
        std::env::remove_var("CARDPUTER_IIO_DIR");
    }

    #[test]
    fn imu_missing_iio_dir_yields_null_fields_without_throwing() {
        let _lock = env_test_lock();
        std::env::set_var("CARDPUTER_IIO_DIR", "/tmp/cz-imu-mod-no-such-dir");
        let out = run_script(
            r#"
            import imu from "cardputer:imu";
            const r = imu.read();
            if (r.available !== false) throw new Error("expected unavailable");
            if (r.accel !== null || r.gyro !== null || r.mag !== null) {
                throw new Error("missing sensors must be null: " + JSON.stringify(r));
            }
            if (JSON.stringify(imu.devices()) !== "[]") throw new Error("devices must be empty");
            globalThis.__R = "IMU_MISSING_MOD_OK";
            "#,
        );
        assert_eq!(out, "IMU_MISSING_MOD_OK");
        std::env::remove_var("CARDPUTER_IIO_DIR");
    }
}
