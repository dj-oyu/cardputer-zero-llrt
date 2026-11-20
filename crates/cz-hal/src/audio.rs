//! ES8389-Audio / PipeWire playback plus a **pure-Rust** PCM WAV synthesizer (no external
//! audio crate, no C dependencies): 44.1 kHz, 16-bit signed little-endian mono RIFF/WAVE.
//!
//! Output resolution: `CARDPUTER_AUDIO_OUT` when set and non-empty is a deterministic file sink
//! (the WAV bytes are written there — ideal for headless testing without speakers). Otherwise the
//! HAL attempts `pw-play` (with an `XDG_RUNTIME_DIR=/run/user/1000` fallback for root-run
//! services) and finally `aplay`, reporting success/failure as a plain bool. Nothing ever panics.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::env_non_empty;

/// Synthesis sample rate (Hz).
pub const SAMPLE_RATE: u32 = 44_100;
/// Bits per sample (signed little-endian).
pub const BITS_PER_SAMPLE: u16 = 16;
/// Mono output.
pub const CHANNELS: u16 = 1;
/// RIFF/WAVE header size (bare canonical layout: RIFF + fmt + data).
pub const HEADER_BYTES: usize = 44;
/// Audible frequency window clamped by `generate_wav`.
pub const FREQ_MIN: f64 = 20.0;
pub const FREQ_MAX: f64 = 20_000.0;
/// Duration window clamped by `generate_wav`.
pub const DURATION_MIN_MS: f64 = 1.0;
pub const DURATION_MAX_MS: f64 = 10_000.0;
/// Default amplitude when the caller supplies no volume.
pub const DEFAULT_VOLUME: f64 = 0.5;

fn clamp(v: f64, lo: f64, hi: f64, fallback: f64) -> f64 {
    if !v.is_finite() {
        fallback.clamp(lo, hi)
    } else {
        v.clamp(lo, hi)
    }
}

/// Sample count for a clamped duration at 44.1 kHz (exactly `duration_ms * 44.1` samples).
pub fn sample_count(duration_ms: f64) -> usize {
    let d = clamp(duration_ms, DURATION_MIN_MS, DURATION_MAX_MS, 100.0);
    (((d * SAMPLE_RATE as f64) / 1000.0).round() as usize).max(1)
}

fn write_u32_le(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn write_u16_le(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// Synthesize a canonical 44-byte-header RIFF/WAVE mono sine wave.
///
/// `freqHz` clamps to `20..=20000`, `durationMs` to `1..=10000`, `volume` to `0.0..=1.0`
/// (default `0.5`). Non-finite inputs fall back to the defaults instead of producing NaN samples.
pub fn generate_wav(freq_hz: f64, duration_ms: f64, volume: f64) -> Vec<u8> {
    let freq = clamp(freq_hz, FREQ_MIN, FREQ_MAX, 440.0);
    let samples = sample_count(duration_ms);
    let vol = if volume.is_finite() {
        volume.clamp(0.0, 1.0)
    } else {
        DEFAULT_VOLUME
    };

    let data_bytes = samples * 2; // 16-bit mono
    let bytes_per_sample = u32::from(BITS_PER_SAMPLE / 8);
    let byte_rate = SAMPLE_RATE * u32::from(CHANNELS) * bytes_per_sample;
    // Mono 16-bit => exactly 2 bytes per frame; fits in u16 by construction.
    let block_align: u16 = CHANNELS * (BITS_PER_SAMPLE / 8);

    let mut out = Vec::with_capacity(HEADER_BYTES + data_bytes);
    out.extend_from_slice(b"RIFF");
    write_u32_le(&mut out, (36 + data_bytes) as u32);
    out.extend_from_slice(b"WAVE");

    out.extend_from_slice(b"fmt ");
    write_u32_le(&mut out, 16);
    write_u16_le(&mut out, 1); // PCM (uncompressed)
    write_u16_le(&mut out, CHANNELS);
    write_u32_le(&mut out, SAMPLE_RATE);
    write_u32_le(&mut out, byte_rate);
    write_u16_le(&mut out, block_align);
    write_u16_le(&mut out, BITS_PER_SAMPLE);

    out.extend_from_slice(b"data");
    write_u32_le(&mut out, data_bytes as u32);

    let amplitude = (i16::MAX as f64) * vol;
    let two_pi = std::f64::consts::TAU;
    for i in 0..samples {
        let phase = two_pi * freq * (i as f64) / (SAMPLE_RATE as f64);
        let s = (phase.sin() * amplitude).round();
        // Rounding NaN would be undefined-ish, so guard: silence for non-finite phases.
        let v: i16 = if s.is_finite() { s as i16 } else { 0 };
        write_i16_le(&mut out, v);
    }
    out
}

fn write_i16_le(out: &mut Vec<u8>, v: i16) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// The deterministic file sink (`CARDPUTER_AUDIO_OUT`), or `None` when unset/empty.
pub fn resolve_audio_out() -> Option<PathBuf> {
    env_non_empty("CARDPUTER_AUDIO_OUT").map(PathBuf::from)
}

static BEEP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Unique temp WAV path for a single playback attempt.
fn beep_temp_path() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seq = BEEP_SEQ.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("cz-beep-{}-{}-{}.wav", std::process::id(), nanos, seq))
}

