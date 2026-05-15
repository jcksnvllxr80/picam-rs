mod backlight;
mod camera;
mod config;
mod gallery;
mod stream;
mod timelapse;

use std::sync::Arc;
use std::time::Duration;
use std::thread;

use slint::{Model, ModelRc, SharedPixelBuffer, Rgba8Pixel, VecModel};

use camera::{
    Camera, CameraEvent, CameraSettings,
    SAVE_DIR, VIDEO_DIR, TL_DIR,
    PHOTO_RESOLUTIONS, VIDEO_RESOLUTIONS, TL_RESOLUTIONS,
    free_space_str,
};

const SELF_TIMER_SECS: &[i32] = &[0, 2, 5, 10];
const BURST_COUNTS:    &[usize] = &[1, 3, 5, 10];

slint::include_modules!();

pub fn timestamp_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

fn read_settings(ui: &AppWindow) -> CameraSettings {
    CameraSettings {
        iso_idx:     ui.get_iso_idx() as usize,
        shutter_idx: ui.get_shutter_idx() as usize,
        awb_idx:     ui.get_awb_idx() as usize,
        ev:          ui.get_ev(),
        contrast:    ui.get_contrast(),
        saturation:  ui.get_saturation(),
        sharpness:   ui.get_sharpness(),
        brightness:  ui.get_brightness(),
        zoom:        ui.get_zoom(),
    }
}

