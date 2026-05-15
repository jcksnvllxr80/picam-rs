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
- Video first-frame thumbnail extracted by ffmpeg at recording stop
- Delete (also removes the `.thumb.jpg` sidecar)
- Newest first

**UX**

- Red astro theme — no white, blue, or green anywhere in the UI
- Tabbed Settings (Capture / Image / Advanced / General)
- Last-shot review thumbnail flashes for 2s after capture (toggleable)
- Storage-free indicator in the General tab
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
| rpicam-apps | system package (for photo/timelapse capture) |
| ffmpeg | system package |
| cage | 0.2 |

---

## Build

```bash
git clone <repo-url> ~/picam-rs
cd ~/picam-rs
cargo build --release
```

> **First build:** ~25 minutes on a Pi 4 (2GB). It compiles the C++ libcamera wrapper (`src/camera_ffi.cpp` via the `cc` crate) plus the full Slint dependency tree. Subsequent builds are fast (a few minutes for Rust changes; ~10s for UI-only changes).

The release binary lands at `target/release/picam`.

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

---

## Settings UI

The Settings page is organised into four tabs so each fits on the 480px screen without scrolling:

| Tab | Contents |
|-----|----------|
| **Capture** | ISO, Shutter (two rows — short and long exposures), AWB, EV, Zoom |
| **Image** | Contrast, Saturation, Sharpness, Brightness |
| **Advanced** | Live Preview on/off, Photo Res, Video Res, TL Res, RAW (DNG) capture |
| **General** | Self-Timer, Burst, Last-Shot Review, Storage Free |

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
- **Screen brightness control** — dim the display further or sleep during long exposures
- **Video playback** — currently videos show a first-frame thumbnail only; in-app playback is deferred (Slint has no native video widget)

---

## License

MIT
