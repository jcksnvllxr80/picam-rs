//! Network surfaces served from the picam process.
//!
//! - **GET /**           HTML landing page with link to /control
//! - **GET /control**    Mobile web companion UI (Plex Mono, single-screen)
//! - **GET /stream**     Multipart MJPEG live preview (browser-friendly)
//! - **GET /api/state**  JSON: { recording, duration, free_space, ... }
//! - **POST /api/capture**       Capture a 12MP photo
//! - **POST /api/record/start**  Start 1080p H264 recording
//! - **POST /api/record/stop**   Stop recording
//!
//! Plus `PushStream`: an ffmpeg subprocess that pushes NV12 to an RTMP/SRT URL.

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::camera::{Camera, PHOTO_RESOLUTIONS, VIDEO_RESOLUTIONS, SAVE_DIR, VIDEO_DIR, TL_DIR, free_space_str};
use crate::gallery;

// ── Local HTTP server (MJPEG + /control web companion) ───────────────────────

/// Latest JPEG-encoded frame, shared with HTTP server worker threads.
pub type LatestFrame = Arc<Mutex<Option<Arc<Vec<u8>>>>>;

pub struct LocalStream {
    frame:   LatestFrame,
    running: Arc<AtomicBool>,
    port:    u16,
}

impl LocalStream {
    pub fn start(port: u16, camera: Weak<Camera>) -> Result<Self> {
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
                            let camera  = camera.clone();
                            thread::spawn(move || serve_client(stream, frame, running, camera));
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

        eprintln!("[stream] local HTTP server listening on http://0.0.0.0:{}/", port);
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

// ── Request routing ──────────────────────────────────────────────────────────

fn serve_client(
    mut stream: TcpStream,
    frame:      LatestFrame,
    running:    Arc<AtomicBool>,
    camera:     Weak<Camera>,
) {
    use std::io::Read;
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));

    // Read enough of the request to learn method + path.
    let mut buf = [0u8; 2048];
    let n = stream.read(&mut buf).unwrap_or(0);
    let request = std::str::from_utf8(&buf[..n]).unwrap_or("");

    let mut parts = request.lines().next().unwrap_or("").split_whitespace();
    let method = parts.next().unwrap_or("GET");
    let full_path = parts.next().unwrap_or("/");
    // Split path?query
    let (path, query) = match full_path.find('?') {
        Some(i) => (&full_path[..i], &full_path[i+1..]),
        None    => (full_path, ""),
    };

    match (method, path) {
        ("GET",  "/")             => serve_html(stream, INDEX_HTML),
        ("GET",  "/control")      => serve_html(stream, CONTROL_HTML),
        ("GET",  "/stream") |
        ("GET",  "/stream.mjpg") |
        ("GET",  "/mjpeg")        => serve_mjpeg(stream, frame, running),
        ("GET",  "/api/state")    => api_state(stream, &camera),
        ("POST", "/api/capture")  => api_capture(stream, &camera),
        ("POST", "/api/record/start") => api_record_start(stream, &camera),
        ("POST", "/api/record/stop")  => api_record_stop(stream, &camera),
        ("GET",    "/api/gallery")      => api_gallery_list(stream),
        ("GET",    "/api/gallery/file") => api_gallery_file(stream, query),
        ("DELETE", "/api/gallery/file") => api_gallery_delete(stream, query),
        _ => serve_404(stream),
    }
}

/// Parse a key from a URL query string. Returns the raw (still percent-encoded)
/// value — callers that need a real path must run it through `url_decode`.
fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key { return Some(v); }
        }
    }
    None
}

