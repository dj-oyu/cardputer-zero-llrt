//! Framebuffer access for the 320x170 ST7789V RGB565 (little-endian) LCD.
//!
//! Supports both real character devices (`/dev/fb_lcd`, `/dev/fb0`) via mmap + ioctl + fsync
//! and regular-file mocks used by deterministic test harnesses.

use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};

use crate::env_non_empty;

/// Display width in pixels.
pub const SCREEN_W: usize = 320;
/// Display height in pixels.
pub const SCREEN_H: usize = 170;
/// Total framebuffer size in bytes (RGB565 little-endian).
pub const FB_BYTES: usize = SCREEN_W * SCREEN_H * 2;

//FBIOGET_VSCREENINFO/FSCREENINFO are encoded with the _IOC read convention (magic 'F').
const FBIOGET_VSCREENINFO: libc::c_ulong = 0x4600;
const FBIOGET_FSCREENINFO: libc::c_ulong = 0x4602;

/// Mirror of kernel `struct fb_bitfield`.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
struct FbBitfield {
    offset: u32,
    length: u32,
    msb_right: u32,
}

/// Mirror of kernel `struct fb_var_screeninfo` (uapi layout, linux/fb.h).
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
#[allow(dead_code)]
struct FbVarScreeninfo {
    xres: u32,
    yres: u32,
    xres_virtual: u32,
    yres_virtual: u32,
    xoffset: u32,
    yoffset: u32,
    bits_per_pixel: u32,
    grayscale: u32,
    red: FbBitfield,
    green: FbBitfield,
    blue: FbBitfield,
    transp: FbBitfield,
    nonstd: u32,
    activate: u32,
    height: u32,
    width: u32,
    accel_flags: u32,
    pixclock: u32,
    left_margin: u32,
    right_margin: u32,
    upper_margin: u32,
    lower_margin: u32,
    hsync_len: u32,
    vsync_len: u32,
    sync: u32,
    vmode: u32,
    rotate: u32,
    colorspace: u32,
    reserved: [u32; 4],
}

/// Mirror of kernel `struct fb_fix_screeninfo` (uapi layout, linux/fb.h).
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
#[allow(dead_code)]
struct FbFixScreeninfo {
    id: [u8; 16],
    smem_start: libc::c_ulong,
    smem_len: u32,
    r#type: u32,
    type_aux: u32,
    visual: u32,
    xpanstep: u16,
    ypanstep: u16,
    ywrapstep: u16,
    line_length: u32,
    mmio_start: libc::c_ulong,
    mmio_len: u32,
    accel: u32,
    capabilities: u16,
    reserved: [u16; 2],
}

/// Resolve the framebuffer path:
/// 1. `CARDPUTER_FB_PATH` when set and non-empty.
/// 2. `/dev/fb_lcd` when it exists.
/// 3. `/dev/fb0` as a fallback.
pub fn resolve_fb_path() -> PathBuf {
    if let Some(p) = env_non_empty("CARDPUTER_FB_PATH") {
        return PathBuf::from(p);
    }
    let lcd = Path::new("/dev/fb_lcd");
    if lcd.exists() {
        lcd.to_owned()
    } else {
        PathBuf::from("/dev/fb0")
    }
}

enum Backend {
    /// Memory-mapped character device with initial contents preserved for restore-on-Drop.
    CharDevice {
        fd: OwnedFd,
        mapped: *mut u8,
        saved: Vec<u8>,
    },
    /// Regular file mock: flushed directly and intentionally NOT restored on Drop so test
    /// harnesses can inspect the flushed bytes after process exit.
    RegularFile { file: File },
}

/// An open framebuffer handle (lazy-opened by callers; opening here touches the device).
pub struct Framebuffer {
    path: PathBuf,
    backend: Backend,
}

impl std::fmt::Debug for Framebuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Framebuffer")
            .field("path", &self.path)
            .field("mock", &self.is_mock_file())
            .finish()
    }
}

// SAFETY: The framebuffer owns either an mmap'ed shared region or a plain file handle; all
// access through it is serialized by callers via `std::sync::Mutex`, never aliased concurrently.
unsafe impl Send for Framebuffer {}

impl Framebuffer {
    /// Open the default resolved path (`CARDPUTER_FB_PATH` / `/dev/fb_lcd` / `/dev/fb0`).
    pub fn open_default() -> io::Result<Self> {
        Self::open(&resolve_fb_path())
    }

