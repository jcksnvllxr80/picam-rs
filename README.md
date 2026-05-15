# picam-rs

An astrophotography-focused kiosk camera application for the Raspberry Pi 4, written in Rust. Built specifically for telescope eyepiece imaging — a Pi 4 sits at the telescope with the official 7" touchscreen, you frame and capture through the eyepiece.

The entire UI is a monochrome red palette to preserve dark adaptation; the screen never emits anything that would wreck the rod-cell vision you need for observing through the eyepiece. Runs full-screen under the `cage` Wayland compositor — no desktop, no menu bars, just the camera.

---

## Hardware

| Component | Details |
|-----------|---------|
| Board | Raspberry Pi 4 (2GB RAM) |
| OS | Debian GNU/Linux 13 (Trixie) 64-bit, aarch64 |
| Camera | Raspberry Pi HQ Camera (IMX477, 12MP), mounted 180° — corrected in software |
| Display | Official Raspberry Pi 7" Touchscreen, 800×480 |

---

## Features

**Astrophotography essentials**

- Long-exposure shutter range from 1/4000s up to 120s for deep-sky imaging
- RAW (DNG) capture toggle saves alongside JPEG for proper post-processing
- Self-timer (Off / 2s / 5s / 10s) eliminates touch-induced vibration
- Burst mode (1 / 3 / 5 / 10 shots) for picking sharpest frames when seeing is unsteady
- Live preview while recording video — dual libcamera streams, no pipeline interruption

**Capture modes**

- Photo: 12MP / 8MP / 5MP / FHD via `rpicam-still`, full resolution control
- Video: 1080p / 720p / 480p H264 MP4, ffmpeg-scaled output
- Timelapse: configurable interval (1s–5min) and duration (30s–2h), ffmpeg renders to MP4

**Live preview**

- 25 fps NV12 direct from libcamera, NV12→RGBA converted on a worker thread (libcamera pipeline never blocks)
- Stream on/off toggle saves CPU when actively imaging (zero-CPU idle mode)
- 180° rotation handled in software via libcamera `Orientation::Rotate180`

**Gallery**

- Photo full-screen preview by tapping any item
- Video playback in-app via `mpv` fullscreen — tap the ▶ overlay on a video; press `q` or `Esc` to return
- Video first-frame thumbnail extracted by ffmpeg at recording stop
- Delete (also removes the `.thumb.jpg` sidecar)
- Auto-refresh on Gallery tab tap — picks up captures from `/control` and side-channel additions
- Newest first

**Mobile web companion (`/control`)**

- Point any phone or laptop browser at `http://<pi-host>:8080/control` for a touch-optimised remote control
- IBM Plex Mono monospace UI on the same astro-red palette as the touchscreen
- Live preview, mode selector (Photo / Video), capture / record buttons with progress feedback
- **Settings drawer** — adjust ISO, Shutter, AWB, EV, Zoom, Contrast, Saturation, Sharpness, Brightness from the phone. Changes sync to the touchscreen UI and back to the camera within 500ms
- Gallery drawer: thumbnail grid, fullscreen lightbox (image or video), delete with confirmation
- Tapping a thumbnail triggers the browser Fullscreen API — no address bar, rotation-aware
- HTTP/MJPEG also served at `/stream` for VLC, OBS, ffmpeg, or any other MJPEG consumer

**UX**

- Red astro theme — no white, blue, or green anywhere in the UI
- No visible mouse cursor on the touchscreen (1×1 transparent XCursor theme + `mouse-cursor: none` on every TouchArea)
- Tabbed Settings (Capture / Image / Advanced / Stream / General)
- Last-shot review thumbnail flashes for 2s after capture (toggleable)
- Storage-free indicator in the General tab
- Screen sleep with configurable timeout (Off / 30s / 1m / 5m / 10m / 30m) — any tap wakes the screen
- Hold-to-confirm Shutdown and Reboot (1.5s) — protects against stray taps during imaging
- Power menu: Shutdown / Reboot / Restart App

**Camera tuning controls**

- ISO, Shutter, AWB, Exposure Value, Zoom (1×–4× via `ScalerCrop`)
- Contrast, Saturation, Sharpness, Brightness
- Settings pushed to libcamera every 500ms; applied without restarting the preview

