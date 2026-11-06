//! Clipboard history pages, pins and deletion on top of cliphist. Images are decoded into the cache
//! directory as `<id>.png`, and the pinned ids live there in `pinned.json`.

use std::fs::File;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::json;

const IMAGE_EXTENSIONS: &[&str] =
    &["png", "jpg", "jpeg", "webp", "gif", "bmp", "svg", "ico", "tiff", "tif", "avif", "jxl"];
const DECODE_THREADS: usize = 8;
/// Decoded images of entries past this many are removed
const KEEP_IMAGES: usize = 100;

fn cliphist(args: &[&str]) -> String {
    Command::new("cliphist")
        .args(args)
        .stderr(Stdio::null())
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .unwrap_or_default()
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = text.get(i + 1..i + 3).and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn file_url_path(url: &str) -> Option<String> {
    let rest = url.strip_prefix("file://")?;
    let path = &rest[rest.find('/')?..];
    let path = path.split(['?', '#']).next().unwrap_or(path);
    Some(percent_decode(path))
}

fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        && path.is_file()
}

/// `~/x` in `home` and `~user/x` in that user's home, as a shell expands them
pub fn expand_tilde(text: &str, home: &Path) -> Option<PathBuf> {
    let rest = text.strip_prefix('~')?;
    let (user, path) = rest.split_once('/').unwrap_or((rest, ""));
    let base = if user.is_empty() {
        home.to_path_buf()
    } else {
        let name = std::ffi::CString::new(user).ok()?;
        // SAFETY: name is a valid C string; the entry is read before any other passwd call
        let entry = unsafe { libc::getpwnam(name.as_ptr()) };
        if entry.is_null() {
            return None;
        }
        // SAFETY: a non-null entry has a valid pw_dir string
        let dir = unsafe { std::ffi::CStr::from_ptr((*entry).pw_dir) };
        PathBuf::from(std::ffi::OsStr::from_bytes(dir.to_bytes()))
    };
    Some(if path.is_empty() { base } else { base.join(path) })
}

/// The first existing image file the text names, as a path, `~/` path or `file://` URL
pub fn image_path(text: &str, home: &Path) -> Option<PathBuf> {
    let text = text.trim();
    let mut candidates: Vec<PathBuf> = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(path) = file_url_path(line) {
            candidates.push(path.into());
        } else if line.starts_with('~') {
            candidates.extend(expand_tilde(line, home));
        } else if line.starts_with('/') {
            candidates.push(line.into());
        }
    }
    candidates.extend(text.split_whitespace().filter_map(file_url_path).map(PathBuf::from));
    candidates.into_iter().find(|p| is_image(p))
}

fn pinned_file(cache: &Path) -> PathBuf {
    cache.join("pinned.json")
}

pub fn pinned(cache: &Path) -> Vec<String> {
    std::fs::read_to_string(pinned_file(cache))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn save_pinned(cache: &Path, ids: &[String]) {
    let _ = std::fs::create_dir_all(cache);
    let _ = std::fs::write(pinned_file(cache), serde_json::to_string(ids).unwrap_or_else(|_| "[]".into()));
}

pub fn toggle_pin(cache: &Path, id: &str) {
    let mut ids = pinned(cache);
    match ids.iter().position(|p| p == id) {
        Some(index) => {
            ids.remove(index);
        }
        None => ids.push(id.to_owned()),
    }
    save_pinned(cache, &ids);
}

pub fn delete(cache: &Path, id: &str) {
    let prefix = format!("{id}\t");
    if let Some(line) = cliphist(&["list"]).lines().find(|l| l.starts_with(&prefix))
        && let Ok(mut child) =
            Command::new("cliphist").arg("delete").stdin(Stdio::piped()).stderr(Stdio::null()).spawn()
    {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(line.as_bytes());
        }
        let _ = child.wait();
    }
    let _ = std::fs::remove_file(cache.join(format!("{id}.png")));
    let ids: Vec<String> = pinned(cache).into_iter().filter(|p| p != id).collect();
    save_pinned(cache, &ids);
}

pub fn wipe(cache: &Path) {
    let _ = Command::new("cliphist").arg("wipe").stderr(Stdio::null()).status();
    for entry in std::fs::read_dir(cache).into_iter().flatten().flatten() {
        let _ = std::fs::remove_file(entry.path());
    }
}