    /// Open a specific framebuffer path with read+write access.
    ///
    /// Character devices are validated (`bpp == 16`), mmap'ed (`FB_BYTES`) and their initial
    /// contents saved for restoration on [`Drop`]. Regular files are sized to `FB_BYTES` and
    /// never restored on Drop. Anything else yields an error.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = match OpenOptions::new().read(true).write(true).open(path) {
            Ok(f) => f,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                // Phase 10B mock semantics: `CARDPUTER_FB_PATH` may name a not-yet-existing
                // regular file inside an EXISTING directory (harness tmp). Lazily create and
                // size it as a regular-file mock. Paths whose parent directory is missing
                // still fail with NotFound (device/early-open guard unchanged).
                let parent_ok = path.parent().map(|p| p.is_dir()).unwrap_or(false);
                if !parent_ok {
                    return Err(err);
                }
                let created = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(path)?;
                created.set_len(FB_BYTES as u64)?;
                created
            }
            Err(err) => return Err(err),
        };
        let file_type = file.metadata()?.file_type();

        if file_type.is_char_device() {
            let fd = unsafe { OwnedFd::from_raw_fd(file.into_raw_fd()) };
            let mut vsi: FbVarScreeninfo = Default::default();
            let rc = unsafe {
                libc::ioctl(
                    fd.as_raw_fd(),
                    FBIOGET_VSCREENINFO as libc::Ioctl,
                    &mut vsi as *mut FbVarScreeninfo,
                )
            };
            if rc != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut fsi: FbFixScreeninfo = Default::default();
            let rc = unsafe {
                libc::ioctl(
                    fd.as_raw_fd(),
                    FBIOGET_FSCREENINFO as libc::Ioctl,
                    &mut fsi as *mut FbFixScreeninfo,
                )
            };
            if rc != 0 {
                return Err(io::Error::last_os_error());
            }
            if vsi.bits_per_pixel != 16 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "framebuffer at {} has bpp {}, expected 16 (RGB565)",
                        path.display(),
                        vsi.bits_per_pixel
                    ),
                ));
            }
            let ptr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    FB_BYTES,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    fd.as_raw_fd(),
                    0,
                )
            };
            if ptr == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            let mapped = ptr as *mut u8;
            let saved =
                unsafe { std::slice::from_raw_parts(mapped, FB_BYTES) }.to_vec();
            Ok(Self {
                path: path.to_owned(),
                backend: Backend::CharDevice { fd, mapped, saved },
            })
        } else if file_type.is_file() {
            // Ensure the mock file is exactly FB_BYTES long so offsets line up with pixels.
            let len = file.metadata()?.len();
            if len != FB_BYTES as u64 {
                file.set_len(FB_BYTES as u64)?;
            }
            Ok(Self {
                path: path.to_owned(),
                backend: Backend::RegularFile { file },
            })
        } else {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("{} is neither a character device nor a regular file", path.display()),
            ))
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether this backend is a regular-file mock (no restore-on-Drop).
    pub fn is_mock_file(&self) -> bool {
        matches!(self.backend, Backend::RegularFile { .. })
    }

    /// Write one full frame (`FB_BYTES` of little-endian RGB565) and sync it to the device.
    pub fn flush_frame(&mut self, data: &[u8]) -> io::Result<()> {
        if data.len() != FB_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("frame must be {} bytes, got {}", FB_BYTES, data.len()),
            ));
        }
        match &mut self.backend {
            Backend::CharDevice { mapped, fd, .. } => {
                unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), *mapped, FB_BYTES) };
                let rc = unsafe { libc::fsync(fd.as_raw_fd()) };
                if rc != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            }
            Backend::RegularFile { file } => {
                file.seek(SeekFrom::Start(0))?;
                file.write_all(data)?;
                file.sync_data()?;
                Ok(())
            }
        }
    }
}

