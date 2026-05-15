use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{bounded, Receiver, Sender};

pub const SAVE_DIR:  &str = "/home/pi/Pictures/picam";
pub const VIDEO_DIR: &str = "/home/pi/Videos/picam";
pub const TL_DIR:    &str = "/home/pi/Pictures/timelapse";

const PREVIEW_W: u32 = 800;
const PREVIEW_H: u32 = 480;
const RECORD_W:  u32 = 1920;
const RECORD_H:  u32 = 1080;

// Resolution presets exposed to the UI (idx → (w, h)).
// Photo defaults to full 12MP (IMX477 native). Video output is scaled by ffmpeg
// — the libcamera record stream is always 1920×1080 regardless of choice.
pub const PHOTO_RESOLUTIONS: &[(u32, u32, &str)] = &[
    (4056, 3040, "12MP"),
    (3280, 2464,  "8MP"),
    (2592, 1944,  "5MP"),
    (1920, 1080, "FHD"),
];
pub const VIDEO_RESOLUTIONS: &[(u32, u32, &str)] = &[
    (1920, 1080, "1080p"),
    (1280,  720,  "720p"),
    ( 854,  480,  "480p"),
];
pub const TL_RESOLUTIONS: &[(u32, u32, &str)] = &[
    (2028, 1520,  "3MP"),
    (1920, 1080,  "FHD"),
    (1280,  720,  "720p"),
];

// ── FFI bindings ──────────────────────────────────────────────────────────────

#[repr(C)]
struct PicamHandle(std::ffi::c_void);

type FrameCb = unsafe extern "C" fn(
    data:   *const u8,
    len:    usize,
    width:  u32,
    height: u32,
    ctx:    *mut std::ffi::c_void,
);

#[link(name = "camera_ffi", kind = "static")]
#[link(name = "camera")]
#[link(name = "camera-base")]
extern "C" {
    fn picam_open(
        preview_w: u32, preview_h: u32,
        record_w:  u32, record_h:  u32,
        preview_cb: FrameCb,
        record_cb:  FrameCb,
        ctx: *mut std::ffi::c_void,
    ) -> *mut PicamHandle;

    fn picam_close(cam: *mut PicamHandle);
    fn picam_pause (cam: *mut PicamHandle);
    fn picam_resume(cam: *mut PicamHandle);

    fn picam_set_gain      (cam: *mut PicamHandle, gain: f32);
    fn picam_set_shutter   (cam: *mut PicamHandle, us: i32);
    fn picam_set_awb       (cam: *mut PicamHandle, mode_idx: i32);
    fn picam_set_ev        (cam: *mut PicamHandle, ev: f32);
    fn picam_set_contrast  (cam: *mut PicamHandle, v: f32);
    fn picam_set_saturation(cam: *mut PicamHandle, v: f32);
    fn picam_set_sharpness (cam: *mut PicamHandle, v: f32);
    fn picam_set_brightness(cam: *mut PicamHandle, v: f32);
    fn picam_set_roi       (cam: *mut PicamHandle, x: f32, y: f32, w: f32, h: f32);

    fn picam_start_recording(cam: *mut PicamHandle);
    fn picam_stop_recording (cam: *mut PicamHandle);
    fn picam_is_recording   (cam: *mut PicamHandle) -> i32;
}

// ── Camera settings ───────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct CameraSettings {
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

