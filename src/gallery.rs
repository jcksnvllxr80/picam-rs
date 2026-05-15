use std::fs;
use std::path::Path;

#[derive(Clone, Debug)]
pub struct GalleryItem {
    pub path:     String,
    pub name:     String,
    pub is_video: bool,
}

pub fn scan(dirs: &[&str]) -> Vec<GalleryItem> {
    let mut items = vec![];
    for &dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else { continue };
        let mut paths: Vec<_> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        paths.sort_by(|a, b| b.cmp(a)); // newest first
        for p in paths {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            // Skip video thumbnails (VID_*.thumb.jpg) — they back the gallery preview
            // and shouldn't show as separate items.
            if name.ends_with(".thumb.jpg") { continue; }

            let ext = p.extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            let is_video = matches!(ext.as_str(), "mp4" | "h264" | "mkv");
            let is_photo = matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "dng");
            if is_video || is_photo {
                items.push(GalleryItem {
                    path: p.to_string_lossy().into_owned(),
                    name,
                    is_video,
                });
            }
        }
    }
    items
}

pub fn delete(path: &str) -> bool {
    if !Path::new(path).exists() {
        return false;
    }
    // Also remove any sidecar thumbnail (e.g. VID_*.thumb.jpg) so the gallery
    // stays consistent.
    let p = Path::new(path);
    let thumb = p.with_extension("thumb.jpg");
    if thumb.exists() {
        let _ = fs::remove_file(&thumb);
    }
    fs::remove_file(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn scan_empty_dir_returns_empty() {
        let dir = tempdir();
        let items = scan(&[dir.to_str().unwrap()]);
        assert!(items.is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_classifies_extensions() {
        let dir = tempdir();
        for name in &["a.jpg", "b.mp4", "c.txt", "d.png", "e.h264"] {
            fs::write(dir.join(name), b"x").unwrap();
        }
        let items = scan(&[dir.to_str().unwrap()]);
        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
        // .txt must be excluded
        assert!(!names.contains(&"c.txt"));
        // jpg and png are photos
        let jpg = items.iter().find(|i| i.name == "a.jpg").unwrap();
        assert!(!jpg.is_video);
        // mp4 and h264 are videos
        let mp4 = items.iter().find(|i| i.name == "b.mp4").unwrap();
        assert!(mp4.is_video);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delete_existing_file_returns_true() {
        let dir = tempdir();
        let p = dir.join("test.jpg");
        fs::write(&p, b"data").unwrap();
        assert!(delete(p.to_str().unwrap()));
        assert!(!p.exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delete_missing_file_returns_false() {
        assert!(!delete("/tmp/picam_test_nonexistent_xyz.jpg"));
    }

    #[test]
    fn scan_missing_dir_returns_empty() {
        let items = scan(&["/tmp/picam_test_no_such_dir_xyz"]);
        assert!(items.is_empty());
    }

    fn tempdir() -> std::path::PathBuf {
        let p = std::path::PathBuf::from(format!("/tmp/picam_test_{}", timestamp_ns()));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn timestamp_ns() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }
}
