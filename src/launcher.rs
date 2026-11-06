//! The launcher's ranking (screen time and launches, both decaying by half a week at a time), its
//! launch log, and its file search, as the shell's app_rank.py and inline search script did them.

use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde_json::{Map, Value as Json, json};

use crate::screentime::{self, Date};

const HALF_LIFE_DAYS: f64 = 7.0;
const LAUNCH_SECONDS_PER_EVENT: f64 = 90.0;
const RETENTION_DAYS: i64 = 60;
const MAX_FILES: usize = 40;
const SKIPPED_DIRS: [&str; 6] = ["node_modules", ".git", ".cache", "target", "build", ".local"];

fn weight(age_days: i64) -> f64 {
    0.5f64.powf(age_days.max(0) as f64 / HALF_LIFE_DAYS)
}

fn env_dir(name: &str, home: &Path, fallback: &str) -> PathBuf {
    std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| home.join(fallback))
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

pub fn stats_dir(home: &Path) -> PathBuf {
    let dir = env_dir("QS_STATE_APPLAUNCHER", home, ".local/state/quickshell/applauncher");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Launch counts per app and day; old files kept a bare count, dated three days back
pub fn launches(dir: &Path, today: Date) -> Map<String, Json> {
    let raw: Json = std::fs::read_to_string(dir.join("launch_stats.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let three_days_ago = today.add(-3).iso();
    raw.as_object()
        .into_iter()
        .flatten()
        .filter_map(|(name, value)| match value {
            Json::Object(_) => Some((name.clone(), value.clone())),
            Json::Number(n) => {
                Some((name.clone(), json!({ three_days_ago.clone(): n.as_f64().unwrap_or(0.0) as i64 })))
            }
            _ => None,
        })
        .collect()
}

/// Counts a launch of `name` today, under the same lock file the script took
pub fn log_launch(dir: &Path, name: &str, today: Date) {
    if name.is_empty() {
        return;
    }
    let path = dir.join("launch_stats.json");
    let Ok(lock) = File::create(dir.join("launch_stats.json.lock")) else { return };
    // SAFETY: the descriptor belongs to `lock`, which outlives the lock
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return;
    }
    let mut stats = launches(dir, today);
    let mut days = stats.get(name).and_then(Json::as_object).cloned().unwrap_or_default();
    let key = today.iso();
    let count = days.get(&key).and_then(Json::as_i64).unwrap_or(0) + 1;
    days.insert(key, count.into());
    let cutoff = today.add(-RETENTION_DAYS);
    days.retain(|d, _| Date::parse(d).is_some_and(|d| d >= cutoff));
    stats.insert(name.to_owned(), Json::Object(days));
    let temp = dir.join("launch_stats.json.tmp");
    if std::fs::write(&temp, Json::Object(stats).to_string()).is_ok() {
        let _ = std::fs::rename(&temp, &path);
    }
}

fn launch_scores(dir: &Path, today: Date) -> Map<String, Json> {
    launches(dir, today)
        .into_iter()
        .map(|(name, days)| {
            let total: f64 = days
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(d, count)| Some(count.as_f64()? * weight(today.days_since(Date::parse(d)?))))
                .sum();
            (name, (total * LAUNCH_SECONDS_PER_EVENT).into())
        })
        .collect()
}

fn focus_db(home: &Path) -> Option<PathBuf> {
    let db = env_dir("QS_STATE_FOCUSTIME", home, ".local/state/quickshell/focustime").join("focustime.db");
    let old = home.join(".local/share/focustime/focustime.db");
    [db, old].into_iter().find(|p| p.exists())
}

/// Screen time per app class, decayed by age, with a bonus for every day it was used
fn focus_scores(conn: &Connection, today: Date) -> Map<String, Json> {
    let cutoff = today.add(-RETENTION_DAYS).iso();
    let Ok(mut statement) = conn.prepare(
        "SELECT app_class, log_date, SUM(seconds) FROM focus_log WHERE log_date >= ? GROUP BY app_class, log_date",
    ) else {
        return Map::new();
    };
    let rows = statement.query_map([&cutoff], |row| {
        Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<f64>>(2)?))
    });
    let mut apps: BTreeMap<String, (HashSet<String>, f64)> = BTreeMap::new();
    for (class, day, secs) in rows.into_iter().flatten().flatten() {
        let Some(class) = class.filter(|c| !c.is_empty()) else { continue };
        let Some(date) = Date::parse(&day) else { continue };
        let entry = apps.entry(class.to_lowercase()).or_default();
        entry.1 += secs.unwrap_or(0.0) * weight(today.days_since(date));
        entry.0.insert(day);
    }
    apps.into_iter().map(|(class, (days, secs))| (class, (secs + days.len() as f64 * 300.0).into())).collect()
}

