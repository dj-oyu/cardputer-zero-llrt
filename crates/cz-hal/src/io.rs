//! Safe port discovery plus guarded I2C / UART helpers for CardputerZero add-ons.
//!
//! Device directory resolution: `CARDPUTER_DEV_DIR` when set and non-empty, else `/dev`. The
//! scan never throws away the whole listing when one entry fails — every unreadable name is
//! simply skipped.
//!
//! Reserved addresses: `0x55` is the onboard **BQ27220** power gauge. Writing to it can brick the
//! fuel gauge, so callers must consult [`is_reserved_i2c_addr`] before any I2C transaction.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::env_non_empty;

/// Onboard BQ27220 battery gauge: writing here risks bricking the power meter.
pub const RESERVED_BQ27220_ADDR: i64 = 0x55;

/// Every reserved I2C address (writes are rejected for all of them).
pub const RESERVED_I2C_ADDRESSES: &[i64] = &[RESERVED_BQ27220_ADDR];

/// Hard cap for a single UART read (keeps a chatty device from ballooning the JS heap).
pub const MAX_UART_READ: usize = 65_536;

/// Discovered ports grouped by bus kind (each list sorted).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ports {
    pub i2c: Vec<String>,
    pub spi: Vec<String>,
    pub gpio: Vec<String>,
    pub uart: Vec<String>,
}

/// Resolve the device directory: `CARDPUTER_DEV_DIR` wins when set and non-empty.
pub fn resolve_dev_dir() -> PathBuf {
    match env_non_empty("CARDPUTER_DEV_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from("/dev"),
    }
}

fn name_of(path: &Path) -> Option<String> {
    path.file_name()?.to_str().map(|s| s.to_string())
}

fn is_uart(name: &str) -> bool {
    // ttyS* (legacy 16550-style), ttyAMA* (ARM PL011) and serial* (mgetty-style links).
    name.starts_with("ttyS") || name.starts_with("ttyAMA") || name.starts_with("serial")
}

/// Classify one entry name into a bus bucket (false for anything not exposed to user code).
pub fn classify_port(name: &str) -> Option<&'static str> {
    if name.starts_with("i2c-") {
        Some("i2c")
    } else if name.starts_with("spidev") {
        Some("spi")
    } else if name.starts_with("gpiochip") {
        Some("gpio")
    } else if is_uart(name) {
        Some("uart")
    } else {
        None
    }
}

/// Enumerate ports inside an explicit directory (deterministic for tests). Missing directories
/// yield empty lists instead of an error.
pub fn list_ports_at(dir: &Path) -> Ports {
    let mut ports = Ports::default();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return ports,
    };
    for e in entries.flatten() {
        let name = match name_of(&e.path()) {
            Some(n) => n,
            None => continue,
        };
        match classify_port(&name) {
            Some("i2c") => ports.i2c.push(name),
            Some("spi") => ports.spi.push(name),
            Some("gpio") => ports.gpio.push(name),
            Some("uart") => ports.uart.push(name),
            _ => {}
        }
    }
    for list in [&mut ports.i2c, &mut ports.spi, &mut ports.gpio, &mut ports.uart] {
        list.sort();
    }
    ports
}

/// Enumerate ports in the resolved device directory.
pub fn list_ports() -> Ports {
    list_ports_at(&resolve_dev_dir())
}

/// Whether an I2C address is reserved and MUST NOT be written (BQ27220 gauge at `0x55`).
pub fn is_reserved_i2c_addr(addr: i64) -> bool {
    RESERVED_I2C_ADDRESSES.contains(&addr)
}

/// Write bytes to a UART/device path, returning how many bytes were accepted. Short writes are
/// retried until the buffer is drained or an error occurs.
pub fn uart_write(path: &Path, data: &[u8]) -> std::io::Result<usize> {
    let mut f = OpenOptions::new().write(true).create(true).open(path)?;
    let mut written = 0usize;
    while written < data.len() {
        match f.write(&data[written..]) {
            Ok(0) => break, // Device refuses further writes: report what landed.
            Ok(n) => written += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                if written > 0 {
                    return Ok(written);
                }
                return Err(e);
            }
        }
    }
    Ok(written)
}

