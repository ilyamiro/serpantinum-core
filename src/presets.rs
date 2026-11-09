//! The settings presets: JSON files in a presets directory, each holding the settings, the layout
//! snapshots and the widget layouts of the moment it was saved, next to the screenshots of its bar,
//! dock and widgets. Every mode but `widgets` ends by listing the presets.

use std::collections::BTreeSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value as Json};

/// What one `presets` command asks for
pub struct Request<'a> {
    pub mode: &'a str,
    pub presets_dir: &'a Path,
    pub widgets_dir: &'a Path,
    pub path: Option<&'a Path>,
    pub json: Option<&'a str>,
}

/// Runs a mode and returns the line to print, if any
pub fn run(request: &Request) -> Result<Option<String>, String> {
    let dir = request.presets_dir;
    match request.mode {
        "save" => {
            let (path, text) = path_and_json(request)?;
            let mut payload = parse(text)?;
            let Json::Object(payload_map) = &mut payload else { return Err("the preset is not a JSON object".into()) };
            payload_map.insert("widgets".to_owned(), Json::Object(read_widgets(request.widgets_dir)));
            let old = snapshot_files(path)?;
            write(dir, path, &payload)?;
            remove_files(dir, old.difference(&snapshot_files(path)?))?;
        }
        "widgets" => {
            let (path, text) = path_and_json(request)?;
            let mut preset = load(path)?;
            let Json::Object(map) = &mut preset else { return Err(format!("{} is not a JSON object", path.display())) };
            if let Some(Json::Object(snapshot)) = map.get_mut("snapshot") {
                snapshot.insert("widgets".to_owned(), parse(text)?);
                write(dir, path, &preset)?;
            }
            return Ok(None);
        }
        "delete" => {
            let path = request.path.ok_or("delete needs PRESET_PATH")?;
            remove_files(dir, snapshot_files(path)?.iter())?;
            ignore_missing(fs::remove_file(path))?;
        }
        "list" => {}
        mode => return Err(format!("unknown presets mode: {mode}")),
    }
    Ok(Some(Json::Array(list(dir)).to_string()))
}

fn path_and_json<'a>(request: &Request<'a>) -> Result<(&'a Path, &'a str), String> {
    match (request.path, request.json) {
        (Some(path), Some(json)) => Ok((path, json)),
        _ => Err(format!("{} needs PRESET_PATH and JSON", request.mode)),
    }
}

fn parse(text: &str) -> Result<Json, String> {
    serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))
}

fn load(path: &Path) -> Result<Json, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    parse(&text).map_err(|e| format!("{}: {e}", path.display()))
}

fn ignore_missing(result: std::io::Result<()>) -> Result<(), String> {
    match result {
        Err(e) if e.kind() != ErrorKind::NotFound => Err(e.to_string()),
        _ => Ok(()),
    }
}

/// The names of the entries of `dir` that end in `suffix`, without the hidden ones, sorted
fn names(dir: &Path, suffix: &str) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| !n.starts_with('.') && n.ends_with(suffix))
        .collect();
    names.sort();
    names
}

/// Every layout of `<dir>/*/layout.json` that is a JSON array, by the name of its folder
fn read_widgets(dir: &Path) -> Map<String, Json> {
    let mut layouts = Map::new();
    for name in names(dir, "") {
        let Ok(text) = fs::read_to_string(dir.join(&name).join("layout.json")) else { continue };
        if let Ok(layout @ Json::Array(_)) = serde_json::from_str(&text) {
            layouts.insert(name, layout);
        }
    }
    layouts
}

fn truthy(value: &Json) -> bool {
    match value {
        Json::Null => false,
        Json::Bool(b) => *b,
        Json::Number(n) => n.as_f64() != Some(0.0),
        Json::String(s) => !s.is_empty(),
        Json::Array(a) => !a.is_empty(),
        Json::Object(o) => !o.is_empty(),
    }
}

/// The names of the screenshots a preset's bar, dock and widgets snapshots point to, or none when
/// the preset cannot be read
fn snapshot_files(path: &Path) -> Result<BTreeSet<String>, String> {
    let Some(Json::Object(preset)) = fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()) else {
        return Ok(BTreeSet::new());
    };
    let snapshot = match preset.get("snapshot") {
        Some(Json::Object(map)) => map.clone(),
        Some(other) if truthy(other) => return Err(format!("{}: snapshot is not a JSON object", path.display())),
        _ => Map::new(),
    };
    Ok(["bar", "dock", "widgets"]
        .iter()
        .filter_map(|layer| snapshot.get(*layer)?.get("file")?.as_str().filter(|f| !f.is_empty()))
        .map(|file| file.rsplit('/').next().unwrap_or(file).to_owned())
        .collect())
}

fn remove_files<'a>(dir: &Path, names: impl IntoIterator<Item = &'a String>) -> Result<(), String> {
    for name in names {
        ignore_missing(fs::remove_file(dir.join(name)))?;
    }
    Ok(())
}

/// Writes `value` as indented JSON to a temporary file beside `target` and renames it over `target`
fn write(dir: &Path, target: &Path, value: &Json) -> Result<(), String> {
    let cannot = |e: std::io::Error| format!("cannot write {}: {e}", target.display());
    fs::create_dir_all(dir).map_err(cannot)?;
    let mut text = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    text.push('\n');
    let mut tmp = target.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, text).map_err(cannot)?;
    fs::rename(&tmp, target).map_err(cannot)
}

fn created(preset: &Json) -> f64 {
    preset.get("created").and_then(Json::as_f64).unwrap_or(0.0)
}

/// The presets in `dir`, oldest first, each with its file's name as `id`
fn list(dir: &Path) -> Vec<Json> {
    let mut presets = Vec::new();
    for name in names(dir, ".json") {
        let Ok(text) = fs::read_to_string(dir.join(&name)) else { continue };
        let Ok(Json::Object(mut preset)) = serde_json::from_str(&text) else { continue };
        if !matches!(preset.get("settings"), Some(Json::Object(_))) {
            continue;
        }
        preset.insert("id".to_owned(), Json::String(name.strip_suffix(".json").unwrap_or(&name).to_owned()));
        presets.push(Json::Object(preset));
    }
    presets.sort_by(|a, b| created(a).total_cmp(&created(b)));
    presets
}