/// Screen time per app class around this hour over the last two weeks
fn context_scores(conn: &Connection, today: Date, hour: u32) -> Map<String, Json> {
    let since = today.add(-14).iso();
    let hours = [(hour + 23) % 24, hour, (hour + 1) % 24];
    let Ok(mut statement) = conn.prepare(
        "SELECT app_class, SUM(seconds) FROM focus_hourly WHERE log_date >= ? AND hour IN (?, ?, ?) GROUP BY app_class",
    ) else {
        return Map::new();
    };
    let rows = statement.query_map(rusqlite::params![since, hours[0], hours[1], hours[2]], |row| {
        Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Option<i64>>(1)?.map_or(Json::Null, Json::from)))
    });
    rows.into_iter().flatten().flatten().filter_map(|(class, secs)| Some((class?.to_lowercase(), secs))).collect()
}

pub fn rank(home: &Path) -> Json {
    let now = screentime::now();
    let conn = focus_db(home).and_then(|db| Connection::open(db).ok());
    let (focus, context) = match &conn {
        Some(conn) => (focus_scores(conn, now.date), context_scores(conn, now.date, now.hour)),
        None => (Map::new(), Map::new()),
    };
    json!({ "focus": focus, "launch": launch_scores(&stats_dir(home), now.date), "context": context })
}

fn is_dir(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_dir())
}

fn entry(name: &str, path: &Path, dir: bool) -> Json {
    json!({ "name": name, "path": path.to_string_lossy(), "isDir": dir })
}

/// `os.path.dirname` and `os.path.basename`
fn split_path(path: &str) -> (String, String) {
    match path.rfind('/') {
        Some(at) => {
            let head = &path[..=at];
            let trimmed = head.trim_end_matches('/');
            (if trimmed.is_empty() { head.to_owned() } else { trimmed.to_owned() }, path[at + 1..].to_owned())
        }
        None => (String::new(), path.to_owned()),
    }
}

/// Entries of the directory a path typed so far points into
fn complete(query: &str, home: &Path) -> Vec<Json> {
    let expanded = if query.starts_with('~') {
        crate::clipboard::expand_tilde(query, home)
            .map_or_else(|| query.to_owned(), |p| p.to_string_lossy().into_owned())
    } else {
        query.to_owned()
    };
    let into = is_dir(Path::new(&expanded)) && (query.ends_with('/') || query.ends_with('\\'));
    let (dir, prefix) = if into {
        (expanded.clone(), String::new())
    } else {
        let (dir, base) = split_path(&expanded);
        (if dir.is_empty() { home.to_string_lossy().into_owned() } else { dir }, base.to_lowercase())
    };
    let dir = PathBuf::from(dir);
    let Ok(read) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut items: Vec<(String, bool)> =
        read.flatten().map(|e| (e.file_name().to_string_lossy().into_owned(), is_dir(&e.path()))).collect();
    items.sort_by_key(|(name, dir)| (!*dir, name.to_lowercase()));
    items
        .into_iter()
        .filter(|(name, _)| !name.starts_with('.') || prefix.starts_with('.'))
        .filter(|(name, _)| prefix.is_empty() || name.to_lowercase().contains(&prefix))
        .take(MAX_FILES)
        .map(|(name, dir_flag)| entry(&name, &dir.join(&name), dir_flag))
        .collect()
}