impl Default for CameraSettings {
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

pub const ISO_VALUES: &[&str]            = &["0", "100", "200", "400", "800", "1600", "3200"];
pub const SHUTTER_SPEEDS: &[(&str, i32)] = &[
    ("Auto",    0),
    ("1/4000",  250),
    ("1/1000",  1_000),
    ("1/500",   2_000),
    ("1/250",   4_000),
    ("1/60",    16_667),
    ("1/30",    33_333),
    ("1s",      1_000_000),
];
pub const AWB_MODES: &[&str] = &[
    "auto", "incandescent", "tungsten", "fluorescent", "indoor", "daylight", "cloudy",
];

// ── Camera events (preview frames) ───────────────────────────────────────────

pub enum CameraEvent {
    Frame { rgba: Vec<u8>, width: u32, height: u32 },
}

// ── Recording state ───────────────────────────────────────────────────────────

struct RecordingState {
    ffmpeg_stdin: std::process::ChildStdin,
    ffmpeg_child: std::process::Child,
    out_path:     PathBuf,
    start:        Instant,
}

// ── FFI callback context (heap-pinned, shared with C++) ───────────────────────

struct Nv12Frame {
    data:   Vec<u8>,
    width:  u32,
    height: u32,
}

struct CallbackCtx {
    // NV12 bytes shipped to a worker thread for conversion.
    // The libcamera completion thread MUST stay fast (memcpy + try_send only),
    // or the camera pipeline backs up and drops frames at the hardware level.
    nv12_tx:   Sender<Nv12Frame>,
    recording: Arc<Mutex<Option<RecordingState>>>,
}

unsafe extern "C" fn on_preview_frame(
    data: *const u8, len: usize, width: u32, height: u32,
    ctx: *mut std::ffi::c_void,
) {
    let ctx = &*(ctx as *const CallbackCtx);
    let nv12 = std::slice::from_raw_parts(data, len);
    let frame = Nv12Frame { data: nv12.to_vec(), width, height };
    let _ = ctx.nv12_tx.try_send(frame);
}

unsafe extern "C" fn on_record_frame(
    data: *const u8, len: usize, _width: u32, _height: u32,
    ctx: *mut std::ffi::c_void,
) {
    let ctx = &*(ctx as *const CallbackCtx);
    let nv12 = std::slice::from_raw_parts(data, len);
    if let Ok(mut rec) = ctx.recording.try_lock() {
        if let Some(ref mut state) = *rec {
            let _ = state.ffmpeg_stdin.write_all(nv12);
        }
    }
}

// ── NV12 → RGBA conversion ────────────────────────────────────────────────────

fn nv12_to_rgba(nv12: &[u8], width: u32, height: u32) -> Vec<u8> {
    let w = width  as usize;
    let h = height as usize;
    let y_plane  = &nv12[..w * h];
    let uv_plane = &nv12[w * h..];
    let mut rgba = vec![0u8; w * h * 4];

    for row in 0..h {
        for col in 0..w {
            let y  = y_plane[row * w + col] as i32;
            let uv = (row / 2) * w + (col & !1);
            let u  = uv_plane[uv]     as i32 - 128;
            let v  = uv_plane[uv + 1] as i32 - 128;

            let r = (y + 1_402 * v / 1_000).clamp(0, 255) as u8;
            let g = (y - 344 * u / 1_000 - 714 * v / 1_000).clamp(0, 255) as u8;
            let b = (y + 1_772 * u / 1_000).clamp(0, 255) as u8;

            let i = (row * w + col) * 4;
            rgba[i]     = r;
            rgba[i + 1] = g;
            rgba[i + 2] = b;
            rgba[i + 3] = 255;
        }
    }
    rgba
}

// ── Camera ────────────────────────────────────────────────────────────────────

pub struct Camera {
    pub settings: Arc<Mutex<CameraSettings>>,
    pub frame_rx: Receiver<CameraEvent>,
    handle:       *mut PicamHandle,
    recording:    Arc<Mutex<Option<RecordingState>>>,
    paused:       Arc<AtomicBool>,
    // User-controlled — true when the user wants live preview on.
    // Differs from `paused`, which is set briefly during captures regardless of
    // user preference. capture_photo() must NOT auto-resume if the user
    // disabled the stream.
    stream_enabled: Arc<AtomicBool>,
    // keeps callback context alive for the lifetime of the camera
    _ctx:         Box<CallbackCtx>,
}

// SAFETY: PicamHandle is accessed only through the C FFI which is internally
// synchronized via mutexes. The callbacks fire on a libcamera thread but only
// access the separately-synchronized CallbackCtx fields.
unsafe impl Send for Camera {}
unsafe impl Sync for Camera {}

impl Camera {
    pub fn new() -> Self {
        for d in [SAVE_DIR, VIDEO_DIR, TL_DIR] {
            let _ = std::fs::create_dir_all(d);
        }

        // Two channels:
        //   nv12: libcamera completion thread → conversion worker
        //   frame: conversion worker → Slint event loop
        // Bounded(2) on each so backpressure naturally drops frames if a stage
        // can't keep up, rather than queueing memory indefinitely.
        let (nv12_tx, nv12_rx) = bounded::<Nv12Frame>(2);
        let (frame_tx, frame_rx) = bounded::<CameraEvent>(2);
        let recording = Arc::new(Mutex::new(None::<RecordingState>));

        // Conversion worker: takes raw NV12, produces RGBA8, ships to UI.
        thread::Builder::new()
            .name("nv12-rgba".into())
            .spawn(move || {
                while let Ok(frame) = nv12_rx.recv() {
                    let rgba = nv12_to_rgba(&frame.data, frame.width, frame.height);
                    let _ = frame_tx.try_send(CameraEvent::Frame {
                        rgba,
                        width:  frame.width,
                        height: frame.height,
                    });
                }
            })
            .expect("spawn nv12-rgba worker");

        let ctx = Box::new(CallbackCtx {
            nv12_tx,
            recording: Arc::clone(&recording),
        });
        let ctx_ptr = &*ctx as *const CallbackCtx as *mut std::ffi::c_void;

        let handle = unsafe {
            picam_open(
                PREVIEW_W, PREVIEW_H,
                RECORD_W,  RECORD_H,
                on_preview_frame,
                on_record_frame,
                ctx_ptr,
            )
        };

        if handle.is_null() {
            panic!("[camera] picam_open failed — is the camera connected?");
        }

        Camera {
            settings: Arc::new(Mutex::new(CameraSettings::default())),
            frame_rx,
            handle,
            recording,
            paused: Arc::new(AtomicBool::new(false)),
            stream_enabled: Arc::new(AtomicBool::new(true)),
            _ctx: ctx,
        }
    }

