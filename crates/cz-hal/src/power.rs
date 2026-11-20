//! TI BQ27220 battery gauge via sysfs (`/sys/class/power_supply/bq27220-0`).

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::env_non_empty;

/// Parsed battery status; `available == false` when no gauge directory exists.
#[derive(Clone, Debug, PartialEq)]
pub struct PowerStatus {
    pub available: bool,
    pub present: bool,
    /// Charge percentage 0..=100.
    pub capacity: Option<u8>,
    /// Voltage in millivolts (sysfs `voltage_now` is microvolts).
    pub voltage_mv: Option<i64>,
    /// Current in milliamps (sysfs `current_now` is microamps; negative = discharging).
    pub current_ma: Option<i64>,
    /// Temperature in degrees Celsius (sysfs `temp` is in tenths of a degree).
    pub temp_c: Option<f64>,
    /// Charging state string ("Charging", "Discharging", "Not charging", "Full", "Unknown").
    pub status: String,
    /// Battery health (e.g. "Good").
    pub health: Option<String>,
}

impl PowerStatus {
    /// The unavailable fallback shape.
    pub fn unavailable() -> Self {
        Self {
            available: false,
            present: false,
            capacity: None,
            voltage_mv: None,
            current_ma: None,
            temp_c: None,
            status: "Unavailable".into(),
            health: None,
        }
    }
}

/// Resolve the battery sysfs directory:
/// 1. `CARDPUTER_POWER_DIR` env when set and non-empty (no further fallback).
/// 2. `/sys/class/power_supply/bq27220-0` if it exists.
/// 3. First entry under `/sys/class/power_supply/*` whose `type` file says "Battery".
pub fn resolve_power_dir() -> Option<PathBuf> {
    if let Some(dir) = env_non_empty("CARDPUTER_POWER_DIR") {
        return Some(PathBuf::from(dir));
    }
    let bq = Path::new("/sys/class/power_supply/bq27220-0");
    if bq.is_dir() {
        return Some(bq.to_owned());
    }
    let base = Path::new("/sys/class/power_supply");
    let entries = fs::read_dir(base).ok()?;
    for e in entries.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        if let Ok(t) = read_trimmed(&p.join("type")) {
            if t == "Battery" {
                return Some(p);
            }
        }
    }
    None
}

fn read_trimmed(path: &Path) -> io::Result<String> {
    let mut s = String::new();
    fs::File::open(path)?.read_to_string(&mut s)?;
    Ok(s.trim().to_string())
}

fn read_i64(path: &Path) -> Option<i64> {
    read_trimmed(path).ok()?.parse::<i64>().ok()
}

/// Read the battery status; any missing/failed fields degrade gracefully but `available`
/// reflects whether the gauge directory exists at all.
pub fn status() -> PowerStatus {
    match resolve_power_dir() {
        None => PowerStatus::unavailable(),
        Some(dir) => {
            if !dir.is_dir() {
                return PowerStatus::unavailable();
            }
            let present = read_trimmed(&dir.join("present")).map(|s| s == "1").unwrap_or(false);
            let capacity = read_i64(&dir.join("capacity")).and_then(|v| {
                if (0..=255).contains(&v) {
                    Some(v as u8)
                } else if v > 255 {
                    Some(100u8)
                } else {
                    None
                }
            });
            let voltage_mv = read_i64(&dir.join("voltage_now")).map(|v| v / 1_000);
            let current_ma = read_i64(&dir.join("current_now")).map(|v| v / 1_000);
            let temp_c = read_i64(&dir.join("temp")).map(|v| v as f64 / 10.0);
            let status = read_trimmed(&dir.join("status")).unwrap_or_else(|_| "Unknown".into());
            let health = read_trimmed(&dir.join("health")).ok().filter(|s| !s.is_empty());
            PowerStatus {
                available: true,
                present,
                capacity,
                voltage_mv,
                current_ma,
                temp_c,
                status,
                health,
            }
        }
    }
}

