# picam-rs

A kiosk camera application for the Raspberry Pi 4, written in Rust. Provides a full-screen touchscreen UI (Slint) with live preview, photo capture, 1080p video recording, timelapse, and a gallery. Runs under the `cage` Wayland compositor with no desktop environment.

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

- Live preview at 25 fps — NV12 frames delivered directly from libcamera, converted to RGBA in Rust, no MJPEG round-trip
- Simultaneous preview and 1080p video recording via dual libcamera streams (lores 800×480 for preview, main 1920×1080 for recording)
- Video recorded as H264 MP4 — NV12 frames piped in real-time to ffmpeg, file is ready immediately when recording stops
- Photo capture at full 12MP (4056×3040) via `rpicam-still`
- Timelapse with configurable interval and duration; ffmpeg renders JPEG frames to MP4
- Gallery: browse and delete photos and videos
- Camera controls: ISO, Shutter Speed, AWB Mode, EV, Contrast, Saturation, Sharpness, Brightness, Zoom (1×–4× via ScalerCrop)
- Power menu: Shutdown, Reboot, Restart App
- True kiosk: `cage` compositor, getty autologin, `~/.bash_profile` cage launch loop — no desktop required

---

## Software Prerequisites

Install system dependencies on the Pi:

```bash
sudo apt install -y \
    libcamera-dev \
    rpicam-apps \
    ffmpeg \
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
| libcamera | 0.7.1 |
| rpicam-apps | system package (for photo capture) |
| ffmpeg | system package |
| cage | 0.2 |

---

## Build

```bash
git clone <repo-url> ~/picam-rs
cd ~/picam-rs
cargo build --release
```

> **Note:** The first build compiles the C++ libcamera wrapper (`src/camera_ffi.cpp` via the `cc` crate) plus all Rust and Slint dependencies. On a Pi 4 (2GB), expect roughly **25 minutes**. Incremental builds are fast.

The release binary is placed at `target/release/picam`.

---

## Kiosk Setup

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

On next boot, `getty` logs in `pi` automatically, `~/.bash_profile` launches `cage`, and `cage` runs the app fullscreen. If the app exits (crash or "Restart App"), the loop relaunches it after 3 seconds. There is no desktop environment involved.

---

## How It Works

### Preview

libcamera opens two concurrent streams when the app starts:

- **lores** (800×480, NV12) — delivered to the Rust preview callback at 25 fps
- **main** (1920×1080, NV12) — delivered to the Rust record callback only when recording is active

The C++ wrapper (`src/camera_ffi.cpp`) handles libcamera initialisation, buffer allocation, request queuing, and completion callbacks. The Rust layer converts NV12 frames to RGBA8 and sends them to the Slint event loop via a bounded channel.

### Video Recording

When recording starts, an ffmpeg child process is spawned with a raw NV12 pipe as input:

```
ffmpeg -f rawvideo -pix_fmt nv12 -s 1920x1080 -r 25 -i pipe:0 -c:v libx264 -preset fast output.mp4
```

Each main-stream frame is written directly to the pipe. The preview continues uninterrupted. When recording stops, the pipe is closed and ffmpeg finalises the MP4 immediately — no post-processing step.

### Photo Capture

Still photos use `rpicam-still` for full 12MP resolution (4056×3040). The libcamera preview streams are briefly paused (~200ms) to release the camera resource, `rpicam-still` runs, then streaming resumes.

### Camera Settings

Settings are applied as libcamera controls on the next queued request (no subprocess restart needed):

| Setting | libcamera control | Range |
|---------|------------------|-------|
| ISO | `AnalogueGain` (gain = ISO/100) | Auto, 100–3200 |
| Shutter speed | `ExposureTime` (microseconds) | Auto, 1/4000s–1s |
| White balance | `AwbMode` | auto, incandescent, tungsten, fluorescent, indoor, daylight, cloudy |
| Exposure compensation | `ExposureValue` | −4.0 to +4.0 |
| Contrast | `Contrast` | 0.0–2.0 |
| Saturation | `Saturation` | 0.0–2.0 |
| Sharpness | `Sharpness` | 0.0–2.0 |
| Brightness | `Brightness` | −1.0 to +1.0 |
| Zoom | `ScalerCrop` (sensor rectangle) | 1×–4× (centre crop) |

Settings are read from the Slint UI and pushed to the camera every 500ms, applied without restarting the preview.

---

## Directory Structure

```
picam-rs/
├── build.rs                  # Slint compiler + cc crate compiles camera_ffi.cpp
├── Cargo.toml                # Dependencies: slint, crossbeam-channel, anyhow; build: slint-build, cc
├── Cargo.lock
├── README.md
├── .gitignore
├── src/
│   ├── main.rs               # Slint event loop, UI callbacks, thread management
│   ├── camera.rs             # Rust FFI bindings, NV12→RGBA, ffmpeg pipe recording
│   ├── camera_ffi.h          # C header: PicamHandle API
│   ├── camera_ffi.cpp        # C++ libcamera wrapper (dual-stream, controls, pause/resume)
│   ├── gallery.rs            # File scanning and deletion
│   └── timelapse.rs          # Timelapse scheduling + ffmpeg MP4 render
├── ui/
│   └── app.slint             # Slint UI: Viewfinder, Settings, Gallery, Power pages
└── setup/
    ├── install.sh            # Kiosk install helper script
    └── picam.service         # systemd unit file (alternative to bash_profile approach)
```

---

## Media Storage

| Type | Directory |
|------|-----------|
| Photos | `/home/pi/Pictures/picam/` |
| Videos | `/home/pi/Videos/picam/` |
| Timelapse | `/home/pi/Pictures/timelapse/` |

Directories are created on first launch.

---

## Development

SSH into the Pi:

```bash
ssh -i ~/.ssh/id_rsa pi@rpi4-2GB
```

`cargo build` (debug) works for iteration. The C++ wrapper is recompiled only when `src/camera_ffi.cpp` or `src/camera_ffi.h` changes. UI changes to `ui/app.slint` require a recompile but incremental builds are fast after the first build.

Keep `camera_ffi.h` and `camera_ffi.cpp` in sync with the `extern "C"` block in `camera.rs` — any signature change on the C++ side must be reflected in the Rust FFI declarations and vice versa.

To monitor the kiosk log:

```bash
tail -f /tmp/picam-rs.log
```

---

## License

MIT