    pub fn set_stream_enabled(&self, on: bool) {
        let prev = self.stream_enabled.swap(on, Ordering::Relaxed);
        if prev == on { return; }
        if on {
            unsafe { picam_resume(self.handle); }
        } else {
            unsafe { picam_pause(self.handle); }
        }
    }

    pub fn is_stream_enabled(&self) -> bool {
        self.stream_enabled.load(Ordering::Relaxed)
    }

    pub fn update_settings(&self, s: CameraSettings) {
        let prev = {
            let mut lock = self.settings.lock().unwrap();
            let prev = lock.clone();
            *lock = s.clone();
            prev
        };
        if settings_changed(&prev, &s) {
            self.apply_settings(&s);
        }
    }

    fn apply_settings(&self, s: &CameraSettings) {
        let gain: f32 = ISO_VALUES[s.iso_idx].parse::<f32>().unwrap_or(0.0) / 100.0;
        unsafe { picam_set_gain(self.handle, gain); }

        let shutter_us = SHUTTER_SPEEDS[s.shutter_idx].1;
        unsafe { picam_set_shutter(self.handle, shutter_us); }

        unsafe { picam_set_awb(self.handle, s.awb_idx as i32); }

        if s.ev.abs() > 0.01 {
            unsafe { picam_set_ev(self.handle, s.ev); }
        }
        unsafe { picam_set_contrast  (self.handle, s.contrast);   }
        unsafe { picam_set_saturation(self.handle, s.saturation); }
        unsafe { picam_set_sharpness (self.handle, s.sharpness);  }
        unsafe { picam_set_brightness(self.handle, s.brightness); }

        if s.zoom > 1.01 {
            let w = 1.0 / s.zoom;
            let h = 1.0 / s.zoom;
            let x = (1.0 - w) / 2.0;
            let y = (1.0 - h) / 2.0;
            unsafe { picam_set_roi(self.handle, x, y, w, h); }
        } else {
            unsafe { picam_set_roi(self.handle, 0.0, 0.0, 1.0, 1.0); }
        }
    }

    // ── Photo capture ─────────────────────────────────────────────────────────

    pub fn capture_photo(&self, width: u32, height: u32) -> Result<PathBuf> {
        if self.is_recording() {
            anyhow::bail!("Stop recording before taking a photo");
        }
        let ts   = timestamp_ms();
        let path = PathBuf::from(SAVE_DIR).join(format!("IMG_{ts}.jpg"));
        let s    = self.settings.lock().unwrap().clone();
        let w_str = width.to_string();
        let h_str = height.to_string();

        self.pause_for_capture();
        let mut cmd = Command::new("rpicam-still");
        cmd.args([
            "--output", path.to_str().unwrap(),
            "--timeout", "200",
            "--rotation", "180",
            "--width",  &w_str,
            "--height", &h_str,
            "--nopreview",
            "--immediate",
        ]);
        for a in build_rpicam_args(&s) { cmd.arg(a); }
        let status = cmd.status().context("rpicam-still failed");
        self.resume_after_capture();

        let status = status?;
        if !status.success() { anyhow::bail!("rpicam-still exited {:?}", status.code()); }
        Ok(path)
    }

