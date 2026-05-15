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

- Live preview at 25 fps — MJPEG stream from `rpicam-vid`, decoded in Rust, displayed via Slint
- Simultaneous preview while recording video (in-process tee: same JPEG bytes written to file and decoded for display)
- Photo capture at full 12MP (4056×3040)
- H264 720p video recording (1280×720): MJPEG frames teed to a temp file while preview continues; ffmpeg transcodes to MP4 when recording stops
- Timelapse with configurable interval and duration; ffmpeg renders JPEG frames to MP4
- Gallery: browse and delete photos and videos
- Camera controls: ISO, Shutter Speed, AWB Mode, EV, Contrast, Saturation, Sharpness, Brightness, Zoom (1×–4× via `--roi`)
- Power menu: Shutdown, Reboot, Restart App
- True kiosk: `cage` compositor, getty autologin, `~/.bash_profile` cage launch loop — no desktop required

---

## Software Prerequisites

Install system dependencies on the Pi:

```bash
sudo apt install -y \
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
| rpicam-apps | system package |
| ffmpeg | system package |
| cage | 0.2 |

---

## Build

```bash
git clone <repo-url> ~/picam-rs
cd ~/picam-rs
cargo build --release
```

> **Note:** The first build downloads and compiles all Rust and Slint dependencies. On a Pi 4 (2GB), expect roughly **25 minutes**. Incremental builds are fast.

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

## How Video Recording Works

The live preview subprocess (`rpicam-vid --codec mjpeg -o -`) produces a continuous MJPEG byte stream. The Rust camera thread parses JPEG frame boundaries (`FF D8` ... `FF D9`) and for each complete frame:

1. **Tee to file** — if recording is active, the raw JPEG bytes are appended to a temp `.mjpeg` file
2. **Decode for display** — the `image` crate decodes the JPEG to RGBA8, which is sent to the Slint event loop as a `SharedPixelBuffer`

When recording stops, ffmpeg transcodes the temp file:

```
ffmpeg -y -framerate 25 -f mjpeg -i temp.mjpeg -c:v libx264 -preset fast -pix_fmt yuv420p output.mp4
```

The preview never pauses during recording. Still photo capture uses `rpicam-still` (separate process), which requires briefly pausing the preview subprocess (~600ms) to release the camera resource.

---

## Directory Structure

```
picam-rs/
├── build.rs                  # Slint compiler invocation
├── Cargo.toml                # Dependencies: slint, image, crossbeam-channel, anyhow
├── Cargo.lock
├── README.md
├── .gitignore
├── src/
│   ├── main.rs               # Slint event loop, UI callbacks, thread management
│   ├── camera.rs             # MJPEG preview loop, tee recording, still capture
│   ├── gallery.rs            # File scanning and deletion
│   └── timelapse.rs          # Timelapse scheduling + ffmpeg MP4 render
├── ui/
│   └── app.slint             # Slint UI: Viewfinder, Settings, Gallery, Power pages
└── setup/
    ├── install.sh            # Kiosk install helper script
    └── picam.service         # systemd unit file (alternative to bash_profile approach)
```

---

## Camera Settings

All settings are applied as CLI arguments to `rpicam-vid` and `rpicam-still`:

| Setting | rpicam flag | Range |
|---------|------------|-------|
| ISO | `--gain` (gain = ISO/100) | Auto, 100–3200 |
| Shutter speed | `--shutter` (microseconds) | Auto, 1/4000s–1s |
| White balance | `--awb` | auto, incandescent, tungsten, fluorescent, indoor, daylight, cloudy |
| Exposure compensation | `--ev` | −4.0 to +4.0 |
| Contrast | `--contrast` | 0.0–2.0 |
| Saturation | `--saturation` | 0.0–2.0 |
| Sharpness | `--sharpness` | 0.0–2.0 |
| Brightness | `--brightness` | −1.0 to +1.0 |
| Zoom | `--roi x,y,w,h` | 1×–4× (center crop) |

Settings are read from the Slint UI and pushed to the camera thread every 500ms. The preview subprocess is restarted when settings change.

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

`cargo build` (debug) works for iteration; run the binary directly in a Wayland session or under cage. UI changes to `ui/app.slint` require a recompile (Slint hot-reload is not used here), but incremental Rust builds are fast after the first build.

To monitor the kiosk log:

```bash
tail -f /tmp/picam-rs.log
```

---

## License

MIT
