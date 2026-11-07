//! The wallpaper picker's index of a folder: every image with its average colour and colour bucket,
//! every video with a poster frame from ffmpeg. Entries whose file kept its mtime and size come from
//! the previous index unchanged. Runs for the same folder are serialized by a lock file in the run
//! directory: a run that finds it taken marks the index dirty, prints the index as it stands and
//! leaves, and the run holding the lock indexes again until nothing marked it dirty meanwhile.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use image::{DynamicImage, ImageReader, Rgb, RgbImage};
use rayon::prelude::*;
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};

const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif", "bmp"];
const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mkv", "mov", "webm"];
const FALLBACK_HEX: &str = "#808080";
const MAGICK_TIMEOUT: Duration = Duration::from_secs(5);
const FFMPEG_TIMEOUT: Duration = Duration::from_secs(8);
const POSTER_SEEKS: &[Option<&str>] = &[Some("00:00:01"), Some("00:00:00.5"), Some("00:00:02"), None];
const POSTER_PLACEHOLDER: Rgb<u8> = Rgb([0x1e, 0x1e, 0x2e]);
const POLL: Duration = Duration::from_millis(20);

/// A file the index lists
struct Entry {
    name: String,
    path: String,
    mtime: i64,
    size: u64,
    video: bool,
}