**True kiosk**

- `cage` compositor, getty autologin, `~/.bash_profile` cage launch loop — no desktop required
- App-exit auto-relaunch via the bash loop (after 3-second pause)

---

## Software Prerequisites

Install system dependencies on the Pi:

```bash
sudo apt install -y \
    libcamera-dev \
    libjpeg62-turbo-dev \
    libfontconfig1-dev \
    libxkbcommon-dev \
    libwayland-dev \
    rpicam-apps \
    ffmpeg \
    mpv \
    cage \
    build-essential \
    pkg-config
```

Install Rust:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source ~/.cargo/env
```

Versions used in development:

| Tool / Library | Version |
|----------------|---------|
| Rust (stable) | 1.95 |
| Slint | 1.16 |
| libcamera | 0.7.1 (Raspberry Pi fork — `+rpt*`) |
| rpicam-apps | system package (for photo/timelapse capture) |
| ffmpeg | system package |
| mpv | system package (for in-gallery video playback) |
| cage | 0.2 |

---

## Install (from prebuilt .deb — recommended)

Every push to `main` triggers a GitHub Actions release with a built `.deb` attached:

```bash
# Replace <VERSION> with the latest release on the GitHub Releases page
wget https://github.com/jcksnvllxr80/picam-rs/releases/latest/download/picam-rs_<VERSION>-1_arm64.deb
sudo apt install ./picam-rs_<VERSION>-1_arm64.deb
sudo reboot
```

The `.deb`'s postinst handles everything: installs the binary at `/usr/bin/picam`, configures passwordless sudo for the Power menu, sets up getty autologin on tty1, disables lightdm, and enables `picam.service`. After reboot the kiosk starts fullscreen.

To uninstall:

```bash
sudo apt remove picam-rs       # keeps config files
sudo apt purge picam-rs        # also removes /etc/sudoers.d/picam-power and getty override
```

---

## Build from source

```bash
git clone <repo-url> ~/picam-rs
cd ~/picam-rs
cargo build --release
```

> **First build:** ~25 minutes on a Pi 4 (2GB). It compiles the C++ libcamera wrapper (`src/camera_ffi.cpp` via the `cc` crate) plus the full Slint dependency tree. Subsequent builds are fast (a few minutes for Rust changes; ~10s for UI-only changes).

The release binary lands at `target/release/picam`. To build a local `.deb`:

```bash
cargo install cargo-deb
cargo deb
# → target/debian/picam-rs_<ver>-1_arm64.deb
```

---

## Kiosk Setup (manual — skip this if you installed from .deb)

> The `.deb` postinst does steps 1–4 automatically. This section is only for source builds or for understanding what the package configures.

### 1. Passwordless sudo for power commands

```bash
echo 'pi ALL=(ALL) NOPASSWD: /sbin/shutdown, /sbin/reboot' | sudo tee /etc/sudoers.d/picam-power
```

### 2. Getty autologin on tty1

```bash
sudo mkdir -p /etc/systemd/system/getty@tty1.service.d
sudo tee /etc/systemd/system/getty@tty1.service.d/autologin.conf <<'EOF'
[Service]
ExecStart=
ExecStart=-/sbin/agetty --autologin pi --noclear %I $TERM
EOF
sudo systemctl daemon-reload
```

### 3. Disable lightdm (if installed)

```bash
sudo systemctl disable lightdm
```

### 4. Add cage launch loop to ~/.bash_profile

```bash
cat >> ~/.bash_profile <<'EOF'

if [[ $(tty) == /dev/tty1 ]] && [[ -z "$WAYLAND_DISPLAY" ]]; then
    while true; do
        cage -s -- /home/pi/picam-rs/target/release/picam >> /tmp/picam-rs.log 2>&1
        echo "$(date): app exited $?, restarting in 3s" >> /tmp/picam-rs.log
        sleep 3
    done
