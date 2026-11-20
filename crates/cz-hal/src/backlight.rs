//! Backlight control via sysfs (`/sys/class/backlight/*`) with sleep/wake state.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::env_non_empty;

/// Default max brightness used when `max_brightness` is missing.
const DEFAULT_MAX_BRIGHTNESS: u32 = 100;

/// Resolve the backlight directory:
/// 1. `CARDPUTER_BACKLIGHT_DIR` when set and non-empty.
/// 2. First sorted entry under `/sys/class/backlight/`.
pub fn resolve_backlight_dir() -> Option<PathBuf> {
    if let Some(dir) = env_non_empty("CARDPUTER_BACKLIGHT_DIR") {
        return Some(PathBuf::from(dir));
    }
    let base = Path::new("/sys/class/backlight");
    let mut entries: Vec<PathBuf> = fs::read_dir(base).ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    entries.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    entries.into_iter().next()
}

fn read_u32(path: &Path) -> io::Result<u32> {
    let mut s = String::new();
    fs::File::open(path)?.read_to_string(&mut s)?;
    let t = s.trim();
    t.parse::<u32>()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn write_level(path: &Path, level: u32) -> io::Result<()> {
    fs::write(path, format!("{level}\n"))
}

/// Backlight controller with clamped levels and sleep/wake bookkeeping.
#[derive(Debug, Clone)]
pub struct Backlight {
    dir: PathBuf,
    max: u32,
    saved_level: u32,
    asleep: bool,
}

impl Backlight {
    /// Open the resolved backlight directory (env or first sorted `/sys/class/backlight/*`).
    pub fn new() -> io::Result<Self> {
        let dir = resolve_backlight_dir()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no backlight directory"))?;
        Self::at(dir)
    }

    /// Open a specific backlight directory. `max_brightness` defaults to 100 when missing.
    pub fn at(dir: PathBuf) -> io::Result<Self> {
        if !dir.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("backlight dir {} not found", dir.display()),
            ));
        }
        let max = read_u32(&dir.join("max_brightness")).unwrap_or(DEFAULT_MAX_BRIGHTNESS);
        let current = read_u32(&dir.join("brightness")).ok();
        let asleep = current == Some(0);
        let saved_level = match current {
            Some(v) if v > 0 => v.min(max),
            _ => 1,
        };
        Ok(Self { dir, max, saved_level, asleep })
    }

    fn brightness_path(&self) -> PathBuf {
        self.dir.join("brightness")
    }

    /// Current hardware brightness (fresh read of the sysfs `brightness` file).
    pub fn get_brightness(&self) -> io::Result<u32> {
        read_u32(&self.brightness_path())
    }

    /// The maximum brightness (100 when `max_brightness` is missing).
    pub fn max_brightness(&self) -> u32 {
        self.max
    }

    /// Clamp `level` into `0..=max`, write it, and update awake state.
    /// Returns the clamped value actually written.
    pub fn set_brightness(&mut self, level: u32) -> io::Result<u32> {
        let clamped = level.min(self.max);
        write_level(&self.brightness_path(), clamped)?;
        self.asleep = clamped == 0;
        if clamped > 0 {
            self.saved_level = clamped;
        }
        Ok(clamped)
    }

    /// Turn the backlight off, remembering the current level for [`wake`](Self::wake).
    pub fn sleep(&mut self) -> io::Result<()> {
        if !self.asleep {
            if let Ok(b) = self.get_brightness() {
                if b > 0 {
                    self.saved_level = b;
                }
            }
        }
        write_level(&self.brightness_path(), 0)?;
        self.asleep = true;
        Ok(())
    }

    /// Restore the remembered level (at least 1, clamped to max) and return it.
    pub fn wake(&mut self) -> io::Result<u32> {
        self.asleep = false;
        let level = self.saved_level.max(1).min(self.max);
        write_level(&self.brightness_path(), level)?;
        Ok(level)
    }

    /// Whether the backlight was turned off via [`sleep`](Self::sleep) or level 0.
    pub fn is_asleep(&self) -> bool {
        self.asleep
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("cz-hal-bl-{}-{}", std::process::id(), name));
            fs::create_dir_all(&path).expect("create backlight tmp dir");
            Self { path }
        }
        fn set(&self, file: &str, value: &str) {
            fs::write(self.path.join(file), value).expect("write sysfs mock");
        }
        fn get(&self, file: &str) -> String {
            fs::read_to_string(self.path.join(file))
                .expect("read sysfs mock")
                .trim()
                .to_string()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.path).ok();
        }
    }

    #[test]
    fn set_clamps_and_updates_state() {
        let t = TempDir::new("clamp");
        t.set("max_brightness", "100\n");
        t.set("brightness", "75\n");
        let mut bl = Backlight::at(t.path.clone()).expect("open backlight");
        assert_eq!(bl.max_brightness(), 100);
        assert_eq!(bl.get_brightness().expect("get"), 75);
        assert!(!bl.is_asleep());

        let clamped = bl.set_brightness(150).expect("set 150");
        assert_eq!(clamped, 100);
        assert_eq!(t.get("brightness"), "100");
        assert_eq!(bl.get_brightness().expect("get after"), 100);

        let small = bl.set_brightness(5).expect("set 5");
        assert_eq!(small, 5);
        assert_eq!(t.get("brightness"), "5");
    }

    #[test]
    fn sleep_wake_cycle_restores_saved_level() {
        let t = TempDir::new("sleepwake");
        t.set("max_brightness", "100\n");
        t.set("brightness", "60\n");
        let mut bl = Backlight::at(t.path.clone()).expect("open");
        bl.sleep().expect("sleep");
        assert!(bl.is_asleep());
        assert_eq!(t.get("brightness"), "0");
        let restored = bl.wake().expect("wake");
        assert_eq!(restored, 60);
        assert_eq!(t.get("brightness"), "60");
        assert!(!bl.is_asleep());
    }

    #[test]
    fn zero_level_marks_asleep_and_wake_floors_at_one() {
        let t = TempDir::new("zerofloor");
        t.set("max_brightness", "40\n");
        t.set("brightness", "30\n");
        let mut bl = Backlight::at(t.path.clone()).expect("open");
        bl.set_brightness(0).expect("set 0");
        assert!(bl.is_asleep());
        // saved level unchanged (still 30) -> wake restores 30 clamped to max 40.
        assert_eq!(bl.wake().expect("wake"), 30);

        // Sleep then force set(0) twice; saved stays; wake never returns 0.
        bl.sleep().expect("sleep2");
        assert_eq!(bl.wake().expect("wake2"), 30);
    }

    #[test]
    fn asleep_initial_when_brightness_zero() {
        let t = TempDir::new("initialasleep");
        t.set("max_brightness", "100\n");
        t.set("brightness", "0\n");
        let bl = Backlight::at(t.path.clone()).expect("open");
        assert!(bl.is_asleep());
    }

    #[test]
    fn missing_max_defaults_to_100() {
        let t = TempDir::new("nomax");
        t.set("brightness", "10\n");
        let bl = Backlight::at(t.path.clone()).expect("open even without max_brightness");
        assert_eq!(bl.max_brightness(), 100);
    }

    #[test]
    fn missing_dir_fails_cleanly() {
        let dir = std::env::temp_dir().join("cz-hal-bl-definitely-missing");
        let err = Backlight::at(dir).expect_err("missing dir must not panic");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn env_var_overrides_backlight_resolution() {
        let t = TempDir::new("envres");
        t.set("max_brightness", "77\n");
        std::env::set_var("CARDPUTER_BACKLIGHT_DIR", &t.path);
        assert_eq!(resolve_backlight_dir(), Some(t.path.clone()));
        let bl = Backlight::new().expect("new from env dir");
        assert_eq!(bl.max_brightness(), 77);
        std::env::set_var("CARDPUTER_BACKLIGHT_DIR", "");
        // Empty env behaves like unset: resolution must not crash.
        let _ = resolve_backlight_dir();
        std::env::remove_var("CARDPUTER_BACKLIGHT_DIR");
    }
}