/// Try PipeWire (`pw-play`) first, then ALSA (`aplay`). Both get an `XDG_RUNTIME_DIR` fallback of
/// `/run/user/1000` so a root-run process can still reach the user's PipeWire socket.
fn spawn_player(path: &Path) -> bool {
    let xdg = match env_non_empty("XDG_RUNTIME_DIR") {
        Some(v) => v,
        None => "/run/user/1000".to_string(),
    };
    for prog in ["pw-play", "aplay"] {
        let status = Command::new(prog)
            .env("XDG_RUNTIME_DIR", &xdg)
            .arg(path)
            .status();
        if matches!(&status, Ok(s) if s.success()) {
            return true;
        }
    }
    false
}

/// Play a tone: writes to `CARDPUTER_AUDIO_OUT` when set (deterministic, hardware-free), else
/// synthesizes into a temp file and drives `pw-play`/`aplay`. Returns whether playback succeeded.
pub fn beep(freq_hz: f64, duration_ms: f64, volume: f64) -> bool {
    let wav = generate_wav(freq_hz, duration_ms, volume);
    if let Some(out) = resolve_audio_out() {
        return match fs::File::create(&out) {
            Ok(mut f) => f.write_all(&wav).is_ok(),
            Err(_) => false,
        };
    }
    let tmp = beep_temp_path();
    match fs::write(&tmp, &wav) {
        Ok(()) => {
            let ok = spawn_player(&tmp);
            // Never leave a stale tone file behind.
            fs::remove_file(&tmp).ok();
            ok
        }
        Err(_) => false,
    }
}

