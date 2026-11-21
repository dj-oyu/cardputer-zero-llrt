//! `cardputer:audio` — ES8389/PipeWire playback backed by the pure-Rust 44.1 kHz 16-bit mono PCM
//! WAV synthesizer in [`cz_hal::audio`] (zero external audio crates).
//!
//! The JavaScript surface (`generateWav`, `beep`, `playWav`) is a thin wrapper over Rust natives
//! that only accept required scalars: the optional `volume` argument is normalized to a number by
//! the shim (`NaN` means "use the HAL default 0.5"), which sidesteps rquickjs rejecting an explicit
//! `undefined` for optional parameters. `CARDPUTER_AUDIO_OUT` turns output into a deterministic
//! file sink so headless runs (CI, sandbox) verify real bytes without speakers.

use std::path::Path;

use cz_hal::audio;
use rquickjs::{Ctx, Function, Object, Result, Value};

use crate::vec_to_uint8array;

/// JS glue: normalize optional args into required numeric scalars before calling the natives.
const AUDIO_SHIM: &str = r#"
(function () {
    if (globalThis.__czAudioInstalled) return;
    function num(v, dflt) { return (typeof v === "number" && isFinite(v)) ? v : dflt; }
    // NaN sentinel: cz_hal::audio treats a non-finite volume as "default 0.5".
    function vol(v) { return (typeof v === "number") ? v : NaN; }
    globalThis.__czAudioGenerateWav = function (freqHz, durationMs, volume) {
        return __czAudioGenerateWavNative(num(freqHz, 440), num(durationMs, 100), vol(volume));
    };
    globalThis.__czAudioBeep = function (freqHz, durationMs, volume) {
        return __czAudioBeepNative(num(freqHz, 440), num(durationMs, 100), vol(volume));
    };
    globalThis.__czAudioPlayWav = function (path) {
        return __czAudioPlayWavNative(typeof path === "string" ? path : String(path));
    };
    globalThis.__czAudioInstalled = true;
})();
"#;

fn generate_wav_native<'js>(ctx: &Ctx<'js>, freq: f64, duration: f64, volume: f64) -> Result<Value<'js>> {
    // Phase 10C: hand the freshly generated WAV over by ownership (zero FFI copy).
    vec_to_uint8array(ctx, audio::generate_wav(freq, duration, volume))
}

fn beep_native(freq: f64, duration: f64, volume: f64) -> bool {
    audio::beep(freq, duration, volume)
}

fn play_wav_native(path: String) -> bool {
    audio::play_wav(Path::new(&path))
}

/// `cardputer:audio` module definition (named exports plus a `default` object).
pub struct AudioModule;

impl rquickjs::module::ModuleDef for AudioModule {
    fn declare<'js>(decl: &rquickjs::module::Declarations<'js>) -> Result<()> {
        for name in ["generateWav", "beep", "playWav", "default"] {
            decl.declare(name)?;
        }
        Ok(())
    }

    fn evaluate<'js>(ctx: &Ctx<'js>, exports: &rquickjs::module::Exports<'js>) -> Result<()> {
        ctx.eval::<(), _>(AUDIO_SHIM)?;

        let gen_native = Function::new(
            ctx.clone(),
            |ctx: Ctx<'js>, freq: f64, duration: f64, volume: f64| -> Result<Value<'js>> {
                generate_wav_native(&ctx, freq, duration, volume)
            },
        )?;
        ctx.globals().set("__czAudioGenerateWavNative", gen_native.clone())?;

        let beep_fn = Function::new(ctx.clone(), |freq: f64, duration: f64, volume: f64| -> bool {
            beep_native(freq, duration, volume)
        })?;
        ctx.globals().set("__czAudioBeepNative", beep_fn.clone())?;

        let play_fn = Function::new(ctx.clone(), |path: String| -> bool { play_wav_native(path) })?;
        ctx.globals().set("__czAudioPlayWavNative", play_fn.clone())?;

        let js_generate: Function<'js> = ctx.globals().get("__czAudioGenerateWav")?;
        let js_beep: Function<'js> = ctx.globals().get("__czAudioBeep")?;
        let js_play: Function<'js> = ctx.globals().get("__czAudioPlayWav")?;

        exports.export("generateWav", js_generate.clone())?;
        exports.export("beep", js_beep.clone())?;
        exports.export("playWav", js_play.clone())?;

        let default_obj = Object::new(ctx.clone())?;
        default_obj.set("generateWav", js_generate)?;
        default_obj.set("beep", js_beep)?;
        default_obj.set("playWav", js_play)?;
        exports.export("default", default_obj)?;
        Ok(())
    }
}

