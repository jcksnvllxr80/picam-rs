use std::fs::File;
use std::io::{BufReader, Read, Write};
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

// Resolution used for the live preview subprocess.
// Also used for recording (tee'd to file, then transcoded to MP4 by ffmpeg).
const PREVIEW_W: u32 = 1280;
const PREVIEW_H: u32 = 720;

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

pub const ISO_VALUES: &[&str] = &["0", "100", "200", "400", "800", "1600", "3200"];
pub const SHUTTER_SPEEDS: &[(&str, u64)] = &[
    ("0",       0),
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

impl CameraSettings {
    fn build_args(&self) -> Vec<String> {
        let mut args = vec![];
        let iso: u32 = ISO_VALUES[self.iso_idx].parse().unwrap_or(0);
        if iso > 0 {
            args.extend(["--gain".into(), (iso as f32 / 100.0).to_string()]);
        }
        let shutter_us = SHUTTER_SPEEDS[self.shutter_idx].1;
        if shutter_us > 0 {
            args.extend(["--shutter".into(), shutter_us.to_string()]);
        }
        args.extend(["--awb".into(), AWB_MODES[self.awb_idx].into()]);
        if self.ev.abs() > 0.01 {
            args.extend(["--ev".into(), format!("{:.2}", self.ev)]);
        }
        args.extend(["--contrast".into(),   format!("{:.2}", self.contrast)]);
        args.extend(["--saturation".into(), format!("{:.2}", self.saturation)]);
        args.extend(["--sharpness".into(),  format!("{:.2}", self.sharpness)]);
        args.extend(["--brightness".into(), format!("{:.2}", self.brightness)]);
        if self.zoom > 1.01 {
            let w = 1.0 / self.zoom;
            let h = 1.0 / self.zoom;
            let x = (1.0 - w) / 2.0;
            let y = (1.0 - h) / 2.0;
            args.extend(["--roi".into(), format!("{x:.4},{y:.4},{w:.4},{h:.4}")]);
        }
        args
    }
}

// ── Messages from camera thread to UI thread ──────────────────────────────────
pub enum CameraEvent {
    Frame { rgba: Vec<u8>, width: u32, height: u32 },
}

// ── Recording state (written by start/stop, read by preview loop) ─────────────
struct RecordingState {
    file:      File,
    temp_path: PathBuf,   // raw MJPEG frames go here
    out_path:  PathBuf,   // final MP4 destination
    start:     Instant,
    framerate: u32,
}

// ── Camera ────────────────────────────────────────────────────────────────────
pub struct Camera {
    pub settings: Arc<Mutex<CameraSettings>>,
    frame_tx:     Sender<CameraEvent>,
    pub frame_rx: Receiver<CameraEvent>,
    running:      Arc<AtomicBool>,
    paused:       Arc<AtomicBool>,   // paused during still/timelapse captures
    recording:    Arc<Mutex<Option<RecordingState>>>,
}

impl Camera {
    pub fn new() -> Self {
        let (tx, rx) = bounded(2);
        Self {
            settings:  Arc::new(Mutex::new(CameraSettings::default())),
            frame_tx:  tx,
            frame_rx:  rx,
            running:   Arc::new(AtomicBool::new(true)),
            paused:    Arc::new(AtomicBool::new(false)),
            recording: Arc::new(Mutex::new(None)),
        }
    }

    pub fn start_preview_thread(&self) {
        let settings  = Arc::clone(&self.settings);
        let tx        = self.frame_tx.clone();
        let running   = Arc::clone(&self.running);
        let paused    = Arc::clone(&self.paused);
        let recording = Arc::clone(&self.recording);

        for d in [SAVE_DIR, VIDEO_DIR, TL_DIR] {
            let _ = std::fs::create_dir_all(d);
        }

        thread::Builder::new()
            .name("camera-preview".into())
            .spawn(move || preview_loop(settings, tx, running, paused, recording))
            .expect("spawn camera thread");
    }

    pub fn update_settings(&self, s: CameraSettings) {
        *self.settings.lock().unwrap() = s;
    }

    fn pause_preview(&self) {
        self.paused.store(true, Ordering::Relaxed);
        thread::sleep(Duration::from_millis(600));
    }

    fn resume_preview(&self) {
        self.paused.store(false, Ordering::Relaxed);
    }

    // ── Photo capture (pauses preview while rpicam-still runs) ────────────────
    pub fn capture_photo(&self) -> Result<PathBuf> {
        if self.is_recording() {
            anyhow::bail!("Stop recording before taking a photo");
        }
        let ts   = timestamp_ms();
        let path = PathBuf::from(SAVE_DIR).join(format!("IMG_{ts}.jpg"));
        let s    = self.settings.lock().unwrap().clone();

        self.pause_preview();
        let mut cmd = Command::new("rpicam-still");
        cmd.args([
            "--output", path.to_str().unwrap(),
            "--timeout", "200",
            "--rotation", "180",
            "--width",  "4056",
            "--height", "3040",
            "--nopreview",
            "--immediate",
        ]);
        for a in s.build_args() { cmd.arg(a); }
        let result = cmd.status().context("rpicam-still failed");
        self.resume_preview();

        let status = result?;
        if !status.success() { anyhow::bail!("rpicam-still exited {:?}", status.code()); }
        Ok(path)
    }

    // ── Video recording: tee MJPEG frames from the running preview stream ──────
    pub fn start_recording(&self) -> Result<PathBuf> {
        let ts        = timestamp_ms();
        let temp_path = PathBuf::from(VIDEO_DIR).join(format!(".rec_{ts}.mjpeg"));
        let out_path  = PathBuf::from(VIDEO_DIR).join(format!("VID_{ts}.mp4"));

        let file = File::create(&temp_path)
            .with_context(|| format!("create temp recording {temp_path:?}"))?;

        *self.recording.lock().unwrap() = Some(RecordingState {
            file,
            temp_path,
            out_path: out_path.clone(),
            start: Instant::now(),
            framerate: 25,
        });
        Ok(out_path)
    }

    /// Stop recording and transcode in the background.
    /// `on_done` is called from a worker thread when the MP4 is ready.
    pub fn stop_recording(&self, on_done: impl FnOnce() + Send + 'static) -> Option<PathBuf> {
        let state = self.recording.lock().unwrap().take()?;
        let framerate  = state.framerate;
        let temp_path  = state.temp_path.clone();
        let out_path   = state.out_path.clone();
        drop(state); // flush + close the temp file

        let tp = temp_path.clone();
        let op = out_path.clone();
        thread::spawn(move || {
            let status = Command::new("ffmpeg")
                .args([
                    "-y",
                    "-framerate", &framerate.to_string(),
                    "-f", "mjpeg",
                    "-i", tp.to_str().unwrap(),
                    "-c:v", "libx264",
                    "-preset", "fast",
                    "-pix_fmt", "yuv420p",
                    op.to_str().unwrap(),
                ])
                .status();
            match status {
                Ok(s) if s.success() => { let _ = std::fs::remove_file(&tp); }
                _ => eprintln!("[rec] ffmpeg transcode failed, keeping {tp:?}"),
            }
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

    // ── Timelapse frame ────────────────────────────────────────────────────────
    pub fn capture_timelapse_frame(&self, index: usize, session_dir: &str) -> Result<PathBuf> {
        let path = PathBuf::from(session_dir).join(format!("frame_{index:06}.jpg"));
        let s    = self.settings.lock().unwrap().clone();

        self.pause_preview();
        let mut cmd = Command::new("rpicam-still");
        cmd.args([
            "--output", path.to_str().unwrap(),
            "--timeout", "200",
            "--rotation", "180",
            "--width",  "2028",
            "--height", "1520",
            "--nopreview",
            "--immediate",
        ]);
        for a in s.build_args() { cmd.arg(a); }
        let result = cmd.status().context("rpicam-still timelapse failed");
        self.resume_preview();

        let status = result?;
        if !status.success() { anyhow::bail!("rpicam-still exited {:?}", status.code()); }
        Ok(path)
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
        self.stop_recording(|| {});
    }
}

// ── MJPEG preview loop ─────────────────────────────────────────────────────────
fn preview_loop(
    settings:  Arc<Mutex<CameraSettings>>,
    tx:        Sender<CameraEvent>,
    running:   Arc<AtomicBool>,
    paused:    Arc<AtomicBool>,
    recording: Arc<Mutex<Option<RecordingState>>>,
) {
    while running.load(Ordering::Relaxed) {
        if paused.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(100));
            continue;
        }
        match run_preview_child(&settings, &tx, &running, &paused, &recording) {
            Ok(_)  => {}
            Err(e) => {
                eprintln!("[camera] {e:#}");
                thread::sleep(Duration::from_secs(2));
            }
        }
        if running.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(300));
        }
    }
}