/// Read status from an explicit directory (deterministic for tests).
pub fn status_at(dir: &Path) -> PowerStatus {
    if !dir.is_dir() {
        return PowerStatus::unavailable();
    }
    let present = read_trimmed(&dir.join("present")).map(|s| s == "1").unwrap_or(false);
    let capacity = read_i64(&dir.join("capacity"))
        .and_then(|v| if v < 0 { None } else { Some(v.min(100) as u8) });
    let voltage_mv = read_i64(&dir.join("voltage_now")).map(|v| v / 1_000);
    let current_ma = read_i64(&dir.join("current_now")).map(|v| v / 1_000);
    let temp_c = read_i64(&dir.join("temp")).map(|v| v as f64 / 10.0);
    let status = read_trimmed(&dir.join("status")).unwrap_or_else(|_| "Unknown".into());
    let health = read_trimmed(&dir.join("health")).ok().filter(|s| !s.is_empty());
    PowerStatus { available: true, present, capacity, voltage_mv, current_ma, temp_c, status, health }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("cz-hal-pw-{}-{}", std::process::id(), name));
            fs::create_dir_all(&path).expect("mk battery dir");
            Self { path }
        }
        fn set(&self, f: &str, v: &str) {
            fs::write(self.path.join(f), v).expect("write sysfs mock");
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.path).ok();
        }
    }

    #[test]
    fn full_battery_fields_parsed_with_unit_conversions() {
        let t = TempDir::new("full");
        t.set("present", "1\n");
        t.set("capacity", "87\n");
        t.set("voltage_now", "4123000\n"); // µV -> mV
        t.set("current_now", "-245000\n"); // µA -> mA
        t.set("temp", "393\n"); // 0.1°C -> °C
        t.set("status", "Discharging\n");
        t.set("health", "Good\n");
        let s = status_at(&t.path);
        assert!(s.available && s.present);
        assert_eq!(s.capacity, Some(87));
        assert_eq!(s.voltage_mv, Some(4123));
        assert_eq!(s.current_ma, Some(-245));
        assert!((s.temp_c.unwrap() - 39.3).abs() < 1e-9);
        assert_eq!(s.status, "Discharging");
        assert_eq!(s.health, Some("Good".into()));
    }

    #[test]
    fn missing_optional_fields_degrade_to_none_unknown() {
        let t = TempDir::new("partial");
        t.set("present", "0\n");
        let s = status_at(&t.path);
        assert!(s.available);
        assert!(!s.present);
        assert_eq!(s.capacity, None);
        assert_eq!(s.voltage_mv, None);
        assert_eq!(s.status, "Unknown");
        assert_eq!(s.health, None);
    }

    #[test]
    fn capacity_clamped_and_negative_rejected() {
        let t = TempDir::new("clamp");
        t.set("capacity", "255\n");
        assert_eq!(status_at(&t.path).capacity, Some(100));
        t.set("capacity", "-3\n");
        assert_eq!(status_at(&t.path).capacity, None);
        t.set("capacity", "not-a-number\n");
        assert_eq!(status_at(&t.path).capacity, None);
    }

    #[test]
    fn missing_directory_is_unavailable_not_an_error() {
        let dir = std::env::temp_dir().join("cz-hal-pw-definitely-missing");
        fs::remove_dir_all(&dir).ok();
        let s = status_at(&dir);
        assert!(!s.available);
        assert!(!s.present);
        assert_eq!(s.capacity, None);
        assert_eq!(s.status, "Unavailable");
        assert_eq!(s.health, None);
    }

    #[test]
    fn env_power_dir_precedence_and_missing_env_dir_yields_unavailable() {
        let t = TempDir::new("envpw");
        t.set("present", "1\n");
        t.set("capacity", "55\n");
        std::env::set_var("CARDPUTER_POWER_DIR", &t.path);
        assert_eq!(resolve_power_dir(), Some(t.path.clone()));
        let s = status();
        assert!(s.available && s.present);
        assert_eq!(s.capacity, Some(55));

        std::env::set_var("CARDPUTER_POWER_DIR", "/tmp/definitely-nonexistent-power-dir-cz");
        let missing = status();
        assert!(!missing.available);
        assert_eq!(missing.status, "Unavailable");

        std::env::set_var("CARDPUTER_POWER_DIR", "");
        std::env::remove_var("CARDPUTER_POWER_DIR");
    }
}