fi
EOF
```

On next boot, `getty` logs in `pi` automatically, `~/.bash_profile` launches `cage`, and `cage` runs the app fullscreen. If the app exits (crash or "Restart App"), the loop relaunches it after 3 seconds. No desktop environment involved.

---

## How It Works

### Preview pipeline

libcamera opens two concurrent streams at startup:

- **lores** (800×480, NV12) — delivered to the Rust preview callback at 25 fps
- **main** (1920×1080, NV12) — delivered to the Rust record callback only when recording is active

The C++ wrapper (`src/camera_ffi.cpp`) handles libcamera initialisation, buffer allocation via `FrameBufferAllocator`, dma-buf mmap, request queuing, and the `requestCompleted` signal. The Rust callbacks on the libcamera completion thread do only fast work — memcpy of the NV12 bytes into an owned `Vec<u8>`, then `try_send` on a bounded channel.

A dedicated **`nv12-rgba` worker thread** in Rust pulls NV12 frames off that channel, converts them to RGBA8, and pushes the RGBA into another bounded channel feeding the Slint event loop. This keeps the libcamera pipeline from ever being blocked by conversion work — if the worker can't keep up, frames drop naturally at the channel boundaries rather than backpressuring the camera.

Architecturally:

```
libcamera completion thread          worker thread              Slint event loop
   on_preview_frame()       →   nv12-rgba (convert)    →    frame-pump (display)
   memcpy + try_send             ~30% of one core           draws preview
```

### Video recording

When recording starts, an ffmpeg child process is spawned with a raw NV12 pipe as input. Each main-stream frame is written directly to the pipe in the libcamera callback:

```
ffmpeg -y -f rawvideo -pix_fmt nv12 -s 1920x1080 -r 25 -i pipe:0 \
       -vf scale=W:H \
       -c:v libx264 -preset fast -pix_fmt yuv420p output.mp4
