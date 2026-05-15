//! Touchscreen backlight on/off control.
//!
//! The Pi 7" DSI touchscreen exposes its backlight under /sys/class/backlight/
//! (typically `10-0045` or `rpi_backlight` depending on kernel/dtoverlay).
//! Writing `0` to `brightness` cuts power to the LEDs entirely — saves power
//! and emits zero light during long astrophoto exposures.
//!
//! Permissions: the brightness sysfs file is root-owned by default. The .deb's
//! postinst drops a udev rule that chmods it 666 on boot. If picam was
//! installed from source, the user can do this manually or run as root.
//! If we can't write (EACCES), we log once and silently no-op.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

pub struct Backlight {
    brightness_path: PathBuf,
    max_brightness:  u32,
    warned:          AtomicBool,
}

impl Backlight {
    /// Auto-detect the first backlight device that has both `brightness` and
    /// `max_brightness` files. Returns None if no backlight is present
    /// (e.g. running the app off a Pi for development).
    pub fn detect() -> Option<Self> {
        let entries = fs::read_dir("/sys/class/backlight").ok()?;
        for entry in entries.flatten() {
            let path  = entry.path();
            let brightness_path = path.join("brightness");
            let max_path        = path.join("max_brightness");
            if brightness_path.exists() && max_path.exists() {
                let max_str = fs::read_to_string(&max_path).ok()?;
                let max     = max_str.trim().parse::<u32>().ok()?;
                eprintln!(
                    "[backlight] detected {} (max={})",
                    path.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
                    max
                );
                return Some(Self {
                    brightness_path,
                    max_brightness: max,
                    warned: AtomicBool::new(false),
                });
            }
        }
        eprintln!("[backlight] no backlight device found under /sys/class/backlight/");
        None
    }

    pub fn off(&self) {
        self.write("0");
    }

    pub fn on(&self) {
        self.write(&self.max_brightness.to_string());
    }

    fn write(&self, value: &str) {
        match fs::write(&self.brightness_path, value) {
            Ok(_) => {}
            Err(e) => {
                // Only complain once — the kiosk runs forever and we don't
                // want to spam the log on every screen-sleep transition.
                if !self.warned.swap(true, Ordering::Relaxed) {
                    eprintln!(
                        "[backlight] write({:?}) failed: {} — backlight control disabled",
                        self.brightness_path, e
                    );
                }
            }
        }
    }
}

impl Drop for Backlight {
    /// On graceful drop, always restore full brightness so we don't leave the
    /// touchscreen black after picam exits cleanly. Doesn't help SIGKILL —
    /// that's caught by the on-startup restore in main().
    fn drop(&mut self) {
        self.on();
    }
}