#[cfg(test)]
mod audio_module_tests {
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
            Module::declare_def::<AudioModule, _>(ctx.clone(), "cardputer:audio").unwrap();
            let p = Module::evaluate(ctx.clone(), "aud-test.mjs", source).unwrap();
            p.finish::<()>().unwrap();
            ctx.globals().get::<_, String>("__R").unwrap_or_else(|_| "NO_R".into())
        });
        while let Ok(true) = rt.execute_pending_job() {}
        out
    }

    /// Sink directory unique per test (env vars are process-wide, hence the shared lock).
    struct Sink {
        dir: std::path::PathBuf,
        path: std::path::PathBuf,
    }

    impl Sink {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("cz-aud-mod-{}-{}", std::process::id(), name));
            std::fs::create_dir_all(&dir).expect("mk sink dir");
            Self {
                dir: dir.clone(),
                path: dir.join("out.wav"),
            }
        }
    }

    impl Drop for Sink {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    #[test]
    fn generate_wav_returns_valid_uint8array_riff_wave_via_module() {
        let _lock = env_test_lock();
        let s = Sink::new("gen");
        std::env::set_var("CARDPUTER_AUDIO_OUT", &s.path);
        let out = run_script(
            r#"
            import * as audio from "cardputer:audio";
            import audioDef from "cardputer:audio";
            for (const n of ["generateWav","beep","playWav"]) {
                if (!(n in audio)) throw new Error("missing named " + n);
                if (!(n in audioDef)) throw new Error("missing default " + n);
            }
            const wav = audio.generateWav(440, 100, 0.5);
            if (!(wav instanceof Uint8Array)) throw new Error("must be Uint8Array");
            if (wav.length !== 44 + 4410 * 2) throw new Error("length " + wav.length);
            if (wav[0]!==0x52||wav[1]!==0x49||wav[2]!==0x46||wav[3]!==0x46) throw new Error("RIFF missing");
            if (wav[8]!==0x57||wav[9]!==0x41||wav[10]!==0x56||wav[11]!==0x45) throw new Error("WAVE missing");
            if (audio.generateWav(440, 100).length !== wav.length) throw new Error("default volume mismatch");
            if (audio.generateWav(5, 20).length !== audio.generateWav(20, 20).length) throw new Error("freq clamp");
            globalThis.__R = "AUDIO_GEN_MOD_OK";
            "#,
        );
        assert_eq!(out, "AUDIO_GEN_MOD_OK");
        std::env::remove_var("CARDPUTER_AUDIO_OUT");
    }

    #[test]
    fn beep_and_play_wav_write_real_non_silent_bytes_into_the_env_sink() {
        let _lock = env_test_lock();
        let s = Sink::new("beep");
        std::env::set_var("CARDPUTER_AUDIO_OUT", &s.path);
        let out = run_script(
            &format!(
                r#"
                import audio from "cardputer:audio";
                if (audio.beep(880, 50, 0.6) !== true) throw new Error("beep false");
                const before = audio.generateWav(440, 10, 0.3);
                if (audio.playWav({:?}) !== false) throw new Error("missing file must be false");
                globalThis.__R = "AUDIO_BEEP_MOD_OK";
                "#,
                s.dir.join("nope.wav").display().to_string()
            ),
        );
        assert_eq!(out, "AUDIO_BEEP_MOD_OK");
        let bytes = std::fs::read(&s.path).expect("beep wrote sink");
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(bytes.len(), 44 + 2205 * 2);
        let mut nonzero = 0;
        for i in (44..bytes.len()).step_by(2) {
            if i16::from_le_bytes([bytes[i], bytes[i + 1]]) != 0 {
                nonzero += 1;
            }
        }
        assert!(nonzero > 1000, "sine must be dense: nonzero={nonzero}");

        // playWav replays a real WAV into the sink and rejects non-RIFF garbage.
        let tone = s.dir.join("tone.wav");
        std::fs::write(&tone, &bytes).expect("seed tone");
        let junk = s.dir.join("junk.bin");
        std::fs::write(&junk, b"NOPE_NOPE").expect("seed junk");
        let replay = run_script(
            &format!(
                r#"
                import * as audio from "cardputer:audio";
                if (audio.playWav({:?}) !== true) throw new Error("playWav true expected");
                if (audio.playWav({:?}) !== false) throw new Error("junk must fail");
                globalThis.__R = "AUDIO_PLAY_MOD_OK";
                "#,
                tone.display().to_string(),
                junk.display().to_string()
            ),
        );
        assert_eq!(replay, "AUDIO_PLAY_MOD_OK");
        std::env::remove_var("CARDPUTER_AUDIO_OUT");
    }

    #[test]
    fn beep_reports_false_when_no_sink_and_no_player_are_available() {
        let _lock = env_test_lock();
        // Sandbox has no pw-play/aplay: honest failure, no hang and no panic.
        std::env::remove_var("CARDPUTER_AUDIO_OUT");
        let out = run_script(
            r#"
            import * as audio from "cardputer:audio";
            const ok = audio.beep(440, 20, 0.5);
            if (typeof ok !== "boolean") throw new Error("beep must return boolean");
            globalThis.__R = ok ? "UNEXPECTED_PLAYBACK" : "AUDIO_NO_PLAYER_OK";
            "#,
        );
        assert_eq!(out, "AUDIO_NO_PLAYER_OK");
    }
}