fn run_preview_child(
    settings:  &Arc<Mutex<CameraSettings>>,
    tx:        &Sender<CameraEvent>,
    running:   &Arc<AtomicBool>,
    paused:    &Arc<AtomicBool>,
    recording: &Arc<Mutex<Option<RecordingState>>>,
) -> Result<()> {
    let s = settings.lock().unwrap().clone();

    let mut cmd = Command::new("rpicam-vid");
    cmd.args([
        "--output", "-",
        "--timeout", "0",
        "--rotation", "180",
        "--width",  &PREVIEW_W.to_string(),
        "--height", &PREVIEW_H.to_string(),
        "--framerate", "25",
        "--codec", "mjpeg",
        "--nopreview",
        "--flush",
    ]);
    for a in s.build_args() { cmd.arg(a); }
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());

    let mut child  = cmd.spawn().context("rpicam-vid spawn")?;
    let stdout     = child.stdout.take().expect("stdout piped");
    let mut reader = BufReader::with_capacity(512 * 1024, stdout);

    let mut buf:     Vec<u8> = Vec::with_capacity(512 * 1024);
    let mut scratch: Vec<u8> = vec![0u8; 65536];
    let mut soi_pos: Option<usize> = None;

    while running.load(Ordering::Relaxed) && !paused.load(Ordering::Relaxed) {
        let n = match reader.read(&mut scratch) {
            Ok(0)  => break,
            Ok(n)  => n,
            Err(_) => break,
        };
        buf.extend_from_slice(&scratch[..n]);

        loop {
            // Locate SOI marker
            if soi_pos.is_none() {
                if let Some(i) = find_marker(&buf, 0xFF, 0xD8) {
                    if i > 0 { buf.drain(..i); }
                    soi_pos = Some(0);
                } else {
                    if buf.len() > 4 { buf.drain(..buf.len() - 4); }
                    break;
                }
            }

            // Locate EOI marker
            let start = soi_pos.unwrap();
            if let Some(eoi) = find_marker_from(&buf, 0xFF, 0xD9, start + 2) {
                let end  = eoi + 2;
                let jpeg = buf[start..end].to_vec();
                buf.drain(..end);
                soi_pos = None;

                // ── Tee: write raw JPEG to recording file if active ────────
                if let Ok(mut rec) = recording.try_lock() {
                    if let Some(ref mut state) = *rec {
                        let _ = state.file.write_all(&jpeg);
                    }
                }

                // ── Decode for live preview display ────────────────────────
                if let Ok(rgba) = decode_jpeg_rgba(&jpeg) {
                    let _ = tx.try_send(CameraEvent::Frame {
                        rgba,
                        width:  PREVIEW_W,
                        height: PREVIEW_H,
                    });
                }
            } else {
                break;
            }
        }

        // Restart subprocess if settings changed
        let new_s = settings.lock().unwrap().clone();
        if settings_changed(&s, &new_s) { break; }
    }

    let _ = child.kill();
    let _ = child.wait();
    Ok(())
}