    // Used during photo/timelapse capture only — respects the user's
    // stream_enabled preference on resume (so we don't auto-enable preview
    // that the user explicitly disabled in Advanced settings).
    fn pause_for_capture(&self) {
        self.paused.store(true, Ordering::Relaxed);
        unsafe { picam_pause(self.handle); }
        // libcamera needs a beat to fully release the pipeline handler before
        // rpicam-still can acquire it. 500ms is conservative; could probably
        // drop to 300ms but we'd rather be reliable than fast for captures.
        thread::sleep(Duration::from_millis(500));
    }

    fn resume_after_capture(&self) {
        if self.stream_enabled.load(Ordering::Relaxed) {
            unsafe { picam_resume(self.handle); }
        }
        self.paused.store(false, Ordering::Relaxed);
    }

    // ── Video recording ───────────────────────────────────────────────────────

    pub fn start_recording(&self, out_width: u32, out_height: u32) -> Result<PathBuf> {
        let ts       = timestamp_ms();
        let out_path = PathBuf::from(VIDEO_DIR).join(format!("VID_{ts}.mp4"));

        // libcamera always streams at RECORD_W×RECORD_H (1080p). If the user
        // chose a smaller output, ffmpeg downscales via the scale filter.
        let input_size = format!("{}x{}", RECORD_W, RECORD_H);
        let scale_arg  = format!("scale={}:{}", out_width, out_height);
        let needs_scale = out_width != RECORD_W || out_height != RECORD_H;

        let mut args: Vec<&str> = vec![
            "-y",
            "-f",         "rawvideo",
            "-pix_fmt",   "nv12",
            "-s",         &input_size,
            "-r",         "25",
            "-i",         "pipe:0",
        ];
        if needs_scale {
            args.push("-vf");
            args.push(&scale_arg);
        }
        args.extend_from_slice(&[
            "-c:v",       "libx264",
            "-preset",    "fast",
            "-pix_fmt",   "yuv420p",
            out_path.to_str().unwrap(),
        ]);

        let mut child = Command::new("ffmpeg")
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("ffmpeg spawn failed")?;

        let stdin = child.stdin.take().expect("piped stdin");

        *self.recording.lock().unwrap() = Some(RecordingState {
            ffmpeg_stdin: stdin,
            ffmpeg_child: child,
            out_path: out_path.clone(),
            start: Instant::now(),
        });

        unsafe { picam_start_recording(self.handle); }
        Ok(out_path)
    }