fn main() {
    std::env::remove_var("DISPLAY");
    // cage sets WAYLAND_DISPLAY; if running standalone, default to wayland-1
    if std::env::var("WAYLAND_DISPLAY").is_err() {
        std::env::set_var("WAYLAND_DISPLAY", "wayland-1");
    }

    let cfg = config::Config::load();

    let cam = Arc::new(Camera::new());
    let tl  = Arc::new(timelapse::Timelapse::new());

    let app    = AppWindow::new().expect("Slint init");
    let handle = app.as_weak();

    // Apply config defaults to UI properties so the user's TOML is honored.
    app.set_iso_idx        (cfg.camera.iso_idx     as i32);
    app.set_shutter_idx    (cfg.camera.shutter_idx as i32);
    app.set_awb_idx        (cfg.camera.awb_idx     as i32);
    app.set_ev             (cfg.camera.ev);
    app.set_contrast       (cfg.camera.contrast);
    app.set_saturation     (cfg.camera.saturation);
    app.set_sharpness      (cfg.camera.sharpness);
    app.set_brightness     (cfg.camera.brightness);
    app.set_zoom           (cfg.camera.zoom);
    app.set_photo_res_idx  (cfg.capture.photo_res_idx as i32);
    app.set_video_res_idx  (cfg.capture.video_res_idx as i32);
    app.set_tl_res_idx     (cfg.capture.tl_res_idx    as i32);
    app.set_raw_enabled    (cfg.capture.raw_enabled);
    app.set_stream_enabled (cfg.capture.stream_enabled);
    app.set_self_timer_idx (cfg.general.self_timer_idx  as i32);
    app.set_burst_count_idx(cfg.general.burst_count_idx as i32);
    app.set_last_shot_enabled(cfg.general.last_shot_enabled);
    app.set_sleep_timeout_idx(cfg.general.sleep_timeout_idx as i32);
    app.set_stream_port    (cfg.stream.local_port as i32);
    app.set_push_url       (cfg.stream.push_url.clone().into());

    // Apply startup defaults to the camera itself
    if !cfg.capture.stream_enabled {
        cam.set_stream_enabled(false);
    }

    // Auto-start streams if config says so
    if cfg.stream.local_enabled {
        match cam.set_local_stream(cfg.stream.local_port) {
            Ok(()) => app.set_local_stream_active(true),
            Err(e) => eprintln!("[stream] local auto-start failed: {e:#}"),
        }
    }
    if cfg.stream.push_enabled && !cfg.stream.push_url.is_empty() {
        let (w, h) = (800u32, 480u32); // preview stream resolution
        match cam.set_push_stream(&cfg.stream.push_url, w, h) {
            Ok(()) => app.set_push_stream_active(true),
            Err(e) => eprintln!("[stream] push auto-start failed: {e:#}"),
        }
    }

    // ── Frame pump: camera thread → Slint event loop ──────────────────────────
    {
        let rx     = cam.frame_rx.clone();
        let handle = handle.clone();
        thread::Builder::new()
            .name("frame-pump".into())
            .spawn(move || loop {
                match rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(CameraEvent::Frame { rgba, width, height }) => {
                        // SharedPixelBuffer is Send; build Image inside event loop where it's allowed
                        let buf = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&rgba, width, height);
                        let _ = handle.upgrade_in_event_loop(move |ui| {
                            ui.set_preview_frame(slint::Image::from_rgba8(buf));
                        });
                    }
                    Err(_) => {}
                }
            })
            .expect("spawn frame-pump");
    }

    // ── Storage free-space updater (every 10s, plus once at startup) ──────────
    {
        let handle = handle.clone();
        thread::Builder::new()
            .name("free-space".into())
            .spawn(move || loop {
                let s = free_space_str();
                let _ = handle.upgrade_in_event_loop(move |ui| ui.set_free_space_str(s.into()));
                thread::sleep(Duration::from_secs(10));
            })
            .expect("spawn free-space");
    }

    // ── Recording duration ticker ─────────────────────────────────────────────
    {
        let cam_ref = Arc::clone(&cam);
        let handle  = handle.clone();
        thread::Builder::new()
            .name("rec-ticker".into())
            .spawn(move || loop {
                thread::sleep(Duration::from_millis(500));
                if cam_ref.is_recording() {
                    let dur = format_duration(cam_ref.recording_duration());
                    let _ = handle.upgrade_in_event_loop(move |ui| ui.set_rec_duration(dur.into()));
                }
            })
            .expect("spawn rec-ticker");
    }

    // ── Settings sync: push UI settings to camera every 500ms ────────────────
    {
        let cam_ref = Arc::clone(&cam);
        let handle  = handle.clone();
        thread::Builder::new()
            .name("settings-sync".into())
            .spawn(move || loop {
                thread::sleep(Duration::from_millis(500));
                let cam_inner = Arc::clone(&cam_ref);
                let _ = handle.upgrade_in_event_loop(move |ui| {
                    cam_inner.update_settings(read_settings(&ui));
                });
            })
            .expect("spawn settings-sync");
    }

    // ── Gallery refresh helper ────────────────────────────────────────────────
    let refresh_gallery = {
        let handle = handle.clone();
        move || {
            // Build plain Vec (Send) before entering event loop; create ModelRc inside.
            let model_data: Vec<GalleryItem> = gallery::scan(&[SAVE_DIR, VIDEO_DIR, TL_DIR])
                .into_iter()
                .map(|i| GalleryItem {
                    path:     i.path.into(),
                    name:     i.name.into(),
                    is_video: i.is_video,
                })
                .collect();
            let _ = handle.upgrade_in_event_loop(move |ui| {
                ui.set_gallery_items(ModelRc::new(VecModel::from(model_data)));
            });
        }
    };

    // Initial gallery load
    {
        let refresh = refresh_gallery.clone();
        thread::spawn(refresh);
    }

    // ── Photo capture ─────────────────────────────────────────────────────────
    {
        let cam_ref = Arc::clone(&cam);
        let handle  = handle.clone();
        let refresh = refresh_gallery.clone();
        app.on_capture_photo(move || {
            let cam_ref = Arc::clone(&cam_ref);
            let handle  = handle.clone();
            let refresh = refresh.clone();

            // Snapshot everything we need from the UI on the event-loop thread
            let (settings, res_idx, raw, timer_idx, burst_idx, review_on) =
                handle.upgrade().map(|ui| (
                    read_settings(&ui),
                    ui.get_photo_res_idx() as usize,
                    ui.get_raw_enabled(),
                    ui.get_self_timer_idx() as usize,
                    ui.get_burst_count_idx() as usize,
                    ui.get_last_shot_enabled(),
                )).unwrap_or_default();
            cam_ref.update_settings(settings);
            let (w, h, _) = PHOTO_RESOLUTIONS[res_idx.min(PHOTO_RESOLUTIONS.len() - 1)];
            let timer_secs = SELF_TIMER_SECS[timer_idx.min(SELF_TIMER_SECS.len() - 1)];
            let burst      = BURST_COUNTS    [burst_idx.min(BURST_COUNTS.len() - 1)];

            thread::spawn(move || {
                // Self-timer countdown
                if timer_secs > 0 {
                    for n in (1..=timer_secs).rev() {
                        let _ = handle.upgrade_in_event_loop(move |ui| ui.set_timer_countdown(n));
                        thread::sleep(Duration::from_secs(1));
                    }
                    let _ = handle.upgrade_in_event_loop(|ui| ui.set_timer_countdown(0));
                }

                let _ = handle.upgrade_in_event_loop(|ui| ui.set_capturing(true));

                let paths: Vec<std::path::PathBuf> = if burst > 1 {
                    let h_inner = handle.clone();
                    let on_progress = move |i: usize, n: usize| {
                        let s = format!("{i} / {n}");
                        let _ = h_inner.upgrade_in_event_loop(move |ui| ui.set_burst_status(s.into()));
                    };
                    let r = cam_ref.capture_burst(burst, w, h, raw, on_progress);
                    let _ = handle.upgrade_in_event_loop(|ui| ui.set_burst_status("".into()));
                    r.unwrap_or_default()
                } else {
                    match cam_ref.capture_photo(w, h, raw) {
                        Ok(p)  => vec![p],
                        Err(e) => { eprintln!("[photo] {e:#}"); vec![] }
                    }
                };

                thread::sleep(Duration::from_millis(150));
                let _ = handle.upgrade_in_event_loop(|ui| ui.set_capturing(false));

                if let Some(last) = paths.last().cloned() {
                    // Last-shot review (thumbnail flash for 2 seconds)
                    if review_on {
                        let h2 = handle.clone();
                        let _ = h2.upgrade_in_event_loop(move |ui| {
                            if let Ok(img) = slint::Image::load_from_path(&last) {
                                ui.set_last_shot_image(img);
                                ui.set_show_last_shot(true);
                            }
                        });
                        thread::sleep(Duration::from_secs(2));
                        let _ = handle.upgrade_in_event_loop(|ui| ui.set_show_last_shot(false));
                    }
                    refresh();
                }
            });
        });
    }

    // ── Video recording start ─────────────────────────────────────────────────
    {
        let cam_ref = Arc::clone(&cam);
        let handle  = handle.clone();
        app.on_start_recording(move || {
            let (settings, res_idx) = handle.upgrade()
                .map(|ui| (read_settings(&ui), ui.get_video_res_idx() as usize))
                .unwrap_or_default();
            cam_ref.update_settings(settings);
            let (w, h, _) = VIDEO_RESOLUTIONS[res_idx.min(VIDEO_RESOLUTIONS.len() - 1)];

            match cam_ref.start_recording(w, h) {
                Ok(_)  => { let _ = handle.upgrade_in_event_loop(|ui| ui.set_recording(true)); }
                Err(e) => eprintln!("[rec] start: {e:#}"),
            }
        });
    }

    // ── Video recording stop ──────────────────────────────────────────────────
    // Preview keeps running; ffmpeg transcodes in background, then gallery refreshes.
    {
        let cam_ref = Arc::clone(&cam);
        let handle  = handle.clone();
        let refresh = refresh_gallery.clone();
        app.on_stop_recording(move || {
            // Update UI immediately (we're on event loop thread)
            if let Some(ui) = handle.upgrade() {
                ui.set_recording(false);
                ui.set_rec_duration("00:00".into());
            }
            // Kick off ffmpeg transcode; refresh gallery when done
            let refresh_inner = refresh.clone();
            cam_ref.stop_recording(move || refresh_inner());
        });
    }

    // ── Timelapse start ───────────────────────────────────────────────────────
    {
        let cam_ref = Arc::clone(&cam);
        let tl_ref  = Arc::clone(&tl);
        let handle  = handle.clone();
        app.on_start_timelapse(move || {
            let (interval, duration, settings, res_idx) = handle.upgrade().map(|ui| (
                ui.get_tl_interval() as u64,
                ui.get_tl_duration() as u64,
                read_settings(&ui),
                ui.get_tl_res_idx() as usize,
            )).unwrap_or_else(|| (10, 60, CameraSettings::default(), 0));
            cam_ref.update_settings(settings);
            let (w, h, _) = TL_RESOLUTIONS[res_idx.min(TL_RESOLUTIONS.len() - 1)];

            let _ = handle.upgrade_in_event_loop(|ui| ui.set_tl_state(TlState::Running));

            let handle_frame = handle.clone();
            let handle_done  = handle.clone();

            tl_ref.start(
                Arc::clone(&cam_ref),
                interval,
                duration,
                w, h,
                move |n| {
                    let _ = handle_frame.upgrade_in_event_loop(move |ui| ui.set_tl_frames(n as i32));
                },
                move || {
                    let _ = handle_done.upgrade_in_event_loop(|ui| ui.set_tl_state(TlState::Idle));
                },
            );
        });
    }

    // ── Timelapse stop ────────────────────────────────────────────────────────
    {
        let tl_ref = Arc::clone(&tl);
        let handle = handle.clone();
        app.on_stop_timelapse(move || {
            tl_ref.stop();
            let _ = handle.upgrade_in_event_loop(|ui| ui.set_tl_state(TlState::Idle));
        });
    }

    // ── Timelapse render ──────────────────────────────────────────────────────
    {
        let tl_ref  = Arc::clone(&tl);
        let handle  = handle.clone();
        let refresh = refresh_gallery.clone();
        app.on_render_timelapse(move || {
            let tl_ref  = Arc::clone(&tl_ref);
            let handle  = handle.clone();
            let refresh = refresh.clone();
            let _ = handle.upgrade_in_event_loop(|ui| ui.set_tl_state(TlState::Rendering));
            thread::spawn(move || {
                tl_ref.render();
                let _ = handle.upgrade_in_event_loop(|ui| ui.set_tl_state(TlState::Idle));
                refresh();
            });
        });
    }

    // ── Gallery delete ────────────────────────────────────────────────────────
    {
        let handle  = handle.clone();
        let refresh = refresh_gallery.clone();
        app.on_delete_item(move |idx| {
            if let Some(ui) = handle.upgrade() {
                let items = ui.get_gallery_items();
                if let Some(item) = items.row_data(idx as usize) {
                    gallery::delete(&item.path.to_string());
                }
            }
            refresh();
        });
    }

    // Tab tap triggers a rescan — catches captures from /control, side-channel
    // uploads via SCP, and any other source not routed through on_delete_item.
    {
        let refresh = refresh_gallery.clone();
        app.on_refresh_gallery(move || refresh());
    }

    // ── Video playback (gallery → ▶ button) ───────────────────────────────────
    // Spawns mpv fullscreen on the Pi's Wayland session. mpv is a separate
    // Wayland client to cage; it draws over picam until the user presses Q.
    // We briefly suspend the libcamera preview so the camera isn't competing
    // with mpv for the framebuffer — saves CPU during playback.
    {
        let cam_ref = Arc::clone(&cam);
        app.on_play_video(move |path| {
            let path = path.to_string();
            let was_streaming = cam_ref.is_stream_enabled();
            cam_ref.set_stream_enabled(false);
            let cam_for_resume = Arc::clone(&cam_ref);
            thread::spawn(move || {
                let _ = std::process::Command::new("mpv")
                    .args([
                        "--fs",
                        "--no-osc",
                        "--no-input-default-bindings",
                        "--input-conf=/dev/null",
                        // Q, Esc, or any tap on the screen exits
                        "--really-quiet",
                        "--keep-open=no",
                        &path,
                    ])
                    .status();
                if was_streaming {
                    cam_for_resume.set_stream_enabled(true);
                }
            });
        });
    }

    // ── Stream on/off toggle (Advanced setting) ───────────────────────────────
    {
        let cam_ref = Arc::clone(&cam);
        app.on_toggle_stream(move |on| {
            cam_ref.set_stream_enabled(on);
        });
    }

    // ── Local MJPEG HTTP server toggle ────────────────────────────────────────
    {
        let cam_ref = Arc::clone(&cam);
        let handle  = handle.clone();
        app.on_toggle_local_stream(move |on| {
            if on {
                let port = handle.upgrade().map(|ui| ui.get_stream_port() as u16).unwrap_or(8080);
                match cam_ref.set_local_stream(port) {
                    Ok(()) => {
                        let _ = handle.upgrade_in_event_loop(|ui| ui.set_local_stream_active(true));
                    }
                    Err(e) => {
                        eprintln!("[stream] local start failed: {e:#}");
                        let _ = handle.upgrade_in_event_loop(|ui| ui.set_local_stream_active(false));
                    }
                }
            } else {
                cam_ref.clear_local_stream();
                let _ = handle.upgrade_in_event_loop(|ui| ui.set_local_stream_active(false));
            }
        });
    }

    // ── Push stream toggle ────────────────────────────────────────────────────
    {
        let cam_ref = Arc::clone(&cam);
        let handle  = handle.clone();
        app.on_toggle_push_stream(move |on| {
            if on {
                let url = handle.upgrade().map(|ui| ui.get_push_url().to_string()).unwrap_or_default();
                if url.is_empty() {
                    eprintln!("[stream] push: no URL configured");
                    let _ = handle.upgrade_in_event_loop(|ui| ui.set_push_stream_active(false));
                    return;
                }
                match cam_ref.set_push_stream(&url, 800, 480) {
                    Ok(()) => {
                        let _ = handle.upgrade_in_event_loop(|ui| ui.set_push_stream_active(true));
                    }
                    Err(e) => {
                        eprintln!("[stream] push start failed: {e:#}");
                        let _ = handle.upgrade_in_event_loop(|ui| ui.set_push_stream_active(false));
                    }
                }
            } else {
                cam_ref.clear_push_stream();
                let _ = handle.upgrade_in_event_loop(|ui| ui.set_push_stream_active(false));
            }
        });
    }

    // ── Reload config (re-reads ~/.config/picam-rs/config.toml) ───────────────
    {
        let cam_ref = Arc::clone(&cam);
        let handle  = handle.clone();
        app.on_reload_config(move || {
            let new_cfg = config::Config::load();
            // Apply only the stream-related properties (camera/capture/general
            // could trample in-flight UI state, so we leave those alone).
            let _ = handle.upgrade_in_event_loop(move |ui| {
                ui.set_stream_port(new_cfg.stream.local_port as i32);
                ui.set_push_url(new_cfg.stream.push_url.clone().into());
            });
            // If a stream is currently active and the port/URL changed, the
            // user should toggle off+on to pick up the new value. Document
            // this rather than silently restart streams behind their back.
            let _ = cam_ref; // keeps reference alive
        });
    }

    // ── Gallery item selected — load the image (or video thumbnail) ──────────
    {
        let handle = handle.clone();
        app.on_select_gallery_item(move |idx| {
            let Some(ui) = handle.upgrade() else { return };
            let items = ui.get_gallery_items();
            let Some(item) = items.row_data(idx as usize) else { return };

            let path = item.path.to_string();
            // For videos: load the thumbnail (<name>.thumb.jpg) if it exists.
            let load_path = if item.is_video {
                let p = std::path::PathBuf::from(&path);
                p.with_extension("thumb.jpg")
            } else {
                std::path::PathBuf::from(&path)
            };

            match slint::Image::load_from_path(&load_path) {
                Ok(img) => ui.set_gallery_preview_image(img),
                Err(_)  => ui.set_gallery_preview_image(slint::Image::default()),
            }
        });
    }

    // ── /control settings applier ─────────────────────────────────────────────
    // The HTTP server posts a JSON patch (e.g. {"iso_idx":3,"ev":0.5}) to
    // /api/settings; this closure ports each known key into the Slint UI's
    // properties. The existing 500ms settings-sync thread then propagates
    // them to the camera. One-way for now: writes via /control land in the
    // touchscreen UI too.
    {
        let handle = handle.clone();
        let applier: camera::SettingsApplier = Arc::new(move |patch: serde_json::Value| {
            let h = handle.clone();
            let _ = h.upgrade_in_event_loop(move |ui| {
                if let Some(o) = patch.as_object() {
                    if let Some(v) = o.get("iso_idx").and_then(|v| v.as_i64()) {
                        ui.set_iso_idx(v as i32);
                    }
                    if let Some(v) = o.get("shutter_idx").and_then(|v| v.as_i64()) {
                        ui.set_shutter_idx(v as i32);
                    }
                    if let Some(v) = o.get("awb_idx").and_then(|v| v.as_i64()) {
                        ui.set_awb_idx(v as i32);
                    }
                    if let Some(v) = o.get("ev").and_then(|v| v.as_f64()) {
                        ui.set_ev(v as f32);
                    }
                    if let Some(v) = o.get("zoom").and_then(|v| v.as_f64()) {
                        ui.set_zoom(v as f32);
                    }
                    if let Some(v) = o.get("contrast").and_then(|v| v.as_f64()) {
                        ui.set_contrast(v as f32);
                    }
                    if let Some(v) = o.get("saturation").and_then(|v| v.as_f64()) {
                        ui.set_saturation(v as f32);
                    }
                    if let Some(v) = o.get("sharpness").and_then(|v| v.as_f64()) {
                        ui.set_sharpness(v as f32);
                    }
                    if let Some(v) = o.get("brightness").and_then(|v| v.as_f64()) {
                        ui.set_brightness(v as f32);
                    }
                }
            });
        });
        cam.set_settings_applier(applier);
    }

    // ── Backlight off during screen sleep ─────────────────────────────────────
    // Slint fires `sleeping-changed` whenever the screen-sleep state flips.
    // We translate that into a brightness write to /sys/class/backlight.
    // Detect once at startup; if the device isn't present (running off-Pi for
    // dev) or we can't write to it, the closure is a no-op.
    //
    // Always force-on at startup. If a previous picam instance was killed
    // while sleeping, the LEDs are still at 0 — nothing else writes max
    // until the next sleep/wake cycle, so the screen would stay black for
    // the user even though picam is healthy. Forcing on here recovers.
    if let Some(backlight) = backlight::Backlight::detect() {
        backlight.on();
        let backlight = Arc::new(backlight);
        app.on_sleeping_changed(move |sleeping| {
            if sleeping { backlight.off(); } else { backlight.on(); }
        });
    }

    // ── Power actions ─────────────────────────────────────────────────────────
    {
        let cam_ref = Arc::clone(&cam);
        app.on_shutdown_system(move || {
            cam_ref.stop();
            thread::sleep(Duration::from_millis(500));
            let _ = std::process::Command::new("sudo").args(["shutdown", "-h", "now"]).spawn();
        });
    }

    {
        let cam_ref = Arc::clone(&cam);
        app.on_reboot_system(move || {
            cam_ref.stop();
            thread::sleep(Duration::from_millis(500));
            let _ = std::process::Command::new("sudo").args(["reboot"]).spawn();
        });
    }

    app.on_restart_app(|| std::process::exit(1));

    // ── Run ───────────────────────────────────────────────────────────────────
    app.run().expect("Slint run loop");
    cam.stop();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_duration_zero() {
        assert_eq!(format_duration(Duration::from_secs(0)), "00:00");
    }

    #[test]
    fn format_duration_under_one_minute() {
        assert_eq!(format_duration(Duration::from_secs(45)), "00:45");
    }

    #[test]
    fn format_duration_exactly_one_minute() {
        assert_eq!(format_duration(Duration::from_secs(60)), "01:00");
    }

    #[test]
    fn format_duration_mixed() {
        assert_eq!(format_duration(Duration::from_secs(125)), "02:05");
    }

    #[test]
    fn format_duration_over_one_hour() {
        assert_eq!(format_duration(Duration::from_secs(3661)), "61:01");
    }

    #[test]
    fn timestamp_ms_is_nonzero() {
        assert!(timestamp_ms() > 0);
    }

    #[test]
    fn timestamp_ms_advances() {
        let t1 = timestamp_ms();
        std::thread::sleep(Duration::from_millis(2));
        let t2 = timestamp_ms();
        assert!(t2 > t1);
    }
}