/// Replay an existing WAV file from disk. Validates existence and the `"RIFF"` tag first; with
/// `CARDPUTER_AUDIO_OUT` set the bytes are copied there (already the sink -> plain success).
pub fn play_wav(path: &Path) -> bool {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(_) => return false,
    };
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" {
        return false;
    }
    if let Some(out) = resolve_audio_out() {
        // Writing the sink onto itself would truncate the source mid-copy: treat as done.
        if let (Ok(src_canon), Ok(dst_canon)) = (path.canonicalize(), out.canonicalize()) {
            if src_canon == dst_canon {
                return true;
            }
        }
        return match fs::File::create(&out) {
            Ok(mut f) => f.write_all(&bytes).is_ok(),
            Err(_) => false,
        };
    }
    spawn_player(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    use std::sync::OnceLock;

    struct TempOut {
        path: PathBuf,
    }

    impl TempOut {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("cz-hal-aud-{}-{}", std::process::id(), name));
            fs::create_dir_all(&dir).expect("mk audio mock dir");
            Self {
                path: dir.join("sink.wav"),
            }
        }
    }

    impl Drop for TempOut {
        fn drop(&mut self) {
            fs::remove_dir_all(self.path.parent().expect("parent")).ok();
        }
    }

    fn u32_at(b: &[u8], off: usize) -> u32 {
        u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
    }

    fn u16_at(b: &[u8], off: usize) -> u16 {
        u16::from_le_bytes([b[off], b[off + 1]])
    }

    #[test]
    fn wav_header_is_canonical_riff_wave_with_exact_sizes() {
        let wav = generate_wav(440.0, 100.0, 0.5);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(wav.len(), HEADER_BYTES + 4410 * 2);
        assert_eq!(u32_at(&wav, 4), (36 + 4410 * 2) as u32);
        assert_eq!(u32_at(&wav, 16), 16); // fmt chunk size
        assert_eq!(u16_at(&wav, 20), 1); // PCM
        assert_eq!(u16_at(&wav, 22), CHANNELS);
        assert_eq!(u32_at(&wav, 24), SAMPLE_RATE);
        assert_eq!(u32_at(&wav, 28), 88_200); // byte rate
        assert_eq!(u16_at(&wav, 32), 2); // block align
        assert_eq!(u16_at(&wav, 34), BITS_PER_SAMPLE);
        assert_eq!(u32_at(&wav, 40), (4410 * 2) as u32);
    }

    #[test]
    fn window_clamps_bounds_and_non_finite_inputs_never_produce_nan() {
        // Frequency window edges clamp (20 Hz..20 kHz), duration clamps to 1..=10000 ms.
        let low = generate_wav(1.0, 50.0, 0.5);
        assert_eq!(low.len(), HEADER_BYTES + 2205 * 2);
        let high = generate_wav(99_999.0, 0.0, 0.5);
        assert_eq!(high.len(), HEADER_BYTES + 44.0f64 as usize * 2); // 1ms -> 44 samples
        let weird = generate_wav(f64::NAN, f64::INFINITY, -3.0);
        assert_eq!(weird.len(), HEADER_BYTES + sample_count(f64::INFINITY) * 2);
        // Volume below 0 clamps to silence, above 1 clamps to full scale.
        let silent = generate_wav(440.0, 20.0, -1.0);
        for i in (HEADER_BYTES..silent.len()).step_by(2) {
            assert_eq!(i16::from_le_bytes([silent[i], silent[i + 1]]), 0);
        }
        assert_eq!(sample_count(-5.0), 44); // clamp to 1 ms
        assert_eq!(sample_count(20_000.0), 441_000); // clamp to 10 s
    }

    #[test]
    fn sine_wave_is_non_silent_and_volume_scaled() {
        let half = generate_wav(880.0, 50.0, 0.6);
        let mut nonzero = 0;
        let mut peak: i16 = 0;
        for i in (HEADER_BYTES..half.len()).step_by(2) {
            let s = i16::from_le_bytes([half[i], half[i + 1]]);
            if s != 0 {
                nonzero += 1;
            }
            peak = peak.max(if s < 0 { -s } else { s });
        }
        // 880 Hz over 2205 samples has ~4 zero crossings only: nearly everything is non-silent.
        assert!(nonzero > 1000, "expected a dense sine waveform, got nonzero={nonzero}");
        assert!((f64::from(peak) - 32767.0 * 0.6).abs() <= 200.0, "peak={peak}");
        let loud = generate_wav(880.0, 50.0, 1.0);
        let mut loud_peak: i16 = 0;
        for i in (HEADER_BYTES..loud.len()).step_by(2) {
            let s = i16::from_le_bytes([loud[i], loud[i + 1]]);
            loud_peak = loud_peak.max(if s < 0 { -s } else { s });
        }
        assert!(loud_peak > peak, "volume 1.0 must be louder than 0.6");
    }

    #[test]
    fn beep_writes_the_env_sink_file_and_reports_true() {
        let _lock = env_test_lock();
        let t = TempOut::new("beep");
        std::env::set_var("CARDPUTER_AUDIO_OUT", &t.path);
        assert!(beep(880.0, 50.0, 0.6));
        let file = fs::read(&t.path).expect("sink wav written");
        assert_eq!(&file[..4], b"RIFF");
        assert_eq!(&file[8..12], b"WAVE");
        assert_eq!(file.len(), HEADER_BYTES + 2205 * 2);
        assert_eq!(u32_at(&file, 24), SAMPLE_RATE);
        std::env::remove_var("CARDPUTER_AUDIO_OUT");
    }

    #[test]
    fn play_wav_validates_riff_tag_copies_to_sink_and_rejects_junk() {
        let _lock = env_test_lock();
        let dir = std::env::temp_dir().join(format!("cz-hal-aud-play-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("mk dir");
        let wav = dir.join("tone.wav");
        fs::write(&wav, generate_wav(660.0, 30.0, 0.4)).expect("seed wav");
        let junk = dir.join("not-a-wav.bin");
        fs::write(&junk, b"NOT_RIFF_AT_ALL").expect("seed junk");

        // Missing file -> false, garbage -> false (never panics).
        assert!(!play_wav(&dir.join("definitely-missing.wav")));
        assert!(!play_wav(&junk));

        std::env::set_var("CARDPUTER_AUDIO_OUT", dir.join("sink2.wav"));
        assert!(play_wav(&wav), "valid WAV must copy into the sink");
        assert_eq!(fs::read(dir.join("sink2.wav")).expect("copy").len(), HEADER_BYTES + 1323 * 2);
        assert!(!play_wav(&junk), "sink path still rejects non-RIFF input");
        // Source == sink: copying onto itself reports success without truncating the source.
        std::env::set_var("CARDPUTER_AUDIO_OUT", &wav);
        assert!(play_wav(&wav));
        assert!(fs::read(&wav).expect("source intact").len() > 0, "self-copy must not truncate");
        std::env::remove_var("CARDPUTER_AUDIO_OUT");

        // Without any player binary present (sandbox) playback honestly reports failure.
        assert!(!play_wav(&wav));
        assert!(!beep(440.0, 10.0, 0.5));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sink_resolution_prefers_non_empty_env_only() {
        let _lock = env_test_lock();
        std::env::remove_var("CARDPUTER_AUDIO_OUT");
        assert!(resolve_audio_out().is_none());
        std::env::set_var("CARDPUTER_AUDIO_OUT", "");
        assert!(resolve_audio_out().is_none(), "empty sink means no file sink");
        std::env::set_var("CARDPUTER_AUDIO_OUT", "/tmp/cz-audio-sink-target.wav");
        assert_eq!(
            resolve_audio_out(),
            Some(PathBuf::from("/tmp/cz-audio-sink-target.wav"))
        );
        std::env::remove_var("CARDPUTER_AUDIO_OUT");
    }
}