/// Removes decoded images of entries that are gone or past the newest `KEEP_IMAGES`
fn clean_images(cache: &Path, ids: &[&str]) {
    let keep: Vec<&str> = ids.iter().take(KEEP_IMAGES).copied().collect();
    for entry in std::fs::read_dir(cache).into_iter().flatten().flatten() {
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(|n| n.strip_suffix(".png")) else { continue };
        if !keep.contains(&id) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn decode_to(id: &str, path: &Path) {
    if path.exists() {
        return;
    }
    let ok = File::create(path).ok().and_then(|file| {
        Command::new("cliphist").args(["decode", id]).stdout(file).stderr(Stdio::null()).status().ok()
    });
    if !ok.is_some_and(|status| status.success()) {
        let _ = std::fs::remove_file(path);
    }
}

fn decode_all(jobs: &[(String, PathBuf)]) {
    if jobs.is_empty() {
        return;
    }
    let per_thread = jobs.len().div_ceil(DECODE_THREADS);
    std::thread::scope(|scope| {
        for chunk in jobs.chunks(per_thread) {
            scope.spawn(move || chunk.iter().for_each(|(id, path)| decode_to(id, path)));
        }
    });
}

fn looks_like_a_file_copy(content: &str) -> bool {
    content.contains("nautilus") || content.contains("clipboard") || content == "copy" || content.starts_with("file:")
}

/// One page of the history, newest first, as a JSON array of {id, content, type, pinned}
pub fn list(cache: &Path, home: &Path, offset: usize, limit: usize) -> String {
    let _ = std::fs::create_dir_all(cache);
    let history = cliphist(&["list"]);
    let entries: Vec<(&str, &str)> = history.lines().filter_map(|l| l.split_once('\t')).collect();
    if offset == 0 {
        let ids: Vec<&str> = entries.iter().map(|(id, _)| *id).collect();
        clean_images(cache, &ids);
    }
    let pinned = pinned(cache);
    let mut jobs = Vec::new();
    let items: Vec<serde_json::Value> = entries
        .iter()
        .skip(offset)
        .take(limit)
        .map(|&(id, content)| {
            let (kind, shown) = if content.contains("[[ binary data") {
                let path = cache.join(format!("{id}.png"));
                let shown = path.to_string_lossy().into_owned();
                jobs.push((id.to_owned(), path));
                ("image", shown)
            } else {
                let image = image_path(content, home).or_else(|| {
                    looks_like_a_file_copy(content).then(|| image_path(&cliphist(&["decode", id]), home)).flatten()
                });
                match image {
                    Some(path) => ("image", path.to_string_lossy().into_owned()),
                    None => ("text", content.trim().to_owned()),
                }
            };
            json!({ "id": id, "content": shown, "type": kind, "pinned": pinned.iter().any(|p| p == id) })
        })
        .collect();
    decode_all(&jobs);
    serde_json::Value::Array(items).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_urls_are_percent_decoded() {
        assert_eq!(file_url_path("file:///home/me/My%20Pics/a.png").as_deref(), Some("/home/me/My Pics/a.png"));
        assert_eq!(file_url_path("file://host/tmp/x.jpg").as_deref(), Some("/tmp/x.jpg"));
        assert_eq!(file_url_path("/tmp/x.jpg"), None);
        assert_eq!(percent_decode("100%"), "100%");
    }

    #[test]
    fn finds_existing_images_by_path_url_or_home() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("shot one.PNG");
        std::fs::write(&image, b"x").unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"x").unwrap();
        let url = format!("file://{}", dir.path().join("shot%20one.PNG").display());
        assert_eq!(image_path(&url, Path::new("/")), Some(image.clone()));
        assert_eq!(image_path(&format!("  {}\n", image.display()), Path::new("/")), Some(image.clone()));
        assert_eq!(image_path("~/shot one.PNG", dir.path()), Some(image));
        assert_eq!(expand_tilde("~", dir.path()), Some(dir.path().to_path_buf()));
        // SAFETY: the entry of the current user is read before any other passwd call
        let (name, home) = unsafe {
            let entry = libc::getpwuid(libc::getuid());
            assert!(!entry.is_null());
            let name = std::ffi::CStr::from_ptr((*entry).pw_name).to_string_lossy().into_owned();
            (name, PathBuf::from(std::ffi::OsStr::from_bytes(std::ffi::CStr::from_ptr((*entry).pw_dir).to_bytes())))
        };
        assert_eq!(expand_tilde(&format!("~{name}/x.png"), dir.path()), Some(home.join("x.png")));
        assert_eq!(expand_tilde("~no-such-user-here/x.png", dir.path()), None);
        assert_eq!(image_path(&dir.path().join("notes.txt").to_string_lossy(), Path::new("/")), None);
        assert_eq!(image_path("hello world", Path::new("/")), None);
    }

    #[test]
    fn pins_toggle_and_survive_a_reread() {
        let dir = tempfile::tempdir().unwrap();
        toggle_pin(dir.path(), "12");
        toggle_pin(dir.path(), "7");
        assert_eq!(pinned(dir.path()), ["12", "7"]);
        toggle_pin(dir.path(), "12");
        assert_eq!(pinned(dir.path()), ["7"]);
    }

    #[test]
    fn cleaning_keeps_images_of_listed_entries_and_other_files() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["1.png", "2.png", "pinned.json"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        clean_images(dir.path(), &["2", "3"]);
        assert!(!dir.path().join("1.png").exists());
        assert!(dir.path().join("2.png").exists());
        assert!(dir.path().join("pinned.json").exists());
    }
}