/// Percent-decode a URL query value. Handles `%XX` and `+` → space.
/// Browser's encodeURIComponent encodes `/` as `%2F`; without this decode,
/// our file path lookups would always fail.
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => { out.push(b' '); i += 1; }
            b'%' if i + 2 < bytes.len() => {
                if let Ok(b) = u8::from_str_radix(
                    std::str::from_utf8(&bytes[i+1..i+3]).unwrap_or(""), 16,
                ) {
                    out.push(b);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            _ => { out.push(bytes[i]); i += 1; }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ── Static HTML responses ────────────────────────────────────────────────────

fn serve_html(mut stream: TcpStream, body: &str) {
    let response = format!(
        "HTTP/1.0 200 OK\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-cache\r\n\
         Connection: close\r\n\r\n{}",
        body.len(), body
    );
    let _ = stream.write_all(response.as_bytes());
}

fn serve_404(mut stream: TcpStream) {
    let body = "404 Not Found\n";
    let response = format!(
        "HTTP/1.0 404 Not Found\r\n\
         Content-Type: text/plain\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{}",
        body.len(), body
    );
    let _ = stream.write_all(response.as_bytes());
}

// ── MJPEG ────────────────────────────────────────────────────────────────────

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

// ── JSON API ─────────────────────────────────────────────────────────────────

fn api_state(stream: TcpStream, camera: &Weak<Camera>) {
    let Some(cam) = camera.upgrade() else {
        return serve_json(stream, 503, r#"{"error":"camera unavailable"}"#);
    };
    let recording = cam.is_recording();
    let dur_secs  = cam.recording_duration().as_secs();
    let free      = free_space_str();
    let preview   = cam.is_stream_enabled();
    let duration  = format!("{:02}:{:02}", dur_secs / 60, dur_secs % 60);

    let json = format!(
        r#"{{"recording":{},"duration":"{}","free_space":"{}","preview":{}}}"#,
        recording, duration, free.replace('"', ""), preview
    );
    serve_json(stream, 200, &json);
}

fn api_capture(stream: TcpStream, camera: &Weak<Camera>) {
    let Some(cam) = camera.upgrade() else {
        return serve_json(stream, 503, r#"{"error":"camera unavailable"}"#);
    };
    // Default to the largest photo res. The on-device touchscreen still
    // controls per-shot res; /control fires "take the best photo" semantics.
    let (w, h, _) = PHOTO_RESOLUTIONS[0];
    match cam.capture_photo(w, h, false) {
        Ok(path) => {
            let p = path.to_string_lossy().replace('"', "\\\"");
            serve_json(stream, 200, &format!(r#"{{"ok":true,"path":"{}"}}"#, p));
        }
        Err(e) => {
            let msg = format!("{e:#}").replace('"', "'");
            serve_json(stream, 500, &format!(r#"{{"ok":false,"error":"{}"}}"#, msg));
        }
    }
}

fn api_record_start(stream: TcpStream, camera: &Weak<Camera>) {
    let Some(cam) = camera.upgrade() else {
        return serve_json(stream, 503, r#"{"error":"camera unavailable"}"#);
    };
    if cam.is_recording() {
        return serve_json(stream, 409, r#"{"ok":false,"error":"already recording"}"#);
    }
    let (w, h, _) = VIDEO_RESOLUTIONS[0];
    match cam.start_recording(w, h) {
        Ok(path) => {
            let p = path.to_string_lossy().replace('"', "\\\"");
            serve_json(stream, 200, &format!(r#"{{"ok":true,"path":"{}"}}"#, p));
        }
        Err(e) => {
            let msg = format!("{e:#}").replace('"', "'");
            serve_json(stream, 500, &format!(r#"{{"ok":false,"error":"{}"}}"#, msg));
        }
    }
}

fn api_record_stop(stream: TcpStream, camera: &Weak<Camera>) {
    let Some(cam) = camera.upgrade() else {
        return serve_json(stream, 503, r#"{"error":"camera unavailable"}"#);
    };
    if !cam.is_recording() {
        return serve_json(stream, 409, r#"{"ok":false,"error":"not recording"}"#);
    }
    let _ = cam.stop_recording(|| {});
    serve_json(stream, 200, r#"{"ok":true}"#);
}

// ── Gallery API ──────────────────────────────────────────────────────────────

fn api_gallery_list(stream: TcpStream) {
    let items = gallery::scan(&[SAVE_DIR, VIDEO_DIR, TL_DIR]);
    let mut json = String::from("[");
    for (i, item) in items.iter().enumerate() {
        if i > 0 { json.push(','); }
        // Escape the strings for JSON — names contain only digits / dots /
        // underscores by our convention, no JSON-special chars expected, but
        // be defensive about backslashes and quotes.
        let path = item.path.replace('\\', "\\\\").replace('"', "\\\"");
        let name = item.name.replace('\\', "\\\\").replace('"', "\\\"");
        json.push_str(&format!(
            r#"{{"path":"{}","name":"{}","is_video":{}}}"#,
            path, name, item.is_video
        ));
    }
    json.push(']');
    serve_json(stream, 200, &json);
}

fn api_gallery_file(stream: TcpStream, query: &str) {
    let Some(raw) = query_param(query, "path") else {
        return serve_json(stream, 400, r#"{"error":"missing path"}"#);
    };
    let req_path = url_decode(raw);

    // Security: only serve files that gallery::scan returns. Anything else
    // (path traversal, symlinks pointing elsewhere, etc.) gets a 404. We also
    // optionally serve the .thumb.jpg sidecar for videos.
    let items = gallery::scan(&[SAVE_DIR, VIDEO_DIR, TL_DIR]);
    let allowed = items.iter().any(|i| i.path == req_path);
    let is_thumb_sidecar = if let Some(base) = req_path.strip_suffix(".thumb.jpg") {
        items.iter().any(|i| i.is_video && i.path.starts_with(base))
    } else {
        false
    };
    if !allowed && !is_thumb_sidecar {
        return serve_404(stream);
    }

    let Ok(bytes) = std::fs::read(&req_path) else {
        return serve_404(stream);
    };

    let content_type = match req_path.rsplit('.').next().unwrap_or("").to_lowercase().as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png"          => "image/png",
        "dng"          => "image/x-adobe-dng",
        "mp4"          => "video/mp4",
        "h264"         => "video/h264",
        "mkv"          => "video/x-matroska",
        _              => "application/octet-stream",
    };

    let header = format!(
        "HTTP/1.0 200 OK\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: max-age=3600\r\n\
         Connection: close\r\n\r\n",
        content_type, bytes.len()
    );
    let mut s = stream;
    if s.write_all(header.as_bytes()).is_ok() {
        let _ = s.write_all(&bytes);
    }
}

fn api_gallery_delete(stream: TcpStream, query: &str) {
    let Some(raw) = query_param(query, "path") else {
        return serve_json(stream, 400, r#"{"error":"missing path"}"#);
    };
    let req_path = url_decode(raw);
    let items = gallery::scan(&[SAVE_DIR, VIDEO_DIR, TL_DIR]);
    if !items.iter().any(|i| i.path == req_path) {
        return serve_404(stream);
    }
    if gallery::delete(&req_path) {
        serve_json(stream, 200, r#"{"ok":true}"#);
    } else {
        serve_json(stream, 500, r#"{"ok":false,"error":"delete failed"}"#);
    }
}

fn serve_json(mut stream: TcpStream, status: u16, body: &str) {
    let status_text = match status {
        200 => "OK",
        409 => "Conflict",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _   => "OK",
    };
    let response = format!(
        "HTTP/1.0 {} {}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-cache\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Connection: close\r\n\r\n{}",
        status, status_text, body.len(), body
    );
    let _ = stream.write_all(response.as_bytes());
}

// ── Embedded HTML pages ──────────────────────────────────────────────────────

const INDEX_HTML: &str = r#"<!DOCTYPE html>
<html><head><title>picam-rs</title><style>
html,body{margin:0;background:#000;color:#d04040;font-family:'IBM Plex Mono',monospace;height:100%;display:flex;align-items:center;justify-content:center;flex-direction:column;gap:24px;}
a{color:#ff3030;text-decoration:none;border:1px solid #d04040;padding:12px 20px;border-radius:6px;font-size:18px;}
a:active{background:#1a0606;}
</style></head>
<body>
<h1 style="font-weight:400;font-size:24px;">picam-rs</h1>
<a href="/control">▸ Control Panel</a>
<a href="/stream">▸ Raw MJPEG Stream</a>
</body></html>"#;

const CONTROL_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, user-scalable=no, viewport-fit=cover">
<meta name="theme-color" content="#000000">
<meta name="mobile-web-app-capable" content="yes">
<meta name="apple-mobile-web-app-capable" content="yes">
<meta name="apple-mobile-web-app-status-bar-style" content="black">
<title>picam control</title>
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link href="https://fonts.googleapis.com/css2?family=IBM+Plex+Mono:wght@400;500;700&display=swap" rel="stylesheet">
<style>
:root {
  --bg:#000; --surface:#1a0606; --overlay:rgba(20,0,0,0.85);
  --accent:#ff3030; --warn:#c04020; --info:#802020;
  --text:#d04040; --dim:#803030; --border:#2a0808;
}
*{box-sizing:border-box;margin:0;padding:0;}
html,body{background:var(--bg);color:var(--text);font-family:'IBM Plex Mono','Courier New',monospace;height:100%;width:100%;overflow:hidden;user-select:none;-webkit-tap-highlight-color:transparent;}
body{display:flex;flex-direction:column;padding:env(safe-area-inset-top) env(safe-area-inset-right) env(safe-area-inset-bottom) env(safe-area-inset-left);font-variant-numeric:tabular-nums;}
.status{display:flex;justify-content:space-between;padding:8px 14px;font-size:12px;font-weight:500;border-bottom:1px solid var(--border);color:var(--dim);flex-shrink:0;}
.status .item{display:flex;gap:6px;align-items:center;}
.status .item .dot{color:var(--accent);font-size:10px;}
.status .recording .dot{color:var(--accent);}
.status .idle .dot{color:var(--dim);}
.preview{flex:1;background:#000;display:flex;align-items:center;justify-content:center;min-height:0;position:relative;}
.preview img{max-width:100%;max-height:100%;object-fit:contain;}
.preview .offline{position:absolute;color:var(--dim);font-size:14px;}
.mode-bar{display:flex;gap:6px;padding:8px;border-top:1px solid var(--border);flex-shrink:0;}
.mode-btn{flex:1;height:48px;background:transparent;color:var(--text);border:1px solid var(--border);border-radius:6px;font-family:inherit;font-size:14px;font-weight:500;cursor:pointer;touch-action:manipulation;}
.mode-btn.active{background:var(--info);font-weight:700;}
.mode-btn:active{background:var(--surface);}
.capture-zone{padding:14px;display:flex;justify-content:center;flex-shrink:0;}
.capture-btn{width:96px;height:96px;border-radius:50%;background:var(--info);border:2px solid var(--text);color:var(--text);font-family:inherit;font-size:14px;font-weight:700;cursor:pointer;touch-action:manipulation;transition:transform 80ms ease-out, background 100ms;}
.capture-btn:active{transform:scale(0.96);}
.capture-btn.recording{background:var(--accent);}
.capture-btn:disabled{opacity:0.5;}
.nav{display:flex;gap:6px;padding:8px 8px 12px;border-top:1px solid var(--border);flex-shrink:0;}
.nav-btn{flex:1;height:52px;background:transparent;color:var(--text);border:1px solid var(--border);border-radius:6px;font-family:inherit;font-size:13px;font-weight:500;cursor:pointer;touch-action:manipulation;}
.nav-btn:active{background:var(--surface);}
.drawer{position:fixed;inset:0;background:var(--overlay);display:none;flex-direction:column;justify-content:flex-end;z-index:10;}
.drawer.open{display:flex;}
.drawer-content{background:var(--bg);border-top:1px solid var(--text);border-radius:12px 12px 0 0;padding:16px;max-height:85vh;overflow-y:auto;display:flex;flex-direction:column;gap:12px;}
.drawer-content h2{font-size:14px;font-weight:700;color:var(--text);}
.drawer-content p{font-size:12px;color:var(--dim);line-height:1.6;}
.drawer-close{width:100%;height:48px;background:var(--surface);color:var(--text);border:1px solid var(--border);border-radius:6px;font-family:inherit;font-size:14px;font-weight:500;cursor:pointer;flex-shrink:0;}
.gallery-grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(120px,1fr));gap:8px;}
.gallery-item{aspect-ratio:1;background:var(--surface);border:1px solid var(--border);border-radius:6px;overflow:hidden;position:relative;cursor:pointer;}
.gallery-item img{width:100%;height:100%;object-fit:cover;display:block;}
.gallery-item .video-badge{position:absolute;top:4px;right:4px;background:var(--accent);color:var(--text);font-size:10px;font-weight:700;padding:2px 5px;border-radius:3px;}
.gallery-item .name{position:absolute;bottom:0;left:0;right:0;background:var(--overlay);color:var(--text);font-size:10px;padding:3px 6px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;}
.gallery-empty{color:var(--dim);font-size:13px;text-align:center;padding:32px;}
.lightbox{position:fixed;inset:0;background:#000;display:none;align-items:center;justify-content:center;z-index:20;width:100vw;height:100vh;}
.lightbox.open{display:flex;}
.lightbox img,.lightbox video{max-width:100vw;max-height:100vh;width:auto;height:auto;display:block;}
.lightbox .close{position:absolute;top:env(safe-area-inset-top,8px);right:8px;background:var(--surface);color:var(--text);border:1px solid var(--text);border-radius:6px;font-family:inherit;font-size:13px;padding:10px 14px;cursor:pointer;z-index:30;}
.lightbox .delete{position:absolute;top:env(safe-area-inset-top,8px);left:8px;background:var(--accent);color:var(--text);border:1px solid var(--text);border-radius:6px;font-family:inherit;font-size:13px;padding:10px 14px;cursor:pointer;z-index:30;}
.lightbox .caption{position:absolute;bottom:env(safe-area-inset-bottom,8px);left:8px;right:8px;background:var(--overlay);color:var(--text);font-size:11px;padding:6px 8px;border-radius:4px;text-align:center;z-index:30;}
@media (orientation:landscape) and (min-width:720px){
  body{flex-direction:row;flex-wrap:wrap;}
  .status{width:100%;}
  .preview{flex:1 0 60%;min-width:0;}
  .right{flex:1 0 40%;display:flex;flex-direction:column;border-left:1px solid var(--border);}
  .mode-bar,.capture-zone,.nav{border-top:1px solid var(--border);}
  .capture-btn{width:112px;height:112px;}
}
</style>
</head>
<body>
<div class="status idle" id="status-bar">
  <div class="item" id="rec-item"><span class="dot">●</span><span id="rec-text">idle</span></div>
  <div class="item"><span id="free-text">--</span> free</div>
  <div class="item"><span id="preview-text">live</span></div>
</div>
<div class="preview">
  <img id="preview-img" src="/stream" alt="preview">
  <div class="offline" id="offline" style="display:none">no preview</div>
</div>
<div class="right">
<div class="mode-bar">
  <button class="mode-btn active" data-mode="photo">Photo</button>
  <button class="mode-btn" data-mode="video">Video</button>
  <button class="mode-btn" data-mode="timelapse">Timelapse</button>
</div>
<div class="capture-zone">
  <button class="capture-btn" id="capture-btn">Capture</button>
</div>
<div class="nav">
  <button class="nav-btn" id="settings-btn">Settings</button>
  <button class="nav-btn" id="gallery-btn">Gallery</button>
</div>
</div>
<div class="drawer" id="drawer">
  <div class="drawer-content">
    <h2 id="drawer-title">Settings</h2>
    <div id="drawer-body"></div>
    <button class="drawer-close" id="drawer-close">Close</button>
  </div>
</div>
<div class="lightbox" id="lightbox">
  <button class="delete" id="lightbox-delete">Delete</button>
  <button class="close" id="lightbox-close">Close</button>
  <div id="lightbox-media-container" style="display:flex;align-items:center;justify-content:center;width:100%;height:100%;"></div>
  <div class="caption" id="lightbox-caption"></div>
</div>
<script>
let mode = 'photo';
let recording = false;
let busy = false;

const $ = (id) => document.getElementById(id);

function setCaptureLabel() {
  const btn = $('capture-btn');
  if (mode === 'photo') btn.textContent = 'Capture';
  else if (mode === 'video') btn.textContent = recording ? 'Stop' : 'Record';
  else btn.textContent = 'Timelapse';  // not wired yet
  btn.classList.toggle('recording', recording);
  btn.disabled = busy || (mode === 'timelapse');
}

document.querySelectorAll('.mode-btn').forEach(b => {
  b.addEventListener('click', () => {
    mode = b.dataset.mode;
    document.querySelectorAll('.mode-btn').forEach(x => x.classList.toggle('active', x.dataset.mode === mode));
    setCaptureLabel();
  });
});

$('capture-btn').addEventListener('click', async () => {
  if (busy) return;
  busy = true;
  setCaptureLabel();
  try {
    if (mode === 'photo') {
      await fetch('/api/capture', { method: 'POST' });
    } else if (mode === 'video') {
      const ep = recording ? '/api/record/stop' : '/api/record/start';
      await fetch(ep, { method: 'POST' });
    }
  } catch (e) { /* silent — state poll will reflect reality */ }
  busy = false;
  await pollState();
});

function setDrawerHTML(title, html) {
  $('drawer-title').textContent = title;
  $('drawer-body').innerHTML = html;
  $('drawer').classList.add('open');
}

$('settings-btn').addEventListener('click', () => {
  setDrawerHTML('Settings',
    '<p>Adjust ISO, shutter, AWB, resolution, etc. on the Pi touchscreen — they apply to all capture surfaces. Web-side settings coming soon.</p>'
  );
});

$('gallery-btn').addEventListener('click', async () => {
  setDrawerHTML('Gallery', '<p>Loading…</p>');
  try {
    const r = await fetch('/api/gallery', { cache: 'no-cache' });
    const items = await r.json();
    if (!items.length) {
      setDrawerHTML('Gallery', '<div class="gallery-empty">No photos or videos yet.</div>');
      return;
    }
    const grid = items.map(item => {
      // For videos, use the .thumb.jpg sidecar; for photos, the file itself.
      const thumbPath = item.is_video
        ? item.path.replace(/\.(mp4|h264|mkv)$/i, '.thumb.jpg')
        : item.path;
      const thumbUrl  = '/api/gallery/file?path=' + encodeURIComponent(thumbPath);
      const fullUrl   = '/api/gallery/file?path=' + encodeURIComponent(item.path);
      const badge     = item.is_video ? '<div class="video-badge">VIDEO</div>' : '';
      return '<div class="gallery-item" data-full="' + fullUrl + '" data-path="' + encodeURIComponent(item.path) + '" data-name="' + item.name + '" data-video="' + item.is_video + '">'
        + '<img src="' + thumbUrl + '" alt="' + item.name + '" loading="lazy">'
        + badge
        + '<div class="name">' + item.name + '</div>'
        + '</div>';
    }).join('');
    setDrawerHTML('Gallery (' + items.length + ')', '<div class="gallery-grid">' + grid + '</div>');

    document.querySelectorAll('.gallery-item').forEach(el => {
      el.addEventListener('click', () => {
        openLightbox(el.dataset.full, el.dataset.name, el.dataset.video === 'true', el.dataset.path);
      });
    });
  } catch (e) {
    setDrawerHTML('Gallery', '<p>Failed to load gallery: ' + e.message + '</p>');
  }
});

$('drawer-close').addEventListener('click', () => $('drawer').classList.remove('open'));
$('drawer').addEventListener('click', (e) => { if (e.target === $('drawer')) $('drawer').classList.remove('open'); });

// Lightbox state — tracks the current item for the Delete button
let currentLightboxPath = null;

function openLightbox(fullUrl, name, isVideo, encodedPath) {
  const c = $('lightbox-media-container');
  c.innerHTML = '';
  if (isVideo) {
    const v = document.createElement('video');
    v.controls = true;
    v.autoplay = true;
    v.playsInline = true;       // iOS Safari: stay inline, no native fullscreen takeover
    v.src = fullUrl;
    c.appendChild(v);
  } else {
    const img = document.createElement('img');
    img.src = fullUrl;
    img.alt = name;
    c.appendChild(img);
  }
  $('lightbox-caption').textContent = name;
  currentLightboxPath = encodedPath;
  $('lightbox').classList.add('open');

  // Request true browser fullscreen — gets rid of address bar, fills the
  // device screen, respects rotation natively via the browser. Best-effort:
  // iOS Safari only supports this on <video> elements (handled below).
  const root = document.documentElement;
  if (root.requestFullscreen) {
    root.requestFullscreen({ navigationUI: 'hide' }).catch(() => {});
  } else if (root.webkitRequestFullscreen) {
    root.webkitRequestFullscreen();
  }
}

function closeLightbox() {
  $('lightbox').classList.remove('open');
  $('lightbox-media-container').innerHTML = '';
  currentLightboxPath = null;
  if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
  else if (document.webkitFullscreenElement) document.webkitExitFullscreen();
}

$('lightbox-close').addEventListener('click', closeLightbox);
$('lightbox').addEventListener('click', (e) => {
  // Tap on the dark background (not on the image/buttons) closes too
  if (e.target === $('lightbox') || e.target.id === 'lightbox-media-container') closeLightbox();
});

$('lightbox-delete').addEventListener('click', async () => {
  if (!currentLightboxPath) return;
  if (!confirm('Delete this file? This cannot be undone.')) return;
  try {
    const r = await fetch('/api/gallery/file?path=' + currentLightboxPath, { method: 'DELETE' });
    if (!r.ok) throw new Error('delete ' + r.status);
    closeLightbox();
    // Refresh the gallery drawer
    $('gallery-btn').click();
  } catch (e) {
    alert('Delete failed: ' + e.message);
  }
});

async function pollState() {
  try {
    const r = await fetch('/api/state', { cache: 'no-cache' });
    if (!r.ok) throw new Error('state ' + r.status);
    const s = await r.json();
    recording = !!s.recording;
    $('rec-text').textContent = recording ? ('REC ' + s.duration) : 'idle';
    $('status-bar').classList.toggle('recording', recording);
    $('status-bar').classList.toggle('idle', !recording);
    $('free-text').textContent = s.free_space;
    $('preview-text').textContent = s.preview ? 'live' : 'off';
    $('offline').style.display = s.preview ? 'none' : 'block';
    setCaptureLabel();
  } catch (e) { /* connection blip — show stale until next tick */ }
}

setInterval(pollState, 1000);
pollState();

// Reload the preview img on connection loss (some browsers stall the multipart stream)
$('preview-img').addEventListener('error', () => {
  setTimeout(() => { $('preview-img').src = '/stream?t=' + Date.now(); }, 1000);
});
</script>
</body>
</html>"##;

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

    pub fn write_frame(&mut self, nv12: &[u8]) {
        if let Some(stdin) = self.stdin.as_mut() {
            let _ = stdin.write_all(nv12);
        }
    }

    pub fn stop(&mut self) {
        self.stdin.take();
        if let Some(mut child) = self.child.take() {
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