impl Drop for Framebuffer {
    fn drop(&mut self) {
        match &self.backend {
            Backend::CharDevice { fd, mapped, saved } => {
                // Restore the initial contents captured at open time.
                let mut offset = 0usize;
                while offset < saved.len() {
                    let chunk = &saved[offset..];
                    let rc = unsafe {
                        libc::pwrite(
                            fd.as_raw_fd(),
                            chunk.as_ptr() as *const libc::c_void,
                            chunk.len(),
                            offset as i64,
                        )
                    };
                    if rc <= 0 {
                        break;
                    }
                    offset += rc as usize;
                }
                let _ = unsafe { libc::fsync(fd.as_raw_fd()) };
                let _ = unsafe { libc::munmap(*mapped as *mut libc::c_void, FB_BYTES) };
                // The OwnedFd closes itself when dropped.
            }
            Backend::RegularFile { .. } => {
                // Intentionally do NOT restore regular files: test harnesses read them back
                // after process exit to inspect what was flushed.
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cz-hal-fb-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("create tmp dir");
        dir.join(name)
    }

    #[test]
    fn mock_file_flush_roundtrip_and_no_restore_on_drop() {
        let path = temp_path("fb0-mock");
        fs::write(&path, vec![0u8; FB_BYTES]).expect("seed fb");

        {
            let mut fb = Framebuffer::open(&path).expect("open mock fb");
            assert!(fb.is_mock_file());
            let frame = vec![0xABu8; FB_BYTES];
            fb.flush_frame(&frame).expect("flush frame");

            // While still open the file must contain the flushed pattern.
            let during = fs::read(&path).expect("read during");
            assert!(during.iter().all(|&b| b == 0xAB));
        }

        // After Drop (regular-file backend): contents must remain the flushed frame, NOT the
        // seed/initial contents.
        let after = fs::read(&path).expect("read after drop");
        assert_eq!(after.len(), FB_BYTES);
        assert!(after.iter().all(|&b| b == 0xAB));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn flush_rejects_wrong_frame_size() {
        let path = temp_path("fb0-size");
        fs::write(&path, vec![0u8; FB_BYTES]).expect("seed fb");
        let mut fb = Framebuffer::open(&path).expect("open");
        let err = fb.flush_frame(&[1u8; 4]).expect_err("short frame must error");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn open_expands_short_mock_file() {
        let path = temp_path("fb0-short");
        fs::write(&path, vec![7u8; 16]).expect("tiny seed");
        let fb = Framebuffer::open(&path).expect("open short mock");
        assert_eq!(fs::metadata(&path).expect("meta").len(), FB_BYTES as u64);
        assert!(fb.is_mock_file());
        fs::remove_file(&path).ok();
    }

    #[test]
    fn missing_path_fails_cleanly() {
        // A path whose parent DIRECTORY is absent must stay NotFound (never auto-created).
        let dir = temp_path("definitely-missing-fb-dir");
        fs::remove_dir_all(&dir).ok();
        let nested = dir.join("fb0");
        let err = Framebuffer::open(&nested).expect_err("missing parent dir must not panic");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(!nested.exists(), "nothing may be created when the parent dir is absent");
    }

    #[test]
    fn env_mock_file_is_lazily_created_and_sized_on_open() {
        // Phase 10B: an explicit mock path in an existing directory is created on first open.
        let path = temp_path("fb0-lazy-create");
        fs::remove_file(&path).ok();
        let mut fb = Framebuffer::open(&path).expect("lazy create must succeed");
        assert!(fb.is_mock_file());
        assert_eq!(fs::metadata(&path).expect("meta").len(), FB_BYTES as u64);
        // Seed canvas frame bytes and flush through the created mock end to end.
        fb.flush_frame(&vec![0xABu8; FB_BYTES]).expect("flush fresh mock");
        assert_eq!(fs::read(&path).expect("read mock").len(), FB_BYTES);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn non_fb_char_device_reports_ioctl_error_without_panic() {
        // /dev/null is a character device but not a framebuffer: ioctl lookups must fail
        // cleanly (ENOTTY) rather than panicking.
        if !Path::new("/dev/null").exists() {
            return;
        }
        let res = Framebuffer::open(Path::new("/dev/null"));
        match res {
            Err(e) => assert_ne!(e.kind(), io::ErrorKind::Unsupported),
            Ok(_) => panic!("expected /dev/null to be rejected as a framebuffer"),
        }
    }

    #[test]
    fn env_var_overrides_fb_resolution() {
        let marker = temp_path("env-override-fb");
        std::env::set_var("CARDPUTER_FB_PATH", &marker);
        assert_eq!(resolve_fb_path(), marker);
        // Empty means "unset".
        std::env::set_var("CARDPUTER_FB_PATH", "");
        let fallback = resolve_fb_path();
        assert!(fallback == std::path::Path::new("/dev/fb0") || fallback == std::path::Path::new("/dev/fb_lcd"));
        std::env::remove_var("CARDPUTER_FB_PATH");
    }

    #[test]
    fn dimensions_constants_are_exact() {
        assert_eq!(SCREEN_W, 320);
        assert_eq!(SCREEN_H, 170);
        assert_eq!(FB_BYTES, 108_800);
    }
}
