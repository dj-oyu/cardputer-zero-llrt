//! NEC infrared protocol encoder/decoder plus LIRC raw transmit (`/dev/lirc0`, `LIRC_MODE_PULSE`
//! little-endian `u32` timings).
//!
//! NEC frame layout (exact): leader `9000 µs` pulse + `4500 µs` space, then 32 bits transmitted
//! LSB-first — `address`, `~address`, `command`, `~command` — where each bit is a `562 µs` pulse
//! followed by a `562 µs` space (0) or a `1687 µs` space (1), and the frame ends with a `562 µs`
//! trailing stop pulse. That yields `2 + 32*2 + 1 = 67` timing values (odd count ending on a
//! pulse), matching what Linux LIRC expects for `LIRC_MODE_PULSE`.
//!
//! TX path resolution: `CARDPUTER_IR_TX_PATH` when set and non-empty, else `/dev/lirc0`.
//! Decoding tolerates ±25% timing deviation (real receivers are noisy) and reports checksum
//! validity through the returned `valid` flag rather than throwing.

use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::env_non_empty;

/// Leader pulse (µs).
pub const LEADER_PULSE_US: u32 = 9000;
/// Leader space (µs).
pub const LEADER_SPACE_US: u32 = 4500;
/// Bit pulse (µs) shared by 0 and 1 bits.
pub const BIT_PULSE_US: u32 = 562;
/// Space encoding a logical 0 (µs).
pub const ZERO_SPACE_US: u32 = 562;
/// Space encoding a logical 1 (µs).
pub const ONE_SPACE_US: u32 = 1687;
/// Trailing stop pulse (µs).
pub const TRAILER_PULSE_US: u32 = 562;
/// Total timing count in one NEC frame: leader (2) + 32 bits (64) + trailer (1).
pub const NEC_TIMINGS: usize = 67;
/// Decoder tolerance window (percent) around nominal timings.
pub const TOLERANCE_PCT: u32 = 25;

/// Decoded NEC frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NecFrame {
    pub address: u8,
    pub command: u8,
    /// True when the address/command complements AND the leader/bit timings all fall inside the
    /// ±25% tolerance window.
    pub valid: bool,
}

fn within_tolerance(value: u32, expected: u32) -> bool {
    let delta = (expected * TOLERANCE_PCT / 100) as i64;
    let diff = i64::from(value) - i64::from(expected);
    diff.abs() <= delta
}

/// Encode an NEC frame into exactly [`NEC_TIMINGS`] microsecond timings (LIRC pulse order).
pub fn encode_nec(address: u8, command: u8) -> Vec<u32> {
    let inv_address = !address;
    let inv_command = !command;
    // Bits travel LSB-first: address bits 0..7, ~address, command, ~command.
    let word = u32::from(address)
        | (u32::from(inv_address) & 0xff) << 8
        | (u32::from(command) & 0xff) << 16
        | (u32::from(inv_command) & 0xff) << 24;

    let mut pulses = Vec::with_capacity(NEC_TIMINGS);
    pulses.push(LEADER_PULSE_US);
    pulses.push(LEADER_SPACE_US);
    for i in 0..32 {
        pulses.push(BIT_PULSE_US);
        let bit = (word >> i) & 1;
        pulses.push(if bit == 1 { ONE_SPACE_US } else { ZERO_SPACE_US });
    }
    pulses.push(TRAILER_PULSE_US);
    pulses
}

/// Decode NEC timings LSB-first. `None` when the stream is too short or the leader is badly
/// formed; otherwise a frame whose `valid` flag reflects complement + timing checks.
pub fn decode_nec(pulses: &[u32]) -> Option<NecFrame> {
    // Minimum usable frame: leader plus 32 complete bit pairs (trailer optional tolerance).
    if pulses.len() < 2 + 32 * 2 {
        return None;
    }
    if !within_tolerance(pulses[0], LEADER_PULSE_US) || !within_tolerance(pulses[1], LEADER_SPACE_US) {
        return None;
    }

    let mut word: u32 = 0;
    let mut timings_ok = true;
    for i in 0..32 {
        let pulse = pulses[2 + i * 2];
        let space = pulses[3 + i * 2];
        if !within_tolerance(pulse, BIT_PULSE_US) {
            timings_ok = false;
        }
        if within_tolerance(space, ONE_SPACE_US) {
            word |= 1 << i;
        } else if !within_tolerance(space, ZERO_SPACE_US) {
            timings_ok = false;
        }
    }
    // Trailing stop pulse when present must look like 562 µs (tolerant, not mandatory).
    if pulses.len() > 2 + 32 * 2 && !within_tolerance(pulses[2 + 32 * 2], TRAILER_PULSE_US) {
        timings_ok = false;
    }

    let address = (word & 0xff) as u8;
    let inv_address = ((word >> 8) & 0xff) as u8;
    let command = ((word >> 16) & 0xff) as u8;
    let inv_command = ((word >> 24) & 0xff) as u8;
    let valid = timings_ok && inv_address == !address && inv_command == !command;
    Some(NecFrame {
        address,
        command,
        valid,
    })
}