/// Read up to `max_bytes` (clamped into `0..=65536`) from a UART/device path.
pub fn uart_read(path: &Path, max_bytes: usize) -> std::io::Result<Vec<u8>> {
    let cap = max_bytes.min(MAX_UART_READ);
    if cap == 0 {
        return Ok(Vec::new());
    }
    let mut f = OpenOptions::new().read(true).open(path)?;
    let mut buf = Vec::with_capacity(cap.min(8192));
    loop {
        if buf.len() >= cap {
            break;
        }
        let mut chunk = [0u8; 4096];
        let want = cap - buf.len();
        let take = if want < chunk.len() { want } else { chunk.len() };
        match f.read(&mut chunk[..take]) {
            Ok(0) => break, // EOF / nothing more available.
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                if !buf.is_empty() {
                    break;
                }
                return Err(e);
            }
        }
    }
    Ok(buf)
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

    struct TempDev {
        path: PathBuf,
    }

    impl TempDev {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("cz-hal-io-{}-{}", std::process::id(), name));
            fs::create_dir_all(&path).expect("mk dev mock dir");
            Self { path }
        }
        fn touch(&self, names: &[&str]) {
            for n in names {
                fs::write(self.path.join(n), "").expect("touch port");
            }
        }
    }

    impl Drop for TempDev {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.path).ok();
        }
    }

    #[test]
    fn port_scan_groups_by_bus_and_sorts_while_ignoring_unrelated_entries() {
        let t = TempDev::new("scan");
        t.touch(&["i2c-1", "i2c-0", "spidev0.1", "gpiochip0", "gpiochip1", "ttyS0", "serial0"]);
        // Unrelated device nodes and subdirectories must never leak into the listing.
        t.touch(&["null", "fb0", "i2c_dev_note", "spi1", "gpio12"]);
        fs::create_dir_all(t.path.join("input")).expect("subdir");
        let p = list_ports_at(&t.path);
        assert_eq!(p.i2c, vec!["i2c-0", "i2c-1"]);
        assert_eq!(p.spi, vec!["spidev0.1"]);
        assert_eq!(p.gpio, vec!["gpiochip0", "gpiochip1"]);
        assert_eq!(p.uart, vec!["serial0", "ttyS0"]);
        assert!(classify_port("ttyAMA0") == Some("uart"));
        assert!(classify_port("random") == None);
    }

    #[test]
    fn missing_dev_dir_yields_empty_port_lists_without_error() {
        let missing = std::env::temp_dir().join("cz-hal-io-definitely-missing");
        fs::remove_dir_all(&missing).ok();
        let p = list_ports_at(&missing);
        assert!(p.i2c.is_empty() && p.spi.is_empty() && p.gpio.is_empty() && p.uart.is_empty());
    }

    #[test]
    fn reserved_bq27220_address_guard_blocks_writes_only_on_0x55() {
        assert!(is_reserved_i2c_addr(RESERVED_BQ27220_ADDR));
        assert!(is_reserved_i2c_addr(85), "0x55 == 85 decimal");
        assert!(!is_reserved_i2c_addr(0x3c));
        assert!(!is_reserved_i2c_addr(0x54));
        assert!(!is_reserved_i2c_addr(0x56));
        assert!(!is_reserved_i2c_addr(-1));
        assert_eq!(RESERVED_I2C_ADDRESSES, &[0x55]);
    }

    #[test]
    fn uart_write_read_roundtrip_reports_exact_byte_counts() {
        let t = TempDev::new("uart");
        let node = t.path.join("ttyS0");
        fs::write(&node, "").expect("seed uart node");
        let n = uart_write(&node, b"AT+OK\r\n").expect("write ok");
        assert_eq!(n, 7);
        let back = uart_read(&node, 64).expect("read ok");
        assert_eq!(back.len(), 7);
        assert_eq!(back[0], b'A');
        assert_eq!(&back[..], b"AT+OK\r\n");

        // Writing into a fresh path creates it; read max_bytes=0 yields an empty buffer.
        let fresh = t.path.join("serial9");
        assert_eq!(uart_write(&fresh, b"hello").expect("create write"), 5);
        assert_eq!(uart_read(&fresh, 0).expect("zero cap"), Vec::<u8>::new());
        assert_eq!(uart_read(&fresh, 3).expect("partial read").len(), 3);

        // A path inside a nonexistent directory surfaces as an io error (never a panic).
        let ghost = t.path.join("no-such-dir").join("ttyS5");
        assert!(uart_read(&ghost, 32).is_err());
        assert!(uart_write(&ghost, b"x").is_err());
    }

    #[test]
    fn uart_read_caps_at_65536_bytes_regardless_of_requested_size() {
        let t = TempDev::new("cap");
        let big = t.path.join("ttyS1");
        fs::write(&big, vec![0xAB_u8; 70_000]).expect("seed big fixture");
        let capped = uart_read(&big, 999_999).expect("capped read");
        assert_eq!(capped.len(), MAX_UART_READ);
        assert!(capped.iter().all(|b| *b == 0xAB));
        assert_eq!(uart_read(&big, 65_537).expect("just over cap").len(), MAX_UART_READ);
        assert_eq!(uart_read(&big, 10).expect("small read").len(), 10);
    }

    #[test]
    fn env_dev_dir_precedence_drives_list_ports_and_empty_falls_back() {
        let _lock = env_test_lock();
        let t = TempDev::new("envdev");
        t.touch(&["i2c-7", "gpiochip3"]);
        std::env::set_var("CARDPUTER_DEV_DIR", &t.path);
        assert_eq!(resolve_dev_dir(), t.path);
        let p = list_ports();
        assert_eq!(p.i2c, vec!["i2c-7"]);
        assert_eq!(p.gpio, vec!["gpiochip3"]);
        assert!(p.spi.is_empty() && p.uart.is_empty());

        std::env::set_var("CARDPUTER_DEV_DIR", "/tmp/cz-hal-io-nonexistent-dir");
        assert!(list_ports().i2c.is_empty());

        std::env::set_var("CARDPUTER_DEV_DIR", "");
        assert_eq!(resolve_dev_dir(), PathBuf::from("/dev"));
        std::env::remove_var("CARDPUTER_DEV_DIR");
    }
}
