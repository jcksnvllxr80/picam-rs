use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::camera::{Camera, TL_DIR};

pub struct Timelapse {
    pub frame_count: Arc<Mutex<usize>>,
    pub session_dir: Arc<Mutex<Option<String>>>,
    running:         Arc<AtomicBool>,
}

impl Timelapse {
    pub fn new() -> Self {
        Self {
            frame_count: Arc::new(Mutex::new(0)),
            session_dir: Arc::new(Mutex::new(None)),
            running:     Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    pub fn start(
        &self,
        camera: Arc<Camera>,
        interval_secs: u64,
        duration_secs: u64,
        frame_width:   u32,
        frame_height:  u32,
        on_frame: impl Fn(usize) + Send + 'static,
        on_done:  impl Fn() + Send + 'static,
    ) {
        if self.running.load(Ordering::Relaxed) { return; }

        let ts  = crate::timestamp_ms();
        let dir = format!("{TL_DIR}/session_{ts}");
        let _   = std::fs::create_dir_all(&dir);

        *self.frame_count.lock().unwrap() = 0;
        *self.session_dir.lock().unwrap() = Some(dir.clone());
        self.running.store(true, Ordering::Relaxed);

        let frame_count = Arc::clone(&self.frame_count);
        let running     = Arc::clone(&self.running);

        thread::Builder::new()
            .name("timelapse".into())
            .spawn(move || {
                let total_frames = (duration_secs / interval_secs.max(1)) as usize;
                for i in 0..total_frames {
                    if !running.load(Ordering::Relaxed) { break; }
                    if let Ok(path) = camera.capture_timelapse_frame(i, &dir, frame_width, frame_height) {
                        if path.exists() {
                            *frame_count.lock().unwrap() = i + 1;
                            on_frame(i + 1);
                        }
                    }
                    for _ in 0..(interval_secs * 10) {
                        if !running.load(Ordering::Relaxed) { break; }
                        thread::sleep(Duration::from_millis(100));
                    }
                }
                running.store(false, Ordering::Relaxed);
                on_done();
            })
            .expect("spawn timelapse");
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
    }

    pub fn current_frame_count(&self) -> usize {
        *self.frame_count.lock().unwrap()
    }

    pub fn render(&self) -> Option<PathBuf> {
        let dir = self.session_dir.lock().unwrap().clone()?;
        let ts  = crate::timestamp_ms();
        let out = format!("{TL_DIR}/timelapse_{ts}.mp4");

        let pattern = format!("{dir}/frame_%06d.jpg");
        let status = Command::new("ffmpeg")
            .args([
                "-y",
                "-framerate", "24",
                "-i", &pattern,
                "-vf", "scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:-1:-1",
                "-c:v", "libx264",
                "-pix_fmt", "yuv420p",
                &out,
            ])
            .status();

        match status {
            Ok(s) if s.success() => Some(PathBuf::from(out)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_timelapse_is_not_running() {
        let tl = Timelapse::new();
        assert!(!tl.is_running());
    }

    #[test]
    fn new_timelapse_frame_count_zero() {
        let tl = Timelapse::new();
        assert_eq!(tl.current_frame_count(), 0);
    }

    #[test]
    fn stop_on_idle_is_safe() {
        let tl = Timelapse::new();
        tl.stop(); // must not panic
        assert!(!tl.is_running());
    }

    #[test]
    fn render_without_session_returns_none() {
        let tl = Timelapse::new();
        // No session started → session_dir is None → render returns None
        assert!(tl.render().is_none());
    }

    #[test]
    fn total_frames_calculation() {
        // (duration / interval) gives total frame count — verify the formula used in start()
        let duration_secs: u64 = 60;
        let interval_secs: u64 = 10;
        let total = (duration_secs / interval_secs.max(1)) as usize;
        assert_eq!(total, 6);
    }

    #[test]
    fn total_frames_zero_interval_clamped() {
        // interval = 0 is clamped to 1 via .max(1) — must not divide by zero
        let duration_secs: u64 = 30;
        let interval_secs: u64 = 0;
        let total = (duration_secs / interval_secs.max(1)) as usize;
        assert_eq!(total, 30);
    }
}