/// `file:///a%20b` as `/a b`, and anything else as it is
fn local_path(src: &str) -> String {
    let Some(rest) = src.strip_prefix("file://") else { return src.to_owned() };
    let bytes = rest.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(hi), Some(lo)) =
                (bytes.get(i + 1).copied().and_then(hex), bytes.get(i + 2).copied().and_then(hex))
        {
            decoded.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// The absolute form of `path` with `.` and `..` resolved by name, as Python's `os.path.abspath`
fn absolute(path: &str) -> String {
    let joined = if path.starts_with('/') {
        PathBuf::from(path)
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")).join(path)
    };
    let mut parts: Vec<&std::ffi::OsStr> = Vec::new();
    for component in joined.components() {
        match component {
            Component::Normal(part) => parts.push(part),
            Component::ParentDir => {
                parts.pop();
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    let mut out = String::from("/");
    out.push_str(&parts.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>().join("/"));
    out
}

fn short_hash(text: &str) -> String {
    Sha256::digest(text.as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// The colour group the picker filters by, from a hue, saturation and value of `hex`
pub fn color_bucket(hex: &str) -> &'static str {
    let hex = hex.trim().trim_start_matches('#');
    let hex = hex.get(..6).unwrap_or(hex);
    if hex.len() != 6 {
        return "Monochrome";
    }
    let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok().map(|c| f64::from(c) / 255.0);
    let (Some(r), Some(g), Some(b)) = (channel(0), channel(2), channel(4)) else { return "Monochrome" };
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let s = if max == 0.0 { 0.0 } else { d / max };
    let mut h = 0.0;
    if max != min {
        h = if max == r {
            (g - b) / d + if g < b { 6.0 } else { 0.0 }
        } else if max == g {
            (b - r) / d + 2.0
        } else {
            (r - g) / d + 4.0
        };
        h /= 6.0;
    }
    h *= 360.0;
    if s < 0.05 || max < 0.08 {
        "Monochrome"
    } else if !(15.0..345.0).contains(&h) {
        "Red"
    } else if h < 45.0 {
        "Orange"
    } else if h < 75.0 {
        "Yellow"
    } else if h < 165.0 {
        "Green"
    } else if h < 260.0 {
        "Blue"
    } else if h < 315.0 {
        "Purple"
    } else {
        "Pink"
    }
}

/// The colour of the image shrunk to one pixel with a box filter, as Pillow shrinks it: each row
/// averaged and rounded, then the rows
fn average(image: &DynamicImage) -> [u8; 3] {
    let owned;
    let rgb = match image {
        DynamicImage::ImageRgb8(rgb) => rgb,
        _ => {
            owned = image.to_rgb8();
            &owned
        }
    };
    let (width, height) = (u64::from(rgb.width()), u64::from(rgb.height()));
    if width == 0 || height == 0 {
        return [0x80; 3];
    }
    let mut totals = [0u64; 3];
    for row in rgb.rows() {
        let mut sums = [0u64; 3];
        for pixel in row {
            for (sum, value) in sums.iter_mut().zip(pixel.0) {
                *sum += u64::from(value);
            }
        }
        for (total, sum) in totals.iter_mut().zip(sums) {
            *total += (sum * 2 + width) / (width * 2);
        }
    }
    totals.map(|total| ((total * 2 + height) / (height * 2)) as u8)
}

fn hex(rgb: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2])
}

/// Waits for `child` up to `timeout`, killing it after, and tells whether it exited successfully
fn finish(mut child: Child, timeout: Duration) -> Option<Child> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success().then_some(child),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// ImageMagick's one-pixel colour of the first frame, for files the decoders here cannot read
fn magick_color(path: &Path) -> Option<String> {
    for program in ["magick", "convert"] {
        let child = Command::new(program)
            .arg(format!("{}[0]", path.display()))
            .args(["-resize", "1x1!", "-format", "%[hex:p{0,0}]", "info:-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let Ok(child) = child else { continue };
        let Some(child) = finish(child, MAGICK_TIMEOUT) else { continue };
        let mut out = String::new();
        if let Some(mut stdout) = child.stdout {
            let _ = std::io::Read::read_to_string(&mut stdout, &mut out);
        }
        let out = out.trim();
        if out.len() >= 6 && out.is_char_boundary(6) {
            return Some(format!("#{}", &out[..6]).to_lowercase());
        }
    }
    None
}

fn extract_color(path: &Path) -> String {
    let decoded = ImageReader::open(path).and_then(|r| r.with_guessed_format()).ok().and_then(|r| r.decode().ok());
    match decoded {
        Some(image) => hex(average(&image)),
        None => magick_color(path).unwrap_or_else(|| FALLBACK_HEX.to_owned()),
    }
}

fn size_of(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

/// A poster frame of the video, 960 pixels wide, from about a second in; a plain dark frame when
/// ffmpeg cannot make one
fn generate_poster(video: &Path, poster: &Path) {
    if size_of(poster) > 1024 {
        return;
    }
    if let Some(parent) = poster.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut tmp = poster.as_os_str().to_owned();
    tmp.push(".tmp.jpg");
    let tmp = PathBuf::from(tmp);
    for seek in POSTER_SEEKS {
        let mut ffmpeg = Command::new("ffmpeg");
        ffmpeg.args(["-y", "-hide_banner", "-loglevel", "error"]);
        if let Some(seek) = seek {
            ffmpeg.args(["-ss", seek]);
        }
        ffmpeg.arg("-i").arg(video);
        ffmpeg.args(["-vframes", "1", "-vf", "scale=960:-2:force_original_aspect_ratio=decrease", "-q:v", "2"]);
        ffmpeg.arg(&tmp).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        let Ok(child) = ffmpeg.spawn() else { break };
        if finish(child, FFMPEG_TIMEOUT).is_some() && size_of(&tmp) > 512 && std::fs::rename(&tmp, poster).is_ok() {
            return;
        }
    }
    let _ = std::fs::remove_file(&tmp);
    let _ = RgbImage::from_pixel(960, 540, POSTER_PLACEHOLDER).save_with_format(poster, image::ImageFormat::Jpeg);
}

fn unchanged(existing: &Json, entry: &Entry) -> bool {
    existing.get("mtime") == Some(&json!(entry.mtime)) && existing.get("size") == Some(&json!(entry.size))
}

fn index_entry(entry: &Entry, posters: &Path, cached: &HashMap<String, Json>) -> Json {
    if let Some(existing) = cached.get(&entry.name).filter(|e| unchanged(e, entry)) {
        let poster = existing.get("posterPath").and_then(Json::as_str).unwrap_or_default();
        if !entry.video || (!poster.is_empty() && size_of(Path::new(poster)) > 512) {
            return existing.clone();
        }
    }
    let file_url = format!("file://{}", entry.path);
    if entry.video {
        let poster = posters.join(format!("{}.jpg", short_hash(&entry.name)));
        generate_poster(Path::new(&entry.path), &poster);
        let color = if poster.exists() { extract_color(&poster) } else { FALLBACK_HEX.to_owned() };
        let poster = poster.to_string_lossy();
        json!({
            "fileName": entry.name, "filePath": entry.path, "fileUrl": file_url, "isVideo": true,
            "posterPath": poster, "posterUrl": format!("file://{poster}"), "hex": color, "bucket": "Video",
            "mtime": entry.mtime, "size": entry.size,
        })
    } else {
        let color = extract_color(Path::new(&entry.path));
        let bucket = color_bucket(&color);
        json!({
            "fileName": entry.name, "filePath": entry.path, "fileUrl": file_url, "isVideo": false,
            "posterPath": "", "posterUrl": "", "hex": color, "bucket": bucket,
            "mtime": entry.mtime, "size": entry.size,
        })
    }
}

/// The images and videos directly in `src`, each file once even when linked under several names
fn scan(src: &str) -> Vec<Entry> {
    let Ok(dir) = std::fs::read_dir(src) else { return Vec::new() };
    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    for item in dir.flatten() {
        let Ok(name) = item.file_name().into_string() else { continue };
        let path = item.path();
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        if !meta.is_file() {
            continue;
        }
        let extension = Path::new(&name).extension().and_then(|e| e.to_str()).unwrap_or_default().to_lowercase();
        let video = VIDEO_EXTENSIONS.contains(&extension.as_str());
        if !video && !IMAGE_EXTENSIONS.contains(&extension.as_str()) {
            continue;
        }
        let Ok(real) = std::fs::canonicalize(&path) else { continue };
        if !seen.insert(real) {
            continue;
        }
        entries.push(Entry {
            path: format!("{}/{}", src.trim_end_matches('/'), name),
            name,
            mtime: meta.mtime(),
            size: meta.len(),
            video,
        });
    }
    entries
}

fn read_cached(index: &Path) -> HashMap<String, Json> {
    let Ok(text) = std::fs::read_to_string(index) else { return HashMap::new() };
    let Ok(data) = serde_json::from_str::<Json>(&text) else { return HashMap::new() };
    let items = data.get("items").and_then(Json::as_array).cloned().unwrap_or_default();
    items
        .into_iter()
        .filter_map(|item| {
            let name = item.get("fileName").and_then(Json::as_str).filter(|n| !n.is_empty())?.to_owned();
            Some((name, item))
        })
        .collect()
}

fn write_atomically(path: &Path, text: &str) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

fn workers() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(4, 12)
}

struct Folder {
    src: String,
    hash: String,
    cache: PathBuf,
    dir: PathBuf,
}

impl Folder {
    fn index_file(&self) -> PathBuf {
        self.dir.join("index.json")
    }

    fn run(&self) -> Result<Json, String> {
        let posters = self.dir.join("posters");
        std::fs::create_dir_all(&posters).map_err(|e| format!("cannot create {}: {e}", posters.display()))?;
        let cached = read_cached(&self.index_file());
        let entries = scan(&self.src);
        let pool = rayon::ThreadPoolBuilder::new().num_threads(workers()).build().map_err(|e| e.to_string())?;
        let mut items: Vec<Json> = pool.install(|| {
            entries
                .par_iter()
                .filter_map(|entry| {
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| index_entry(entry, &posters, &cached)))
                        .ok()
                })
                .collect()
        });
        let name = |item: &Json| item.get("fileName").and_then(Json::as_str).unwrap_or_default().to_owned();
        items.sort_by_key(name);
        items.dedup_by_key(|item| name(item));
        let updated = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let data = json!({ "srcDir": self.src, "dirHash": self.hash, "updatedAt": updated, "items": items });
        let pretty = serde_json::to_string_pretty(&data).map_err(|e| e.to_string())?;
        write_atomically(&self.index_file(), &pretty).map_err(|e| format!("cannot write the index: {e}"))?;
        write_atomically(&self.cache.join("current_index.json"), &data.to_string())
            .map_err(|e| format!("cannot write the current index: {e}"))?;
        Ok(data)
    }
}

/// Indexes `src` (a path or a file URL) into `cache`, locking in `run`, and returns what to print:
/// the new index, or the last one when another run holds the lock
pub fn index(src: &str, cache: &Path, run: &Path) -> Result<String, String> {
    for dir in [cache, run] {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let src = absolute(&local_path(src));
    let hash = short_hash(&src);
    let folder = Folder { dir: cache.join("dirs").join(&hash), cache: cache.to_owned(), src, hash };
    let dirty = run.join(format!("indexer_{}.dirty", folder.hash));
    let lock_path = run.join(format!("indexer_{}.lock", folder.hash));
    let lock = File::create(&lock_path).map_err(|e| format!("cannot open {}: {e}", lock_path.display()))?;
    // SAFETY: flock only takes the lock of a descriptor this function owns
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let _ = std::fs::write(&dirty, "1");
        return Ok(std::fs::read_to_string(folder.index_file()).unwrap_or_default());
    }
    loop {
        let _ = std::fs::remove_file(&dirty);
        let data = folder.run()?;
        if !dirty.exists() {
            return Ok(data.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_follow_hue_saturation_and_value() {
        assert_eq!(color_bucket("#ff0000"), "Red");
        assert_eq!(color_bucket("#ff8000"), "Orange");
        assert_eq!(color_bucket("#ffff00"), "Yellow");
        assert_eq!(color_bucket("#00ff00"), "Green");
        assert_eq!(color_bucket("#0000ff"), "Blue");
        assert_eq!(color_bucket("#8000ff"), "Purple");
        assert_eq!(color_bucket("#ff00aa"), "Pink");
        assert_eq!(color_bucket("#808080"), "Monochrome");
        assert_eq!(color_bucket("#100000"), "Monochrome");
        assert_eq!(color_bucket("#ff0000aa"), "Red");
        assert_eq!(color_bucket("xyz"), "Monochrome");
        assert_eq!(color_bucket(""), "Monochrome");
    }

    #[test]
    fn hashes_match_the_python_indexer() {
        assert_eq!(short_hash("/home/user/Pictures/Wallpapers"), "402f0272b046cd61");
    }

    #[test]
    fn paths_are_made_absolute_by_name() {
        assert_eq!(absolute("/a/b/../c/./d/"), "/a/c/d");
        assert_eq!(absolute("/"), "/");
        assert_eq!(local_path("file:///home/u/My%20Walls"), "/home/u/My Walls");
        assert_eq!(local_path("/plain/100%"), "/plain/100%");
    }

    #[test]
    fn average_rounds_rows_then_columns() {
        let mut image = RgbImage::new(2, 2);
        image.put_pixel(0, 0, Rgb([0, 0, 0]));
        image.put_pixel(1, 0, Rgb([1, 255, 10]));
        image.put_pixel(0, 1, Rgb([0, 0, 0]));
        image.put_pixel(1, 1, Rgb([0, 255, 11]));
        assert_eq!(average(&DynamicImage::ImageRgb8(image)), [1, 128, 6]);
    }
}
