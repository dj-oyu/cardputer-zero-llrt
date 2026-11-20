//! Bosch BMI270 accelerometer/gyroscope + BMM150 magnetometer via Linux IIO sysfs.
//!
//! IIO root resolution: `CARDPUTER_IIO_DIR` when set and non-empty, otherwise
//! `/sys/bus/iio/devices`. Every device directory is scanned for its `name` attribute, so mock
//! sysfs trees work without root privileges or real hardware. A missing root or missing sensor
//! degrades gracefully (`available: false`, fields `null`) — never panics.
//!
//! Physical units come from the IIO `scale` attributes: `raw * scale` yields m/s^2 for the
//! accelerometer, rad/s for the gyroscope and µT for the magnetometer.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::env_non_empty;

/// One 3-axis physical reading.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vec3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

/// Combined IMU snapshot; `available == false` when no sensor data could be read at all.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ImuReading {
    pub available: bool,
    /// Accelerometer in m/s^2 (BMI270), null when absent/unreadable.
    pub accel: Option<Vec3>,
    /// Angular velocity in rad/s (BMI270), null when absent/unreadable.
    pub gyro: Option<Vec3>,
    /// Magnetic field in µT (BMM150), null when absent/unreadable.
    pub mag: Option<Vec3>,
}

impl ImuReading {
    /// The graceful fallback shape for a missing IIO root or missing sensors.
    pub fn unavailable() -> Self {
        Self {
            available: false,
            accel: None,
            gyro: None,
            mag: None,
        }
    }
}

/// Resolve the IIO device root: `CARDPUTER_IIO_DIR` wins when set and non-empty.
pub fn resolve_iio_root() -> PathBuf {
    match env_non_empty("CARDPUTER_IIO_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from("/sys/bus/iio/devices"),
    }
}

fn read_trimmed(path: &Path) -> Option<String> {
    let mut s = String::new();
    fs::File::open(path).ok()?.read_to_string(&mut s).ok()?;
    let t = s.trim().to_string();
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

fn read_f64(path: &Path) -> Option<f64> {
    read_trimmed(path)?.parse::<f64>().ok()
}

/// Iterate `iio:device*` directories under `root` (sorted by directory name for determinism).
fn iio_device_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = match fs::read_dir(root) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("iio:device"))
                    .unwrap_or(false)
            })
            .collect(),
        Err(_) => return Vec::new(),
    };
    dirs.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    dirs
}

/// Sorted list of every IIO device `name` found under `root` (e.g. bmi270, bmm150, m5ioe1).
pub fn devices_at(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = iio_device_dirs(root)
        .iter()
        .filter_map(|d| read_trimmed(&d.join("name")))
        .collect();
    names.sort();
    names
}

/// Sorted detected IIO device names for the resolved root.
pub fn devices() -> Vec<String> {
    devices_at(&resolve_iio_root())
}

/// Scale factor for one measurement group: the shared `in_<group>_scale` attribute first, then a
/// per-axis `in_<group>_<axis>_scale`, defaulting to 1.0 when neither exists.
fn scale_for(dir: &Path, group: &str, axis: &str) -> f64 {
    match read_f64(&dir.join(format!("in_{group}_scale")))
        .or_else(|| read_f64(&dir.join(format!("in_{group}_{axis}_scale"))))
    {
        Some(s) if s.is_finite() => s,
        _ => 1.0,
    }
}

/// Read one raw 3-axis group and convert it with its scale attributes (`raw * scale`).
/// Returns `None` unless all three `_raw` files parse.
fn read_group(dir: &Path, group: &str) -> Option<Vec3> {
    let x_raw = read_f64(&dir.join(format!("in_{group}_x_raw")))?;
    let y_raw = read_f64(&dir.join(format!("in_{group}_y_raw")))?;
    let z_raw = read_f64(&dir.join(format!("in_{group}_z_raw")))?;
    Some(Vec3 {
        x: x_raw * scale_for(dir, group, "x"),
        y: y_raw * scale_for(dir, group, "y"),
        z: z_raw * scale_for(dir, group, "z"),
    })
}

