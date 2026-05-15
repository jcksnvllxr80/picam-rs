//! User-editable TOML config at $HOME/.config/picam-rs/config.toml.
//!
//! Holds:
//!   - Default values for every in-camera setting (applied on startup)
//!   - Stream configuration (local HTTP MJPEG server port, push URL)
//!
//! If the file doesn't exist, picam writes a default one on first launch so the
//! user can edit it. Missing keys fall back to defaults — partial configs are
//! valid.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub camera:  CameraDefaults,
    pub capture: CaptureDefaults,
    pub general: GeneralDefaults,
    pub stream:  StreamConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CameraDefaults {
    pub iso_idx:     usize,
    pub shutter_idx: usize,
    pub awb_idx:     usize,
    pub ev:          f32,
    pub contrast:    f32,
    pub saturation:  f32,
    pub sharpness:   f32,
    pub brightness:  f32,
    pub zoom:        f32,
}

impl Default for CameraDefaults {
    fn default() -> Self {
        Self {
            iso_idx:     0,
            shutter_idx: 0,
            awb_idx:     0,
            ev:          0.0,
            contrast:    1.0,
            saturation:  1.0,
            sharpness:   1.0,
            brightness:  0.0,
            zoom:        1.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureDefaults {
    pub photo_res_idx:  usize,   // 0=12MP, 1=8MP, 2=5MP, 3=FHD
    pub video_res_idx:  usize,   // 0=1080p, 1=720p, 2=480p
    pub tl_res_idx:     usize,   // 0=3MP, 1=FHD, 2=720p
    pub raw_enabled:    bool,
    pub stream_enabled: bool,    // live preview on/off at startup
}

impl Default for CaptureDefaults {
    fn default() -> Self {
        Self {
            photo_res_idx:  0,
            video_res_idx:  0,
            tl_res_idx:     0,
            raw_enabled:    false,
            stream_enabled: true,   // preview ON by default
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralDefaults {
    pub self_timer_idx:    usize, // 0=off, 1=2s, 2=5s, 3=10s
    pub burst_count_idx:   usize, // 0=1, 1=3, 2=5, 3=10
    pub last_shot_enabled: bool,
    pub sleep_timeout_idx: usize, // 0=off, 1=30s, 2=1m, 3=5m, 4=10m, 5=30m
}

impl Default for GeneralDefaults {
    fn default() -> Self {
        Self {
            self_timer_idx:    0,
            burst_count_idx:   0,
            last_shot_enabled: true,
            sleep_timeout_idx: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamConfig {
    /// HTTP port for the local MJPEG server. Browser at http://<pi>:<port>/
    pub local_port: u16,
    /// Whether the local stream auto-starts on launch.
    pub local_enabled: bool,
    /// Destination URL for the push stream (e.g. rtmp://example.com/live/key).
    /// Empty disables push.
    pub push_url: String,
    /// Whether the push stream auto-starts on launch.
    pub push_enabled: bool,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            local_port:    8080,
            local_enabled: false,
            push_url:      String::new(),
            push_enabled:  false,
        }
    }
}

impl Config {
    fn path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/pi".into());
        PathBuf::from(home).join(".config/picam-rs/config.toml")
    }

    pub fn load() -> Self {
        let path = Self::path();
        match fs::read_to_string(&path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(cfg) => {
                    eprintln!("[config] loaded {}", path.display());
                    cfg
                }
                Err(e) => {
                    eprintln!("[config] parse error in {}: {e}; using defaults", path.display());
                    Self::default()
                }
            },
            Err(_) => {
                let cfg = Self::default();
                // Write the default config so the user has something to edit.
                if let Some(dir) = path.parent() {
                    let _ = fs::create_dir_all(dir);
                }
                if let Ok(text) = toml::to_string_pretty(&cfg) {
                    let _ = fs::write(&path, text);
                    eprintln!("[config] created defaults at {}", path.display());
                }
                cfg
            }
        }
    }
}
