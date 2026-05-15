//! Two ways to share the live preview off the device:
//!
//! - `LocalStream`: tiny embedded HTTP server that serves multipart MJPEG.
//!   Any browser at http://<pi>:<port>/ sees the live feed. No external deps.
//!
//! - `PushStream`: spawns ffmpeg with stdin pipe, transcodes NV12 to H264, and
//!   pushes to an RTMP/HTTP/whatever URL. Useful for streaming to YouTube,
//!   Twitch, a custom NGINX-RTMP server, etc.
//!
//! Both consume frames from the existing preview pipeline (800×480 NV12).
//! The conversion worker in `camera.rs` fans frames out to whichever streamer
//! is active.

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

// ── Local HTTP MJPEG server ──────────────────────────────────────────────────

/// Latest JPEG-encoded frame, shared with HTTP server worker threads.
pub type LatestFrame = Arc<Mutex<Option<Arc<Vec<u8>>>>>;

pub struct LocalStream {
    frame:   LatestFrame,
    running: Arc<AtomicBool>,
    port:    u16,
}

impl LocalStream {
    pub fn start(port: u16) -> Result<Self> {
        let listener = TcpListener::bind(format!("0.0.0.0:{}", port))
            .with_context(|| format!("bind 0.0.0.0:{}", port))?;
        listener.set_nonblocking(true)?;

        let frame:   LatestFrame  = Arc::new(Mutex::new(None));
        let running              = Arc::new(AtomicBool::new(true));

        let frame_c   = Arc::clone(&frame);
        let running_c = Arc::clone(&running);
        thread::Builder::new()
            .name("http-stream".into())
            .spawn(move || {
                while running_c.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _addr)) => {
                            let frame   = Arc::clone(&frame_c);
                            let running = Arc::clone(&running_c);
                            thread::spawn(move || serve_client(stream, frame, running));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(100));
                        }
                        Err(_) => break,
                    }
                }
                eprintln!("[stream] local HTTP listener exited");
            })
            .context("spawn http-stream")?;

        eprintln!("[stream] local MJPEG server listening on http://0.0.0.0:{}/", port);
        Ok(Self { frame, running, port })
    }

    pub fn port(&self) -> u16 { self.port }

    /// Replace the broadcast frame. Cheap — just stores an Arc.
    pub fn publish(&self, jpeg: Vec<u8>) {
        if let Ok(mut f) = self.frame.lock() {
            *f = Some(Arc::new(jpeg));
        }
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

impl Drop for LocalStream {
    fn drop(&mut self) {
        self.stop();
    }
}

fn serve_client(mut stream: TcpStream, frame: LatestFrame, running: Arc<AtomicBool>) {
    use std::io::Read;
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));

    // Read up to 1KB of the request to learn the path.
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).unwrap_or(0);
    let request = std::str::from_utf8(&buf[..n]).unwrap_or("");
    let path = request.lines().next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");

    if path == "/stream" || path == "/stream.mjpg" || path == "/mjpeg" {
        serve_mjpeg(stream, frame, running);
    } else {
        serve_index(stream);
    }
}

fn serve_index(mut stream: TcpStream) {
    // Modern browsers (Chrome 75+) refuse multipart/x-mixed-replace at the
    // top-level navigation. Wrapping it in <img src=...> still works.
    let html = "<!DOCTYPE html><html><head>\
        <title>picam-rs</title>\
        <style>html,body{margin:0;background:#000;height:100%;}\
        img{display:block;margin:0 auto;max-width:100%;max-height:100vh;}</style>\
        </head><body><img src=\"/stream\"></body></html>";
    let response = format!(
        "HTTP/1.0 200 OK\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-cache\r\n\
         Connection: close\r\n\r\n{}",
        html.len(), html
    );
    let _ = stream.write_all(response.as_bytes());
}

fn serve_mjpeg(mut stream: TcpStream, frame: LatestFrame, running: Arc<AtomicBool>) {
    const BOUNDARY: &str = "picamframe";
    let header = format!(
        "HTTP/1.0 200 OK\r\n\
         Cache-Control: no-cache, private\r\n\
         Pragma: no-cache\r\n\
         Connection: close\r\n\
         Content-Type: multipart/x-mixed-replace; boundary={BOUNDARY}\r\n\
         \r\n"
    );
    if stream.write_all(header.as_bytes()).is_err() { return; }

    let mut last_published: usize = 0;
    while running.load(Ordering::Relaxed) {
        let snapshot = frame.lock().ok().and_then(|f| f.clone());
        let Some(jpeg) = snapshot else {
            thread::sleep(Duration::from_millis(50));
            continue;
        };
        let id = Arc::as_ptr(&jpeg) as usize;
        if id == last_published {
            thread::sleep(Duration::from_millis(20));
            continue;
        }
        last_published = id;

        let part = format!(
            "--{}\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
            BOUNDARY, jpeg.len()
        );
        if stream.write_all(part.as_bytes()).is_err()
            || stream.write_all(&jpeg).is_err()
            || stream.write_all(b"\r\n").is_err()
        {
            break;
        }
    }
}

// ── ffmpeg push stream ───────────────────────────────────────────────────────

pub struct PushStream {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
}

impl PushStream {
    /// Start an ffmpeg subprocess that consumes NV12 from stdin and pushes
    /// H264 to `url`. Format is chosen by URL prefix.
    pub fn start(url: &str, width: u32, height: u32, fps: u32) -> Result<Self> {
        let s = format!("{}x{}", width, height);
        let r = fps.to_string();
        let format = pick_format(url);

        let mut child = Command::new("ffmpeg")
            .args([
                "-y",
                "-f", "rawvideo",
                "-pix_fmt", "nv12",
                "-s", &s,
                "-r", &r,
                "-i", "pipe:0",
                "-c:v", "libx264",
                "-preset", "ultrafast",
                "-tune", "zerolatency",
                "-pix_fmt", "yuv420p",
                "-f", format,
                url,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("ffmpeg push spawn (url={url})"))?;

        let stdin = child.stdin.take();
        eprintln!("[stream] push to {} started ({} {}x{}@{}fps)", url, format, width, height, fps);
        Ok(Self { child: Some(child), stdin })
    }

    /// Feed one frame's worth of NV12 bytes. Drops silently if the pipe is
    /// closed (ffmpeg crashed or we're shutting down).
    pub fn write_frame(&mut self, nv12: &[u8]) {
        if let Some(stdin) = self.stdin.as_mut() {
            let _ = stdin.write_all(nv12);
        }
    }

    pub fn stop(&mut self) {
        // Closing stdin causes ffmpeg to finalise and exit
        self.stdin.take();
        if let Some(mut child) = self.child.take() {
            // Give ffmpeg ~2 seconds, then kill if needed
            for _ in 0..20 {
                match child.try_wait() {
                    Ok(Some(_)) => return,
                    Ok(None)    => thread::sleep(Duration::from_millis(100)),
                    Err(_)      => break,
                }
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for PushStream {
    fn drop(&mut self) {
        self.stop();
    }
}

fn pick_format(url: &str) -> &'static str {
    let u = url.to_lowercase();
    if u.starts_with("rtmp://") || u.starts_with("rtmps://") { "flv" }
    else if u.starts_with("srt://")                          { "mpegts" }
    else if u.starts_with("udp://")                          { "mpegts" }
    else if u.starts_with("rtp://")                          { "rtp_mpegts" }
    else                                                     { "mpegts" }
}