/// Read the IMU snapshot from an explicit IIO root (deterministic for tests).
pub fn read_at(root: &Path) -> ImuReading {
    let mut accel = None;
    let mut gyro = None;
    let mut mag = None;
    for dir in iio_device_dirs(root) {
        match read_trimmed(&dir.join("name")).map(|n| n.to_lowercase()).as_deref() {
            Some("bmi270") => {
                // Existing values win so a second BMI270 device never overwrites a good reading.
                if accel.is_none() {
                    accel = read_group(&dir, "accel");
                }
                if gyro.is_none() {
                    gyro = read_group(&dir, "anglvel");
                }
            }
            Some("bmm150") => {
                if mag.is_none() {
                    mag = read_group(&dir, "magn");
                }
            }
            _ => {}
        }
    }
    let available = accel.is_some() || gyro.is_some() || mag.is_some();
    ImuReading {
        available,
        accel,
        gyro,
        mag,
    }
}

/// Read the IMU snapshot for the resolved IIO root (never panics on missing hardware).
pub fn read() -> ImuReading {
    read_at(&resolve_iio_root())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Process-wide env-var serialization: all env-touching HAL tests share one lock.
    fn env_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    use std::sync::OnceLock;

    struct TempIio {
        path: PathBuf,
    }

    impl TempIio {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("cz-hal-imu-{}-{}", std::process::id(), name));
            fs::create_dir_all(&path).expect("mk iio mock root");
            Self { path }
        }
        /// Create `iio:deviceN` with a name plus raw/scale attributes.
        fn device(&self, idx: usize, name: &str, attrs: &[(&str, &str)]) {
            let d = self.path.join(format!("iio:device{idx}"));
            fs::create_dir_all(&d).expect("mk iio device dir");
            fs::write(d.join("name"), format!("{name}\n")).expect("write name");
            for (f, v) in attrs {
                fs::write(d.join(f), format!("{v}\n")).expect("write attr");
            }
        }
    }

    impl Drop for TempIio {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.path).ok();
        }
    }

    fn bmi_attrs() -> Vec<(&'static str, &'static str)> {
        vec![
            ("in_accel_x_raw", "1000"),
            ("in_accel_y_raw", "-500"),
            ("in_accel_z_raw", "16384"),
            ("in_accel_scale", "0.00059855"),
            ("in_anglvel_x_raw", "100"),
            ("in_anglvel_y_raw", "-200"),
            ("in_anglvel_z_raw", "0"),
            ("in_anglvel_scale", "0.001065"),
        ]
    }

    fn bmm_attrs() -> Vec<(&'static str, &'static str)> {
        vec![
            ("in_magn_x_raw", "40"),
            ("in_magn_y_raw", "-20"),
            ("in_magn_z_raw", "80"),
            ("in_magn_scale", "0.005"),
        ]
    }

    #[test]
    fn devices_scan_lists_sorted_names_and_ignores_non_iio_entries() {
        let t = TempIio::new("scan");
        t.device(2, "bmm150", &bmm_attrs());
        t.device(0, "m5ioe1", &[("name_unused", "x")]);
        t.device(1, "bmi270", &bmi_attrs());
        fs::create_dir_all(t.path.join("not-an-iio-device")).expect("noise dir");
        fs::write(t.path.join("README.txt"), "ignore me").expect("noise file");
        assert_eq!(devices_at(&t.path), vec!["bmi270", "bmm150", "m5ioe1"]);
    }

    #[test]
    fn full_reading_applies_raw_times_scale_conversions() {
        let t = TempIio::new("full");
        t.device(0, "m5ioe1", &[]);
        t.device(1, "bmi270", &bmi_attrs());
        t.device(2, "bmm150", &bmm_attrs());
        let r = read_at(&t.path);
        assert!(r.available);
        let accel = r.accel.expect("accel present");
        assert!((accel.z - 16384.0 * 0.00059855).abs() < 1e-9);
        assert!((accel.x - 1000.0 * 0.00059855).abs() < 1e-9);
        assert!((accel.y - -500.0 * 0.00059855).abs() < 1e-9);
        let gyro = r.gyro.expect("gyro present");
        assert!((gyro.y - -200.0 * 0.001065).abs() < 1e-12);
        assert!((gyro.x - 100.0 * 0.001065).abs() < 1e-12);
        let mag = r.mag.expect("mag present");
        assert!((mag.z - 0.4).abs() < 1e-12);
        assert!((mag.x - 0.2).abs() < 1e-12);
    }

    #[test]
    fn missing_root_and_missing_sensors_degrade_to_unavailable_nulls() {
        let missing = std::env::temp_dir().join("cz-hal-imu-definitely-missing");
        fs::remove_dir_all(&missing).ok();
        let r = read_at(&missing);
        assert!(!r.available);
        assert!(r.accel.is_none() && r.gyro.is_none() && r.mag.is_none());
        assert_eq!(devices_at(&missing), Vec::<String>::new());

        // Root exists but holds no BMI270/BMM150: still unavailable, no panic.
        let t = TempIio::new("no-sensors");
        t.device(0, "m5ioe1", &[]);
        let r2 = read_at(&t.path);
        assert!(!r2.available);
        assert!(r2.accel.is_none() && r2.gyro.is_none() && r2.mag.is_none());
        assert_eq!(devices_at(&t.path), vec!["m5ioe1"]);
    }

    #[test]
    fn per_axis_scale_fallback_defaults_and_partial_axes_are_none() {
        let t = TempIio::new("per-axis");
        t.device(
            0,
            "bmi270",
            &[
                ("in_accel_x_raw", "2"),
                ("in_accel_y_raw", "4"),
                ("in_accel_z_raw", "8"),
                ("in_accel_x_scale", "0.5"),
                ("in_accel_y_scale", "0.25"),
                // No z scale: defaults to 1.0 per the spec fallback chain.
                ("in_anglvel_x_raw", "7"),
            ],
        );
        let r = read_at(&t.path);
        assert!(r.available, "accel alone still counts as available");
        let accel = r.accel.expect("accel present");
        assert_eq!(accel.x, 1.0);
        assert_eq!(accel.y, 1.0);
        assert_eq!(accel.z, 8.0);
        assert!(r.gyro.is_none(), "incomplete gyro group must be null");
        assert!(r.mag.is_none());

        // All raw axes missing -> None even though a scale file exists.
        let empty = TempIio::new("raw-missing");
        empty.device(0, "bmm150", &[("in_magn_scale", "0.005")]);
        let r2 = read_at(&empty.path);
        assert!(!r2.available);
        assert!(r2.mag.is_none());
    }

    #[test]
    fn env_iio_dir_precedence_drives_devices_and_read() {
        let _lock = env_test_lock();
        let t = TempIio::new("env");
        t.device(0, "bmi270", &bmi_attrs());
        std::env::set_var("CARDPUTER_IIO_DIR", &t.path);
        assert_eq!(resolve_iio_root(), t.path);
        assert_eq!(devices(), vec!["bmi270"]);
        let r = read();
        assert!(r.available && r.accel.is_some() && r.gyro.is_some() && r.mag.is_none());

        std::env::set_var("CARDPUTER_IIO_DIR", "/tmp/cz-hal-imu-nonexistent-dir");
        let miss = read();
        assert!(!miss.available);
        assert_eq!(devices(), Vec::<String>::new());

        // An EMPTY env var must fall back to the default sysfs root (never an empty path).
        std::env::set_var("CARDPUTER_IIO_DIR", "");
        assert_eq!(resolve_iio_root(), PathBuf::from("/sys/bus/iio/devices"));
        std::env::remove_var("CARDPUTER_IIO_DIR");
    }
}