    pub fn stop_recording(&self, on_done: impl FnOnce() + Send + 'static) -> Option<PathBuf> {
        unsafe { picam_stop_recording(self.handle); }

        let state = self.recording.lock().unwrap().take()?;
        let out_path = state.out_path.clone();

        // Close the ffmpeg stdin pipe so ffmpeg finalises the file, then wait
        // and extract a thumbnail JPEG (used as the gallery preview).
        thread::spawn(move || {
            drop(state.ffmpeg_stdin); // EOF → ffmpeg writes trailer and exits
            let _ = { let mut c = state.ffmpeg_child; c.wait() };

            let thumb_path = state.out_path.with_extension("thumb.jpg");
            let _ = Command::new("ffmpeg")
                .args([
                    "-y",
                    "-i", state.out_path.to_str().unwrap(),
                    "-vframes", "1",
                    "-q:v", "4",
                    thumb_path.to_str().unwrap(),
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();

            on_done();
        });

        Some(out_path)
    }

    pub fn is_recording(&self) -> bool {
        self.recording.lock().unwrap().is_some()
    }

    pub fn recording_duration(&self) -> Duration {
        self.recording.lock().unwrap()
            .as_ref()
            .map(|r| r.start.elapsed())
            .unwrap_or_default()
    }

    // ── Timelapse frame ───────────────────────────────────────────────────────

    pub fn capture_timelapse_frame(
        &self, index: usize, session_dir: &str, width: u32, height: u32,
    ) -> Result<PathBuf> {
        let path = PathBuf::from(session_dir).join(format!("frame_{index:06}.jpg"));
        let s    = self.settings.lock().unwrap().clone();
        let w_str = width.to_string();
        let h_str = height.to_string();

        self.pause_for_capture();
        let mut cmd = Command::new("rpicam-still");
        cmd.args([
            "--output", path.to_str().unwrap(),
            "--timeout", "200",
            "--rotation", "180",
            "--width",  &w_str,
            "--height", &h_str,
            "--nopreview",
            "--immediate",
        ]);
        for a in build_rpicam_args(&s) { cmd.arg(a); }
        let status = cmd.status().context("rpicam-still timelapse failed");
        self.resume_after_capture();

        let status = status?;
        if !status.success() { anyhow::bail!("rpicam-still exited {:?}", status.code()); }
        Ok(path)
    }

    pub fn stop(&self) {
        self.stop_recording(|| {});
        unsafe { picam_close(self.handle); }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn build_rpicam_args(s: &CameraSettings) -> Vec<String> {
    let mut args = vec![];
    let iso: u32 = ISO_VALUES[s.iso_idx].parse().unwrap_or(0);
    if iso > 0 {
        args.extend(["--gain".into(), (iso as f32 / 100.0).to_string()]);
    }
    let shutter_us = SHUTTER_SPEEDS[s.shutter_idx].1;
    if shutter_us > 0 {
        args.extend(["--shutter".into(), shutter_us.to_string()]);
    }
    args.extend(["--awb".into(), AWB_MODES[s.awb_idx].into()]);
    if s.ev.abs() > 0.01 {
        args.extend(["--ev".into(), format!("{:.2}", s.ev)]);
    }
    args.extend(["--contrast".into(),   format!("{:.2}", s.contrast)]);
    args.extend(["--saturation".into(), format!("{:.2}", s.saturation)]);
    args.extend(["--sharpness".into(),  format!("{:.2}", s.sharpness)]);
    args.extend(["--brightness".into(), format!("{:.2}", s.brightness)]);
    if s.zoom > 1.01 {
        let w = 1.0 / s.zoom;
        let h = 1.0 / s.zoom;
        let x = (1.0 - w) / 2.0;
        let y = (1.0 - h) / 2.0;
        args.extend(["--roi".into(), format!("{x:.4},{y:.4},{w:.4},{h:.4}")]);
    }
    args
}

fn settings_changed(a: &CameraSettings, b: &CameraSettings) -> bool {
    a.iso_idx     != b.iso_idx
        || a.shutter_idx != b.shutter_idx
        || a.awb_idx     != b.awb_idx
        || (a.ev         - b.ev).abs()         > 0.01
        || (a.contrast   - b.contrast).abs()   > 0.01
        || (a.saturation - b.saturation).abs() > 0.01
        || (a.sharpness  - b.sharpness).abs()  > 0.01
        || (a.brightness - b.brightness).abs() > 0.01
        || (a.zoom       - b.zoom).abs()        > 0.01
}

fn timestamp_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_changed_identical_returns_false() {
        let s = CameraSettings::default();
        assert!(!settings_changed(&s, &s.clone()));
    }

    #[test]
    fn settings_changed_iso_returns_true() {
        let a = CameraSettings::default();
        let mut b = a.clone();
        b.iso_idx = 2;
        assert!(settings_changed(&a, &b));
    }

    #[test]
    fn settings_changed_ev_small_delta_returns_false() {
        let a = CameraSettings::default();
        let mut b = a.clone();
        b.ev = 0.005;
        assert!(!settings_changed(&a, &b));
    }

    #[test]
    fn settings_changed_ev_large_delta_returns_true() {
        let a = CameraSettings::default();
        let mut b = a.clone();
        b.ev = 0.5;
        assert!(settings_changed(&a, &b));
    }

    #[test]
    fn settings_changed_zoom_threshold() {
        let a = CameraSettings::default();
        let mut b = a.clone();
        b.zoom = 1.005;
        assert!(!settings_changed(&a, &b));
        b.zoom = 1.02;
        assert!(settings_changed(&a, &b));
    }

    #[test]
    fn build_rpicam_args_auto_iso_omits_gain() {
        let s = CameraSettings::default();
        let args = build_rpicam_args(&s);
        assert!(!args.contains(&"--gain".to_string()));
    }

    #[test]
    fn build_rpicam_args_iso_100_sets_gain_1() {
        let mut s = CameraSettings::default();
        s.iso_idx = 1;
        let args = build_rpicam_args(&s);
        let idx = args.iter().position(|a| a == "--gain").unwrap();
        assert_eq!(args[idx + 1], "1");
    }

    #[test]
    fn build_rpicam_args_zoom_sets_roi() {
        let mut s = CameraSettings::default();
        s.zoom = 2.0;
        let args = build_rpicam_args(&s);
        assert!(args.contains(&"--roi".to_string()));
        let idx = args.iter().position(|a| a == "--roi").unwrap();
        assert_eq!(args[idx + 1], "0.2500,0.2500,0.5000,0.5000");
    }

    #[test]
    fn nv12_to_rgba_pure_black() {
        // Y=16, U=128, V=128 → near-black
        let w = 2u32; let h = 2u32;
        let mut nv12 = vec![16u8; (w * h) as usize];      // Y plane
        nv12.extend_from_slice(&[128u8, 128u8]);           // UV plane (one 2×2 block)
        let rgba = nv12_to_rgba(&nv12, w, h);
        assert_eq!(rgba.len(), (w * h * 4) as usize);
        // alpha channel must be 255
        assert!(rgba.iter().skip(3).step_by(4).all(|&a| a == 255));
    }

    #[test]
    fn nv12_to_rgba_output_size() {
        let w = 4u32; let h = 4u32;
        let nv12 = vec![128u8; (w * h + w * h / 2) as usize];
        let rgba = nv12_to_rgba(&nv12, w, h);
        assert_eq!(rgba.len(), (w * h * 4) as usize);
    }

    #[test]
    fn nv12_to_rgba_alpha_always_255() {
        let w = 8u32; let h = 8u32;
        let nv12 = vec![200u8; (w * h + w * h / 2) as usize];
        let rgba = nv12_to_rgba(&nv12, w, h);
        assert!(rgba.iter().skip(3).step_by(4).all(|&a| a == 255));
    }

    #[test]
    fn nv12_to_rgba_pure_white() {
        // Y=235 (broadcast white), U=128, V=128 → near-white RGB
        let w = 2u32; let h = 2u32;
        let mut nv12 = vec![235u8; (w * h) as usize];
        nv12.extend_from_slice(&[128u8, 128u8]);
        let rgba = nv12_to_rgba(&nv12, w, h);
        // All R, G, B channels should be > 200
        for i in 0..(w * h) as usize {
            assert!(rgba[i * 4]     > 200, "R too low");
            assert!(rgba[i * 4 + 1] > 200, "G too low");
            assert!(rgba[i * 4 + 2] > 200, "B too low");
        }
    }

    #[test]
    fn nv12_to_rgba_red_channel() {
        // Y=81, U=90, V=240 → approximately red (255, 0, 0) in studio swing
        let w = 2u32; let h = 2u32;
        let mut nv12 = vec![81u8; (w * h) as usize];
        nv12.extend_from_slice(&[90u8, 240u8]); // U=90, V=240
        let rgba = nv12_to_rgba(&nv12, w, h);
        // Red should be significantly higher than blue
        assert!(rgba[0] > rgba[2], "red channel should dominate");
    }

    #[test]
    fn build_rpicam_args_auto_shutter_omits_flag() {
        let s = CameraSettings::default();
        let args = build_rpicam_args(&s);
        assert!(!args.contains(&"--shutter".to_string()));
    }

    #[test]
    fn build_rpicam_args_shutter_sets_microseconds() {
        let mut s = CameraSettings::default();
        s.shutter_idx = 1; // SHUTTER_SPEEDS[1] = ("1/4000", 250)
        let args = build_rpicam_args(&s);
        let idx = args.iter().position(|a| a == "--shutter").unwrap();
        assert_eq!(args[idx + 1], "250");
    }

    #[test]
    fn build_rpicam_args_no_zoom_omits_roi() {
        let s = CameraSettings::default();
        let args = build_rpicam_args(&s);
        assert!(!args.contains(&"--roi".to_string()));
    }

    #[test]
    fn build_rpicam_args_ev_nonzero_sets_flag() {
        let mut s = CameraSettings::default();
        s.ev = 1.5;
        let args = build_rpicam_args(&s);
        assert!(args.contains(&"--ev".to_string()));
    }

    #[test]
    fn build_rpicam_args_ev_zero_omits_flag() {
        let s = CameraSettings::default();
        let args = build_rpicam_args(&s);
        assert!(!args.contains(&"--ev".to_string()));
    }
}