/// Walks a directory two levels deep the way `os.walk` does, top-down
fn walk(root: &Path, depth: usize, search: &str, seen: &mut HashSet<PathBuf>, out: &mut Vec<Json>) {
    let Ok(read) = std::fs::read_dir(root) else { return };
    let (mut dirs, mut files) = (Vec::new(), Vec::new());
    for e in read.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if is_dir(&e.path()) {
            let real = e.file_type().is_ok_and(|t| t.is_dir());
            dirs.push((name, real));
        } else {
            files.push(name);
        }
    }
    dirs.retain(|(name, _)| !name.starts_with('.') && !SKIPPED_DIRS.contains(&name.as_str()));
    let mut add = |name: &str, dir: bool, out: &mut Vec<Json>| {
        let full = root.join(name);
        if seen.insert(full.clone()) {
            out.push(entry(name, &full, dir));
        }
    };
    if !search.is_empty() {
        for (name, _) in &dirs {
            if name.to_lowercase().contains(search) {
                add(name, true, out);
                if out.len() >= MAX_FILES {
                    break;
                }
            }
        }
    }
    for name in &files {
        if !name.starts_with('.') && (search.is_empty() || name.to_lowercase().contains(search)) {
            add(name, false, out);
            if out.len() >= MAX_FILES {
                break;
            }
        }
    }
    if out.len() >= MAX_FILES || depth >= 2 {
        return;
    }
    for (name, real) in dirs {
        if real {
            walk(&root.join(name), depth + 1, search, seen, out);
            if out.len() >= MAX_FILES {
                return;
            }
        }
    }
}

/// Files and folders for the launcher: a typed path is completed, other text is searched for in
/// the home folder and the usual folders under it
pub fn files(query: &str, home: &Path) -> Json {
    if query.starts_with('/') || query.starts_with('~') {
        return Json::Array(complete(query, home));
    }
    let search = query.trim().to_lowercase();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for name in ["", "Desktop", "Documents", "Downloads", "Pictures", "Videos", "Music"] {
        let target = if name.is_empty() { home.to_path_buf() } else { home.join(name) };
        if is_dir(&target) {
            walk(&target, 0, &search, &mut seen, &mut out);
        }
        if out.len() >= MAX_FILES {
            break;
        }
    }
    out.truncate(MAX_FILES);
    Json::Array(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launches_decay_and_old_counts_migrate() {
        let dir = tempfile::tempdir().unwrap();
        let today = Date::from_ymd(2026, 11, 6);
        std::fs::write(dir.path().join("launch_stats.json"), r#"{"old": 4, "Foot": {"2026-10-01": 9}}"#).unwrap();
        log_launch(dir.path(), "Foot", today);
        log_launch(dir.path(), "Foot", today);
        let stats = launches(dir.path(), today);
        assert_eq!(stats["old"], json!({"2026-11-03": 4}));
        assert_eq!(stats["Foot"], json!({"2026-10-01": 9, "2026-11-06": 2}));
        let scores = launch_scores(dir.path(), today);
        let foot = (2.0 + 9.0 * 0.5f64.powf(36.0 / 7.0)) * 90.0;
        assert!((scores["Foot"].as_f64().unwrap() - foot).abs() < 1e-9);
        assert!((scores["old"].as_f64().unwrap() - 4.0 * 90.0 * 0.5f64.powf(3.0 / 7.0)).abs() < 1e-9);
    }

    #[test]
    fn paths_split_like_python() {
        assert_eq!(split_path("/home/me/Doc"), ("/home/me".into(), "Doc".into()));
        assert_eq!(split_path("/etc"), ("/".into(), "etc".into()));
        assert_eq!(split_path("x"), ("".into(), "x".into()));
    }

    #[test]
    fn search_stays_two_levels_deep_and_skips_hidden() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        for dir in ["a/b/c", "Documents/notes", ".secret", "node_modules/x"] {
            std::fs::create_dir_all(h.join(dir)).unwrap();
        }
        for file in
            ["a/b/plan.txt", "a/b/c/plan-deep.txt", "Documents/notes/plan.md", ".secret/plan", "node_modules/x/plan"]
        {
            std::fs::write(h.join(file), "").unwrap();
        }
        let found = files("PLAN", h);
        let mut paths: Vec<String> =
            found.as_array().unwrap().iter().map(|e| e["path"].as_str().unwrap().to_owned()).collect();
        paths.sort();
        let expect = |p: &str| h.join(p).to_string_lossy().into_owned();
        assert_eq!(paths, [expect("Documents/notes/plan.md"), expect("a/b/plan.txt")]);
        let typed = files(&format!("{}/a/", h.display()), h);
        assert_eq!(typed, json!([{"name": "b", "path": expect("a/b"), "isDir": true}]));
        let partial = files(&format!("{}/Doc", h.display()), h);
        assert_eq!(partial[0]["name"], "Documents");
    }
}
