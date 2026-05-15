mod camera;
mod gallery;
mod timelapse;

use std::sync::Arc;
use std::time::Duration;
use std::thread;

use slint::{Model, ModelRc, SharedPixelBuffer, Rgba8Pixel, VecModel};

use camera::{
    Camera, CameraEvent, CameraSettings,
    SAVE_DIR, VIDEO_DIR, TL_DIR,
    PHOTO_RESOLUTIONS, VIDEO_RESOLUTIONS, TL_RESOLUTIONS,
};

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

    let cam = Arc::new(Camera::new());

    let tl = Arc::new(timelapse::Timelapse::new());

    let app    = AppWindow::new().expect("Slint init");
    let handle = app.as_weak();

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

            // Read settings + resolution while on event-loop thread
            let (settings, res_idx) = handle.upgrade()
                .map(|ui| (read_settings(&ui), ui.get_photo_res_idx() as usize))
                .unwrap_or_default();
            cam_ref.update_settings(settings);
            let (w, h, _) = PHOTO_RESOLUTIONS[res_idx.min(PHOTO_RESOLUTIONS.len() - 1)];

            thread::spawn(move || {
                let _ = handle.upgrade_in_event_loop(|ui| ui.set_capturing(true));
                let result = cam_ref.capture_photo(w, h);
                thread::sleep(Duration::from_millis(150));
                let _ = handle.upgrade_in_event_loop(|ui| ui.set_capturing(false));
                if let Err(e) = result { eprintln!("[photo] {e:#}"); } else { refresh(); }
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

    // ── Stream on/off toggle (Advanced setting) ───────────────────────────────
    {
        let cam_ref = Arc::clone(&cam);
        app.on_toggle_stream(move |on| {
            cam_ref.set_stream_enabled(on);
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