```

The `-vf scale=W:H` filter is added only when the selected output resolution differs from 1080p (libcamera always streams at 1080p internally). Preview never pauses during recording.

When recording stops, the stdin pipe is closed and ffmpeg finalises the MP4 immediately — no post-processing pass. A second ffmpeg invocation extracts a first-frame `<name>.thumb.jpg` for the gallery preview.

### Photo capture

Still photos use `rpicam-still` for full 12MP resolution. Because libcamera holds the camera pipeline handler exclusively, the in-process libcamera session must fully release the camera before `rpicam-still` can acquire it. `picam_pause` therefore does a complete teardown — stop streams, `munmap` all buffers, drop the FrameBufferAllocator and Request objects, then `camera->release()`. `picam_resume` rebuilds everything on the way back: `acquire()` → `configure()` → reallocate buffers → re-mmap → rebuild requests → `start()` → re-queue. Total round-trip is roughly 500–800ms.

For long exposures, `rpicam-still --timeout` is padded to exceed the shutter length (capture would otherwise be cut short).

### Mobile web companion (`/control`)

An embedded HTTP server (hand-written multipart MJPEG + JSON router, no external dependencies) runs on port 8080 by default. It serves three surfaces:

| Path | Purpose |
|------|---------|
| `GET /control` | Touch-optimised HTML control page (IBM Plex Mono on the astro-red palette) |
| `GET /stream` | Multipart MJPEG live preview for browsers, VLC, OBS, ffmpeg, etc. |
| `GET /` | Landing page with links to `/control` and `/stream` |

The control page polls a small JSON API for state and posts back capture commands:

| Method | Path | Body / Query | Returns |
|---|---|---|---|
| `GET` | `/api/state` | — | `{recording, duration, free_space, preview}` |
| `POST` | `/api/capture` | — | `{ok, path}` — fires a 12MP photo |
| `POST` | `/api/record/start` | — | `{ok, path}` — start 1080p H264 recording |
| `POST` | `/api/record/stop` | — | `{ok}` |
| `GET` | `/api/gallery` | — | `[{path, name, is_video}, ...]` |
| `GET` | `/api/gallery/file` | `?path=...` | File bytes with correct Content-Type |
| `DELETE` | `/api/gallery/file` | `?path=...` | `{ok}` — also removes the `.thumb.jpg` sidecar |
| `GET` | `/api/settings` | — | `{iso_idx, shutter_idx, awb_idx, ev, zoom, contrast, saturation, sharpness, brightness}` |
| `POST` | `/api/settings` | JSON patch (any subset of those keys) | `{ok}` — applied via UI handle, propagates to camera within 500ms |

The server requires *path validity* against `gallery::scan()` — any request for a path outside `/home/pi/Pictures/picam`, `/home/pi/Videos/picam`, or `/home/pi/Pictures/timelapse` gets a 404 even if the file exists. URL-percent-encoded paths are decoded server-side.

Streaming sinks (configured in `[stream]` of the TOML config):
- `local_port` — bind port for the HTTP/MJPEG server (default 8080)
- `push_url` — optional RTMP/SRT/MPEG-TS URL; when set and enabled, an ffmpeg subprocess pushes H264 to that destination from the same preview stream

### Capture settings table

Most settings are applied as libcamera controls on the next queued request (no restart needed):

| Setting | libcamera control | Range |
|---------|------------------|-------|
| ISO | `AnalogueGain` (gain = ISO/100) | Auto, 100–3200 |
| Shutter speed | `ExposureTime` (microseconds) | Auto, 1/4000s–120s |
| White balance | `AwbMode` | auto, incandescent, tungsten, fluorescent, indoor, daylight, cloudy |
| Exposure compensation | `ExposureValue` | −4.0 to +4.0 |
| Contrast | `Contrast` | 0.0–2.0 |
| Saturation | `Saturation` | 0.0–2.0 |
| Sharpness | `Sharpness` | 0.0–2.0 |
| Brightness | `Brightness` | −1.0 to +1.0 |
| Zoom | `ScalerCrop` (sensor rectangle) | 1×–4× (centre crop) |
| Auto-exposure toggle | `AeEnable` | auto-driven: `false` whenever the user manually sets ISO or Shutter, `true` otherwise |

**How control persistence works.** Two libcamera gotchas dictate the C++ wrapper's design:

1. `Request::reuse(ReuseBuffers)` clears all controls on a recycled request — setting controls *before* the reuse means they're immediately wiped.
2. libcamera does not auto-propagate per-frame controls (`AnalogueGain`, `ExposureTime`, `Contrast`, …) to subsequent requests. Setting them once means they apply to a single frame and then revert.

So the wrapper stores the desired control state in atomic fields on `PicamHandle`. Every `requestCompleted` callback:

1. Delivers the buffered frame to the Rust callback.
2. Calls `req->reuse(ReuseBuffers)` to recycle the request.
3. Calls `apply_controls(req)` which reads the atomic state and writes a fresh `ControlList`.
4. Re-queues the request.

Auto-exposure on/off is driven by `controls::AeEnable` (the canonical, well-supported AE toggle). The newer `AnalogueGainMode` / `ExposureTimeMode` controls exist in libcamera 0.7.1's headers but the RPi IPA doesn't yet honor them — picam tried using them first and silently no-op'd.

**Long-exposure preview cap.** The dual-stream libcamera session uses one sensor exposure for both the preview and record streams. A 60-second user-set shutter means every preview frame takes 60 seconds, which freezes the live view. picam therefore caps the *preview* shutter at 1 second — anything longer (deep-sky exposures) keeps the preview running at a reasonable framerate. Actual still capture (`rpicam-still` subprocess) uses the full unclamped user value via `--shutter`, so the photo on disk is the real 60-second / 120-second exposure you asked for.

---

## Settings UI

The Settings page is organised into five tabs so each fits on the 480px screen without scrolling:

| Tab | Contents |
|-----|----------|
| **Capture** | ISO, Shutter (two rows — short and long exposures), AWB, EV, Zoom |
| **Image** | Contrast, Saturation, Sharpness, Brightness |
| **Advanced** | Live Preview on/off, Photo Res, Video Res, TL Res, RAW (DNG) capture |
| **Stream** | HTTP/MJPEG server on/off + port, push-URL on/off, Reload config |
| **General** | Self-Timer, Burst, Last-Shot Review, **Sleep Timeout**, Storage Free |

### Screen sleep

`General → Sleep Timeout` blanks the screen after N seconds of touch inactivity. Any tap wakes it. Powered by a Slint `pointer-event` back-layer that resets a global idle counter without instrumenting every individual button — same trick that hides the cursor (`mouse-cursor: none`).

**Backlight off:** when the screen sleeps, picam writes `0` to `/sys/class/backlight/*/brightness` so the touchscreen LEDs cut power entirely — pitch black, no stray light during long astrophoto exposures. The wake-up tap restores `max_brightness`. A udev rule installed by the `.deb` chmods the sysfs path to 0666 so the kiosk user can write to it without sudo.

If picam is killed while the screen is asleep, the LEDs stay dark — nothing wrote `max_brightness` back. The next picam launch always forces brightness back to max on startup, so the cage relaunch loop (or a manual `sudo systemctl restart picam`) recovers automatically. As a last resort: `echo 255 | sudo tee /sys/class/backlight/*/brightness`.

---

## Directory Structure

```
picam-rs/
├── build.rs                  # Slint compiler + cc crate compiles camera_ffi.cpp
├── Cargo.toml                # Deps: slint, crossbeam-channel, anyhow; build-deps: slint-build, cc
├── Cargo.lock
├── README.md
├── .gitignore
├── src/
│   ├── main.rs               # Slint event loop, UI callbacks, worker threads
│   ├── camera.rs             # Rust FFI bindings, NV12→RGBA worker, capture orchestration
│   ├── camera_ffi.h          # C header: PicamHandle API
│   ├── camera_ffi.cpp        # C++ libcamera wrapper (dual-stream, controls, pause/resume)
│   ├── gallery.rs            # File scanning, deletion (with thumbnail cleanup)
│   └── timelapse.rs          # Timelapse scheduling + ffmpeg MP4 render
├── ui/
│   └── app.slint             # Slint UI: Viewfinder, Settings (4 tabs), Gallery, Power
└── setup/
    ├── install.sh            # Kiosk install helper script
    └── picam.service         # systemd unit (alternative to bash_profile approach)