fn settings_changed(a: &CameraSettings, b: &CameraSettings) -> bool {
    a.iso_idx != b.iso_idx
        || a.shutter_idx != b.shutter_idx
        || a.awb_idx     != b.awb_idx
        || (a.ev         - b.ev).abs()         > 0.01
        || (a.contrast   - b.contrast).abs()   > 0.01
        || (a.saturation - b.saturation).abs() > 0.01
        || (a.sharpness  - b.sharpness).abs()  > 0.01
        || (a.brightness - b.brightness).abs() > 0.01
        || (a.zoom       - b.zoom).abs()        > 0.01
}

fn find_marker(buf: &[u8], b0: u8, b1: u8) -> Option<usize> {
    buf.windows(2).position(|w| w[0] == b0 && w[1] == b1)
}

fn find_marker_from(buf: &[u8], b0: u8, b1: u8, from: usize) -> Option<usize> {
    if from >= buf.len().saturating_sub(1) { return None; }
    buf[from..].windows(2).position(|w| w[0] == b0 && w[1] == b1).map(|p| p + from)
}

fn decode_jpeg_rgba(jpeg: &[u8]) -> Result<Vec<u8>> {
    let img  = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)?;
    let rgba = img.into_rgba8();
    Ok(rgba.into_raw())
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

    // ── find_marker ────────────────────────────────────────────────────────────

    #[test]
    fn find_marker_finds_at_start() {
        let buf = [0xFF, 0xD8, 0x00, 0x01];
        assert_eq!(find_marker(&buf, 0xFF, 0xD8), Some(0));
    }

    #[test]
    fn find_marker_finds_in_middle() {
        let buf = [0x00, 0x01, 0xFF, 0xD9, 0x00];
        assert_eq!(find_marker(&buf, 0xFF, 0xD9), Some(2));
    }

    #[test]
    fn find_marker_returns_none_when_absent() {
        let buf = [0x00, 0x01, 0x02, 0x03];
        assert_eq!(find_marker(&buf, 0xFF, 0xD8), None);
    }

    #[test]
    fn find_marker_from_skips_before_offset() {
        let buf = [0xFF, 0xD9, 0x00, 0xFF, 0xD9];
        // skip the first occurrence at 0, find the one at 3
        assert_eq!(find_marker_from(&buf, 0xFF, 0xD9, 2), Some(3));
    }

    #[test]
    fn find_marker_from_offset_past_end_returns_none() {
        let buf = [0xFF, 0xD9];
        assert_eq!(find_marker_from(&buf, 0xFF, 0xD9, 10), None);
    }

    // ── settings_changed ──────────────────────────────────────────────────────

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
        b.ev = 0.005; // below 0.01 threshold
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
        let a = CameraSettings::default(); // zoom = 1.0
        let mut b = a.clone();
        b.zoom = 1.005; // within 0.01
        assert!(!settings_changed(&a, &b));
        b.zoom = 1.02;
        assert!(settings_changed(&a, &b));
    }

    // ── CameraSettings::build_args ────────────────────────────────────────────

    #[test]
    fn build_args_auto_iso_omits_gain() {
        let s = CameraSettings::default(); // iso_idx = 0 → "0"
        let args = s.build_args();
        assert!(!args.contains(&"--gain".to_string()));
    }

    #[test]
    fn build_args_iso_100_sets_gain_1() {
        let mut s = CameraSettings::default();
        s.iso_idx = 1; // ISO_VALUES[1] = "100"
        let args = s.build_args();
        let idx = args.iter().position(|a| a == "--gain").unwrap();
        assert_eq!(args[idx + 1], "1"); // 100/100 = 1
    }

    #[test]
    fn build_args_auto_shutter_omits_flag() {
        let s = CameraSettings::default(); // shutter_idx = 0 → 0µs
        let args = s.build_args();
        assert!(!args.contains(&"--shutter".to_string()));
    }

    #[test]
    fn build_args_shutter_sets_microseconds() {
        let mut s = CameraSettings::default();
        s.shutter_idx = 1; // SHUTTER_SPEEDS[1] = ("1/4000", 250)
        let args = s.build_args();
        let idx = args.iter().position(|a| a == "--shutter").unwrap();
        assert_eq!(args[idx + 1], "250");
    }

    #[test]
    fn build_args_no_zoom_omits_roi() {
        let s = CameraSettings::default(); // zoom = 1.0
        let args = s.build_args();
        assert!(!args.contains(&"--roi".to_string()));
    }

    #[test]
    fn build_args_zoom_sets_roi() {
        let mut s = CameraSettings::default();
        s.zoom = 2.0;
        let args = s.build_args();
        assert!(args.contains(&"--roi".to_string()));
        // w = h = 0.5, x = y = 0.25
        let idx = args.iter().position(|a| a == "--roi").unwrap();
        assert_eq!(args[idx + 1], "0.2500,0.2500,0.5000,0.5000");
    }

    #[test]
    fn build_args_ev_zero_omits_flag() {
        let s = CameraSettings::default(); // ev = 0.0
        let args = s.build_args();
        assert!(!args.contains(&"--ev".to_string()));
    }

    #[test]
    fn build_args_ev_nonzero_sets_flag() {
        let mut s = CameraSettings::default();
        s.ev = 1.5;
        let args = s.build_args();
        assert!(args.contains(&"--ev".to_string()));
    }
}