/// TX device resolution: `CARDPUTER_IR_TX_PATH` wins when set and non-empty, else `/dev/lirc0`.
pub fn resolve_tx_path() -> PathBuf {
    match env_non_empty("CARDPUTER_IR_TX_PATH") {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from("/dev/lirc0"),
    }
}

/// Write raw pulse timings to the resolved LIRC TX path as little-endian `u32` values and return
/// how many pulses were written.
pub fn send_raw(pulses: &[u32]) -> std::io::Result<usize> {
    send_raw_to(&resolve_tx_path(), pulses)
}

/// Write raw timings to an explicit path (deterministic for tests).
pub fn send_raw_to(path: &Path, pulses: &[u32]) -> std::io::Result<usize> {
    let mut w = BufWriter::new(fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?);
    for p in pulses {
        w.write_all(&p.to_le_bytes())?;
    }
    w.flush()?;
    Ok(pulses.len())
}

/// Encode an NEC frame and transmit it; returns the number of timings written.
pub fn send_nec(address: u8, command: u8) -> std::io::Result<usize> {
    send_raw(&encode_nec(address, command))
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

    #[test]
    fn encoder_produces_exactly_67_timings_with_leader_bit_and_trailer_shape() {
        let p = encode_nec(0x04, 0x08);
        assert_eq!(p.len(), NEC_TIMINGS);
        assert_eq!(p[0], LEADER_PULSE_US);
        assert_eq!(p[1], LEADER_SPACE_US);
        assert_eq!(p[NEC_TIMINGS - 1], TRAILER_PULSE_US);
        // Every bit pulse is 562 µs; the frame ends on a pulse (odd count => LIRC_MODE_PULSE).
        for i in 0..32 {
            assert_eq!(p[2 + i * 2], BIT_PULSE_US);
        }
        // address 0x04 = 0b00000100 LSB-first: bit2 (space slot at index 2+2*2+1) is the only
        // set address bit; bits 0/1 stay zero.
        assert_eq!(p[2 + 2 * 2 + 1], ONE_SPACE_US);
        assert_eq!(p[2 + 0 * 2 + 1], ZERO_SPACE_US);
        assert_eq!(p[2 + 1 * 2 + 1], ZERO_SPACE_US);
        // command 0x08 = 0b00001000 -> bit3 of the command block (index 16+3) is a 1.
        assert_eq!(p[2 + (16 + 3) * 2 + 1], ONE_SPACE_US);
        // ~address (0xFB) clears only bit 2, so bit 1 of ~address (word bit 9) is a 1 ...
        assert_eq!(p[2 + 9 * 2 + 1], ONE_SPACE_US);
        assert_eq!(p[2 + (8 + 2) * 2 + 1], ZERO_SPACE_US); // inverted bit stays inverted
        // Word bits 16..23 carry `command` (0x08 -> only bit 19 set) and bits 24..31 carry
        // `~command` (0xF7 -> every bit set except word bit 27, which mirrors command bit 3).
        assert_eq!(p[2 + 27 * 2 + 1], ZERO_SPACE_US);
        assert_eq!(p[2 + 24 * 2 + 1], ONE_SPACE_US);
        assert_eq!(p[2 + 17 * 2 + 1], ZERO_SPACE_US);
    }

    #[test]
    fn roundtrip_decode_recovers_address_and_command_from_encoder() {
        for (addr, cmd) in [(0x04, 0x08), (0x12, 0x34), (0xFF, 0x00), (0x00, 0xFF)] {
            let f = decode_nec(&encode_nec(addr, cmd)).expect("decodable frame");
            assert_eq!(f.address, addr);
            assert_eq!(f.command, cmd);
            assert!(f.valid, "clean encoder output must validate for {addr:02X}/{cmd:02X}");
        }
    }

    #[test]
    fn decoder_tolerates_noisy_timings_within_25_percent_but_rejects_bad_leader() {
        let mut p = encode_nec(0x7E, 0x21);
        // ±25% jitter on every timing still decodes identically.
        for i in 0..p.len() {
            p[i] = if i % 2 == 0 { p[i] * 8 / 10 + 1 } else { p[i] * 12 / 10 };
        }
        let f = decode_nec(&p).expect("noisy frame must decode");
        assert_eq!(f.address, 0x7E);
        assert_eq!(f.command, 0x21);
        assert!(f.valid, "jitter inside the window stays valid: {f:?}");

        // Leader outside tolerance (9000*0.6 = 5400) is rejected outright -> None.
        let mut bad_leader = encode_nec(0x01, 0x02);
        bad_leader[0] = LEADER_PULSE_US * 6 / 10;
        assert_eq!(decode_nec(&bad_leader), None);

        // Too-short stream: no panic, just None.
        assert_eq!(decode_nec(&[9000, 4500, 562]), None);
        assert_eq!(decode_nec(&[]), None);
    }

    #[test]
    fn complement_mismatch_and_bad_bit_shapes_flag_invalid_frames() {
        // Hand-built frame with a broken address complement: bits 8..15 are NOT ~address.
        let mut p = encode_nec(0x11, 0x22);
        // Flip bit 8 (first bit of ~address) so inv_address no longer matches !address.
        p[2 + 8 * 2 + 1] = if p[2 + 8 * 2 + 1] == ONE_SPACE_US { ZERO_SPACE_US } else { ONE_SPACE_US };
        let f = decode_nec(&p).expect("frame still parses");
        assert_eq!(f.address, 0x11);
        assert_eq!(f.command, 0x22);
        assert!(!f.valid, "complement mismatch must flag invalid");

        // A garbage space width (neither ~562 nor ~1687) marks the frame invalid and is read as
        // a logical 0: address 0x33 (bits 0,1,4,5) loses bit 5 -> 0x13.
        let mut weird = encode_nec(0x33, 0x44);
        weird[2 + 5 * 2 + 1] = 1000; // sits between the 0-window [422..702] and 1-window
        let g = decode_nec(&weird).expect("parses anyway");
        assert!(!g.valid);
        assert_eq!(g.address, 0x13);
        assert_eq!(g.command, 0x44);

        // A malformed trailer flags invalid too (trailing pulse of 9000 µs).
        let mut trailer_bad = encode_nec(0x55, 0x66);
        trailer_bad[66] = LEADER_PULSE_US;
        let t = decode_nec(&trailer_bad).expect("parses");
        assert!(!t.valid);
    }

    #[test]
    fn tx_writes_little_endian_u32_stream_to_env_path_and_returns_count() {
        let _lock = env_test_lock();
        let dir = std::env::temp_dir().join(format!("cz-hal-ir-{}-tx", std::process::id()));
        fs::create_dir_all(&dir).expect("mk ir mock dir");
        let lirc = dir.join("lirc0");
        std::env::set_var("CARDPUTER_IR_TX_PATH", &lirc);
        assert_eq!(resolve_tx_path(), lirc);

        let n = send_nec(0x12, 0x34).expect("tx ok");
        assert_eq!(n, NEC_TIMINGS);
        let raw = fs::read(&lirc).expect("tx file");
        assert_eq!(raw.len(), NEC_TIMINGS * 4);
        assert_eq!(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]), LEADER_PULSE_US);
        assert_eq!(u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]), LEADER_SPACE_US);
        assert_eq!(
            u32::from_le_bytes([raw[(NEC_TIMINGS - 1) * 4], raw[(NEC_TIMINGS - 1) * 4 + 1],
                                 raw[(NEC_TIMINGS - 1) * 4 + 2], raw[(NEC_TIMINGS - 1) * 4 + 3]]),
            TRAILER_PULSE_US
        );

        // Re-send truncates the previous frame (never appends stale pulses).
        let n2 = send_raw_to(&lirc, &[1, 2, 3]).expect("raw ok");
        assert_eq!(n2, 3);
        assert_eq!(fs::read(&lirc).expect("rewritten").len(), 12);

        // Missing device directory surfaces as an io error (no panic).
        std::env::set_var("CARDPUTER_IR_TX_PATH", "/tmp/cz-hal-ir-definitely-missing-dir/lirc0");
        assert!(send_nec(0x01, 0x02).is_err());

        std::env::set_var("CARDPUTER_IR_TX_PATH", "");
        assert_eq!(resolve_tx_path(), PathBuf::from("/dev/lirc0"));
        std::env::remove_var("CARDPUTER_IR_TX_PATH");
        fs::remove_dir_all(&dir).ok();
    }
}