```

---

## Media Storage

| Type | Directory | Filename format |
|------|-----------|-----------------|
| Photos | `/home/pi/Pictures/picam/` | `IMG_<ts>.jpg` (+ `.dng` when RAW is enabled) |
| Burst photos | `/home/pi/Pictures/picam/` | `IMG_<ts>_NN.jpg` |
| Videos | `/home/pi/Videos/picam/` | `VID_<ts>.mp4` (+ `.thumb.jpg` sidecar) |
| Timelapse frames | `/home/pi/Pictures/timelapse/session_<ts>/` | `frame_NNNNNN.jpg` |
| Rendered timelapses | `/home/pi/Pictures/timelapse/` | `timelapse_<ts>.mp4` |

Directories are created on first launch.

---

## Development

SSH into the Pi:

```bash
ssh -i ~/.ssh/id_rsa pi@rpi4-2GB
```

`cargo build` (debug) works for iteration. The C++ wrapper recompiles only when `src/camera_ffi.cpp` or `src/camera_ffi.h` changes. UI changes to `ui/app.slint` trigger a Slint codegen pass; incremental Rust builds after that are fast.

Keep `camera_ffi.h` and `camera_ffi.cpp` in sync with the `extern "C"` block at the top of `src/camera.rs` — any signature change on the C++ side must be reflected in the Rust FFI declarations and vice versa.

Run tests on the Pi (the camera tests don't touch hardware):

```bash
cargo test --release
```

Monitor the kiosk log:

```bash
tail -f /tmp/picam-rs.log
```

If the running app is hogging CPU during a rebuild, you can free a core with:

```bash
sudo kill -STOP $(pgrep -x picam)   # suspend (doesn't trigger the cage relaunch loop)
sudo kill -CONT $(pgrep -x picam)   # resume
sudo kill -KILL $(pgrep -x picam)   # actually quit; bash loop relaunches in 3s
```

---

## Roadmap

Coming features (not yet shipped):

- **Histogram overlay** — luminance histogram in the viewfinder corner to verify exposure
- **Focus peaking** — edge detection overlay on preview to confirm sharp focus through the eyepiece
- **`/control` Advanced/General settings** — currently the web companion's settings drawer exposes the Capture and Image tabs; resolution, RAW, self-timer, burst, sleep timeout still touchscreen-only

---

## License

MIT
