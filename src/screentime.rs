//! Screen time: how long each app was focused, per day, hour, quarter hour and minute, in the same
//! sqlite database and state files the shell's focus daemon kept, with the same daily limits and
//! notifications. The focus comes from the compositor feed.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rusqlite::{Connection, params};
use serde_json::{Value as Json, json};

/// What is focused when no app is: none of these is counted
pub const SYSTEM_STATES: [&str; 4] = ["Desktop", "Locked", "Quickshell", "Unknown"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// A calendar day, counted from 1970-01-01
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Date(i64);

impl Date {
    pub fn from_ymd(year: i64, month: u32, day: u32) -> Date {
        let y = if month <= 2 { year - 1 } else { year };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let m = i64::from(month);
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        Date(era * 146097 + doe - 719468)
    }

    pub fn ymd(self) -> (i64, u32, u32) {
        let z = self.0 + 719468;
        let era = z.div_euclid(146097);
        let doe = z - era * 146097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        (yoe + era * 400 + i64::from(month <= 2), month, day)
    }

    pub fn parse(text: &str) -> Option<Date> {
        let mut parts = text.splitn(3, '-');
        let year: i64 = parts.next()?.parse().ok()?;
        let month: u32 = parts.next()?.parse().ok()?;
        let day: u32 = parts.next()?.parse().ok()?;
        let date = Date::from_ymd(year, month, day);
        (text.len() == 10 && date.ymd() == (year, month, day)).then_some(date)
    }

    pub fn iso(self) -> String {
        let (y, m, d) = self.ymd();
        format!("{y:04}-{m:02}-{d:02}")
    }

    /// Monday is 0, as Python counts
    pub fn weekday(self) -> usize {
        (self.0 + 3).rem_euclid(7) as usize
    }

    pub fn add(self, days: i64) -> Date {
        Date(self.0 + days)
    }

    pub fn month_length(self) -> u32 {
        let (y, m, _) = self.ymd();
        let next = if m == 12 { Date::from_ymd(y + 1, 1, 1) } else { Date::from_ymd(y, m + 1, 1) };
        (next.0 - Date::from_ymd(y, m, 1).0) as u32
    }
}

/// The local date and time of day
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Now {
    pub date: Date,
    pub hour: u32,
    pub minute: u32,
}

pub fn now() -> Now {
    // SAFETY: time with a null pointer only returns the time; localtime_r fills the zeroed tm
    let tm = unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm
    };
    Now {
        date: Date::from_ymd(i64::from(tm.tm_year) + 1900, (tm.tm_mon + 1) as u32, tm.tm_mday as u32),
        hour: tm.tm_hour as u32,
        minute: tm.tm_min as u32,
    }
}

/// `round(x, 1)` as Python does it
fn round1(value: f64) -> Json {
    format!("{value:.1}").parse::<f64>().map(Json::from).unwrap_or(Json::from(0))
}

fn percent(part: i64, total: i64) -> Json {
    if total > 0 { round1(part as f64 / total as f64 * 100.0) } else { Json::from(0) }
}

/// Splits on `\s+[-—|]\s+`, left to right like a regex split
fn split_title(text: &str) -> Vec<&str> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let (mut parts, mut start, mut i) = (Vec::new(), 0, 0);
    while i < chars.len() {
        if chars[i].1.is_whitespace() {
            let mut j = i;
            while j < chars.len() && chars[j].1.is_whitespace() {
                j += 1;
            }
            if j + 1 < chars.len() && matches!(chars[j].1, '-' | '—' | '|') && chars[j + 1].1.is_whitespace() {
                let mut k = j + 1;
                while k < chars.len() && chars[k].1.is_whitespace() {
                    k += 1;
                }
                parts.push(&text[start..chars[i].0]);
                start = chars.get(k).map_or(text.len(), |c| c.0);
                i = k;
                continue;
            }
        }
        i += 1;
    }
    parts.push(&text[start..]);
    parts
}

/// Drops a leading `(3) ` or `[3] ` and a trailing ` (3)`, the counters apps put in titles
fn strip_counters(title: &str) -> &str {
    let digits_then = |text: &str, close: char| -> Option<usize> {
        let end = text.find(|c: char| !c.is_ascii_digit())?;
        (end > 0 && text[end..].starts_with(close)).then_some(end + close.len_utf8())
    };
    let mut title = title;
    for (open, close) in [('(', ')'), ('[', ']')] {
        if let Some(rest) = title.strip_prefix(open)
            && let Some(len) = digits_then(rest, close)
        {
            title = rest[len..].trim_start();
            break;
        }
    }
    if let Some(body) = title.strip_suffix(')')
        && let Some(open) = body.rfind('(')
        && open + 1 < body.len()
        && body[open + 1..].bytes().all(|b| b.is_ascii_digit())
    {
        title = body[..open].trim_end();
    }
    title
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars.as_str().to_lowercase().chars()).collect()).unwrap_or_default()
}

pub fn resolve_app_name(class: &str, title: &str) -> String {
    if class.is_empty() || SYSTEM_STATES.contains(&class) {
        return if class.is_empty() { "Unknown".into() } else { class.into() };
    }
    let clean = strip_counters(title);
    let parts = split_title(clean);
    let name = if parts.len() > 1 { parts[parts.len() - 1].trim() } else { clean.trim() };
    if name.chars().count() > 25 { capitalize(class) } else { name.to_owned() }
}

/// The class and the shown name of a focused window
pub fn classify_window(class: &str, title: &str) -> (String, String) {
    let (class, title) = (class.trim(), title.trim());
    if class.to_lowercase().contains("quickshell")
        || title.to_lowercase().contains("qs-master")
        || class.to_lowercase().contains("qs-master")
    {
        return ("Quickshell".into(), "Quickshell".into());
    }
    let class = if class.is_empty() { "Unknown" } else { class };
    let title = if title.is_empty() { class } else { title };
    (class.to_owned(), resolve_app_name(class, title))
}

/// Moves the database from where old versions kept it
pub fn migrate(db_dir: &Path, home: &Path) {
    let old = home.join(".local/share/focustime");
    if db_dir.join("focustime.db").exists() || !old.join("focustime.db").exists() {
        return;
    }
    let _ = std::fs::create_dir_all(db_dir);
    for entry in std::fs::read_dir(&old).into_iter().flatten().flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with("focustime.db") {
            let _ = std::fs::rename(entry.path(), db_dir.join(&name));
        }
    }
}

pub fn open(db: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(db)?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         CREATE TABLE IF NOT EXISTS focus_log (log_date TEXT, app_class TEXT, seconds INTEGER, app_title TEXT, PRIMARY KEY (log_date, app_class));
         CREATE INDEX IF NOT EXISTS idx_log_date ON focus_log(log_date);
         CREATE TABLE IF NOT EXISTS focus_hourly (log_date TEXT, hour INTEGER, app_class TEXT, seconds INTEGER, PRIMARY KEY (log_date, hour, app_class));
         CREATE TABLE IF NOT EXISTS focus_intervals (log_date TEXT, interval_idx INTEGER, app_class TEXT, seconds INTEGER, PRIMARY KEY (log_date, interval_idx, app_class));
         CREATE TABLE IF NOT EXISTS focus_minutes (log_date TEXT, minute_idx INTEGER, app_class TEXT, seconds INTEGER, PRIMARY KEY (log_date, minute_idx, app_class));",
    )?;
    let has_title = conn
        .prepare("PRAGMA table_info(focus_log)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .flatten()
        .any(|name| name == "app_title");
    if !has_title {
        conn.execute("ALTER TABLE focus_log ADD COLUMN app_title TEXT", [])?;
    }
    Ok(conn)
}

/// A query with `AND app_class = ?` when filtered by app
struct Filter<'a>(Option<&'a str>);

impl Filter<'_> {
    fn query(&self, base: &str, tail: &str) -> String {
        format!("{base}{}{tail}", if self.0.is_some() { " AND app_class = ?" } else { "" })
    }

    fn params(&self, dates: &[String]) -> Vec<String> {
        dates.iter().cloned().chain(self.0.map(str::to_owned)).collect()
    }

    fn rows<T>(
        &self,
        conn: &Connection,
        base: &str,
        tail: &str,
        dates: &[String],
        map: impl Fn(&rusqlite::Row) -> rusqlite::Result<T>,
    ) -> Vec<T> {
        let Ok(mut statement) = conn.prepare(&self.query(base, tail)) else { return Vec::new() };
        let params = self.params(dates);
        statement
            .query_map(rusqlite::params_from_iter(params.iter()), |row| map(row))
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    fn sum(&self, conn: &Connection, base: &str, dates: &[String]) -> i64 {
        self.rows(conn, base, "", dates, |row| row.get::<_, Option<i64>>(0)).first().copied().flatten().unwrap_or(0)
    }
}

pub fn known_apps(conn: &Connection) -> Vec<Json> {
    Filter(None).rows(
        conn,
        "SELECT app_class, MAX(COALESCE(app_title, app_class)) FROM focus_log GROUP BY app_class",
        "",
        &[],
        |row| Ok(json!({ "class": row.get::<_, String>(0)?, "name": row.get::<_, Option<String>>(1)? })),
    )
}

/// The statistics the wellbeing page shows for `target`, all apps or one
pub fn stats(conn: &Connection, target: Date, label: &str, app: Option<&str>, current: &str) -> Json {
    let f = Filter(app);
    let day = |d: Date| vec![d.iso()];
    let monday = target.add(-(target.weekday() as i64));
    let sunday = monday.add(6);
    let week = vec![monday.iso(), sunday.iso()];
    let (_, ms, ds) = monday.ymd();
    let (_, me, de) = sunday.ymd();
    let week_range = format!("{} {ds} - {} {de}", MONTHS[ms as usize - 1], MONTHS[me as usize - 1]);

    let yesterday = f.sum(conn, "SELECT SUM(seconds) FROM focus_log WHERE log_date = ?", &day(target.add(-1)));
    let (days, week_total) = f
        .rows(
            conn,
            "SELECT COUNT(DISTINCT log_date), SUM(seconds) FROM focus_log WHERE log_date >= ? AND log_date <= ? AND seconds > 0",
            "",
            &week,
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?.unwrap_or(0))),
        )
        .first()
        .copied()
        .unwrap_or((0, 0));
    let average = if days > 0 { week_total.div_euclid(days) } else { 0 };
    let total = f.sum(conn, "SELECT SUM(seconds) FROM focus_log WHERE log_date = ?", &day(target));

    let app_row =
        |row: &rusqlite::Row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?, row.get::<_, i64>(2)?));
    let apps: Vec<Json> = f
        .rows(
            conn,
            "SELECT app_class, COALESCE(app_title, app_class), SUM(seconds) as secs FROM focus_log WHERE log_date = ?",
            " GROUP BY app_class ORDER BY secs DESC",
            &day(target),
            app_row,
        )
        .into_iter()
        .map(|(class, name, secs)| json!({"class": class, "name": name, "seconds": secs, "percent": percent(secs, total)}))
        .collect();
    let week_rows = f.rows(
        conn,
        "SELECT app_class, COALESCE(app_title, app_class), SUM(seconds) as secs FROM focus_log WHERE log_date >= ? AND log_date <= ?",
        " GROUP BY app_class ORDER BY secs DESC LIMIT 50",
        &week,
        app_row,
    );
    let week_apps_total: i64 = week_rows.iter().map(|r| r.2).sum();
    let week_apps: Vec<Json> = week_rows
        .into_iter()
        .map(|(class, name, secs)| {
            json!({"class": class, "name": name, "seconds": secs, "percent": percent(secs, week_apps_total)})
        })
        .collect();

    let per_day = |dates: &[String]| -> HashMap<String, i64> {
        f.rows(
            conn,
            "SELECT log_date, SUM(seconds) FROM focus_log WHERE log_date >= ? AND log_date <= ?",
            " GROUP BY log_date",
            dates,
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .into_iter()
        .collect()
    };
    let week_map = per_day(&week);
    let week_data: Vec<Json> = (0..7)
        .map(|i| {
            let d = monday.add(i).iso();
            json!({"date": d, "day": WEEKDAYS[i as usize], "total": week_map.get(&d).copied().unwrap_or(0), "is_target": d == label})
        })
        .collect();

    let (y, m, _) = target.ymd();
    let first = Date::from_ymd(y, m, 1);
    let month_map = per_day(&[first.iso(), first.add(i64::from(target.month_length()) - 1).iso()]);
    let mut month_data: Vec<Json> =
        (0..first.weekday()).map(|_| json!({"date": "", "total": -1, "is_target": false})).collect();
    for i in 0..target.month_length() {
        let d = first.add(i64::from(i)).iso();
        month_data.push(json!({"date": d, "total": month_map.get(&d).copied().unwrap_or(0), "is_target": d == label}));
    }

    let mut hourly = [0i64; 48];
    let pairs = |base: &str, tail: &str, dates: &[String]| {
        f.rows(conn, base, tail, dates, |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))
    };
    for (hour, secs) in
        pairs("SELECT hour, SUM(seconds) FROM focus_hourly WHERE log_date = ?", " GROUP BY hour", &day(target))
    {
        if (0..=23).contains(&hour) {
            hourly[hour as usize * 2] += secs;
        }
    }
    for (index, secs) in pairs(
        "SELECT interval_idx, SUM(seconds) FROM focus_intervals WHERE log_date = ?",
        " GROUP BY interval_idx",
        &day(target),
    ) {
        if (0..96).contains(&index) {
            hourly[index as usize / 2] += secs;
        }
    }

    let mut heatmap = [[0i64; 24]; 7];
    for (date, hour, secs) in f.rows(
        conn,
        "SELECT log_date, hour, SUM(seconds) FROM focus_hourly WHERE log_date >= ? AND log_date <= ?",
        " GROUP BY log_date, hour",
        &week,
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)),
    ) {
        if let Some(d) = Date::parse(&date)
            && (0..=23).contains(&hour)
        {
            heatmap[d.weekday()][hour as usize] += secs;
        }
    }

    let mut minutes = vec![0i64; 1440];
    for (index, secs) in pairs(
        "SELECT minute_idx, SUM(seconds) FROM focus_minutes WHERE log_date >= ? AND log_date <= ?",
        " GROUP BY minute_idx",
        &week,
    ) {
        if (0..1440).contains(&index) {
            minutes[index as usize] += secs;
        }
    }

    json!({
        "selected_date": target.iso(),
        "total": total,
        "average": average,
        "week_range": week_range,
        "yesterday": yesterday,
        "current": current,
        "apps": apps,
        "week_apps": week_apps,
        "week": week_data,
        "month": month_data,
        "hourly": hourly.to_vec(),
        "week_heatmap": heatmap.iter().map(|d| d.to_vec()).collect::<Vec<_>>(),
        "peak_usage_str": peak(&minutes),
        "all_known_apps": known_apps(conn),
    })
}

/// The busiest hour of the week by minute, trimmed to its first and last active minute
pub fn peak(minutes: &[i64]) -> String {
    let (mut best, mut max) = (None, 0);
    let mut sum: i64 = minutes[..60].iter().sum();
    for i in 0..1440 - 60 {
        if i > 0 {
            sum += minutes[i + 59] - minutes[i - 1];
        }
        if sum > max && sum > 0 {
            max = sum;
            best = Some((i, i + 60));
        }
    }
    let Some((mut start, end)) = best else { return "N/A".into() };
    while start < end && minutes[start] == 0 {
        start += 1;
    }
    let mut last = end - 1;
    while last > start && minutes[last] == 0 {
        last -= 1;
    }
    format!("{:02}:{:02} - {:02}:{:02}", start / 60, start % 60, last / 60, last % 60)
}

/// One second or more of an app's focus, kept until it is written
struct Entry {
    date: Date,
    class: String,
    title: String,
    at: Now,
    seconds: i64,
}

/// Counts focus time and keeps the state file the wellbeing page reads
pub struct Tracker {
    conn: Connection,
    state_file: PathBuf,
    buffer: Vec<Entry>,
    cached: Option<Json>,
    last_sync: Option<std::time::Instant>,
    last_date: Option<Date>,
    known: BTreeSet<String>,
    last_written: Option<String>,
}

impl Tracker {
    pub fn new(conn: Connection, state_file: PathBuf) -> Tracker {
        Tracker {
            conn,
            state_file,
            buffer: Vec::new(),
            cached: None,
            last_sync: None,
            last_date: None,
            known: BTreeSet::new(),
            last_written: None,
        }
    }

    pub fn cached(&self) -> Option<&Json> {
        self.cached.as_ref()
    }

    pub fn write_state(&mut self) {
        let Some(cached) = &self.cached else { return };
        let text = cached.to_string();
        if self.last_written.as_ref() == Some(&text) && self.state_file.exists() {
            return;
        }
        let temp = self.state_file.with_extension("json.tmp");
        if std::fs::write(&temp, &text).is_ok() && std::fs::rename(&temp, &self.state_file).is_ok() {
            self.last_written = Some(text);
        }
    }

    fn full_sync(&mut self, date: Date, current: &str) {
        let data = stats(&self.conn, date, &date.iso(), None, current);
        self.known = data["all_known_apps"]
            .as_array()
            .map(|apps| apps.iter().filter_map(|a| a["class"].as_str().map(str::to_owned)).collect())
            .unwrap_or_default();
        self.cached = Some(data);
        self.last_sync = Some(std::time::Instant::now());
        self.last_date = Some(date);
    }

    /// Counts `seconds` of focus on `class`, or only notes what is shown when nothing is counted
    pub fn tick(&mut self, class: &str, title: &str, seconds: i64, write: bool) {
        let at = now();
        if class.is_empty() || SYSTEM_STATES.contains(&class) || seconds <= 0 {
            if let Some(cached) = &mut self.cached {
                cached["current"] = title.into();
                if write {
                    self.write_state();
                }
            }
            return;
        }
        self.buffer.push(Entry { date: at.date, class: class.into(), title: title.into(), at, seconds });
        let stale = self.last_sync.is_none_or(|t| t.elapsed().as_secs() > 60);
        if self.cached.is_none() || self.last_date != Some(at.date) || stale {
            self.flush();
            self.full_sync(at.date, title);
        } else if let Some(d) = &mut self.cached {
            d["total"] = (d["total"].as_i64().unwrap_or(0) + seconds).into();
            d["current"] = title.into();
            let total = d["total"].as_i64().unwrap_or(0);
            let apps = d["apps"].as_array_mut().expect("apps is a list");
            match apps.iter_mut().find(|a| a["class"] == class) {
                Some(app) => {
                    app["seconds"] = (app["seconds"].as_i64().unwrap_or(0) + seconds).into();
                    app["name"] = title.into();
                }
                None => apps.push(json!({"class": class, "name": title, "seconds": seconds, "percent": 0})),
            }
            for app in apps.iter_mut() {
                app["percent"] = percent(app["seconds"].as_i64().unwrap_or(0), total);
            }
            apps.sort_by_key(|a| std::cmp::Reverse(a["seconds"].as_i64().unwrap_or(0)));
            for key in ["week", "month"] {
                for day in d[key].as_array_mut().into_iter().flatten().filter(|x| x["is_target"] == true) {
                    day["total"] = (day["total"].as_i64().unwrap_or(0) + seconds).into();
                }
            }
            let half = (at.hour * 2 + u32::from(at.minute >= 30)) as usize;
            if let Some(slot) = d["hourly"].get_mut(half) {
                *slot = (slot.as_i64().unwrap_or(0) + seconds).into();
            }
            if let Some(slot) = d["week_heatmap"].get_mut(at.date.weekday()).and_then(|h| h.get_mut(at.hour as usize)) {
                *slot = (slot.as_i64().unwrap_or(0) + seconds).into();
            }
        }
        if self.known.insert(class.to_owned())
            && let Some(apps) = self.cached.as_mut().and_then(|d| d["all_known_apps"].as_array_mut())
        {
            apps.push(json!({"class": class, "name": title}));
        }
        if write {
            self.write_state();
        }
        if self.buffer.len() >= 15 {
            self.flush();
        }
    }

    pub fn flush(&mut self) {
        if self.buffer.is_empty() {
            return;
        }
        let mut logs: BTreeMap<(String, String), i64> = BTreeMap::new();
        let mut titles: HashMap<String, String> = HashMap::new();
        let mut hours: BTreeMap<(String, u32, String), i64> = BTreeMap::new();
        let mut quarters: BTreeMap<(String, u32, String), i64> = BTreeMap::new();
        let mut minutes: BTreeMap<(String, u32, String), i64> = BTreeMap::new();
        for e in self.buffer.drain(..) {
            let day = e.date.iso();
            *logs.entry((day.clone(), e.class.clone())).or_default() += e.seconds;
            titles.insert(e.class.clone(), e.title);
            let minute = e.at.hour * 60 + e.at.minute;
            *hours.entry((day.clone(), e.at.hour, e.class.clone())).or_default() += e.seconds;
            *quarters.entry((day.clone(), minute / 15, e.class.clone())).or_default() += e.seconds;
            *minutes.entry((day, minute, e.class)).or_default() += e.seconds;
        }
        let write = |conn: &Connection| -> rusqlite::Result<()> {
            let tx = conn.unchecked_transaction()?;
            for ((day, class), secs) in &logs {
                tx.execute(
                    "INSERT INTO focus_log (log_date, app_class, seconds, app_title) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(log_date, app_class) DO UPDATE SET seconds = seconds + ?3, app_title = ?4",
                    params![day, class, secs, titles[class]],
                )?;
            }
            for (table, column, rows) in [
                ("focus_hourly", "hour", &hours),
                ("focus_intervals", "interval_idx", &quarters),
                ("focus_minutes", "minute_idx", &minutes),
            ] {
                for ((day, index, class), secs) in rows {
                    tx.execute(
                        &format!(
                            "INSERT INTO {table} (log_date, {column}, app_class, seconds) VALUES (?1, ?2, ?3, ?4)
                             ON CONFLICT(log_date, {column}, app_class) DO UPDATE SET seconds = seconds + ?4"
                        ),
                        params![day, index, class, secs],
                    )?;
                }
            }
            tx.commit()
        };
        if let Err(error) = write(&self.conn) {
            eprintln!("serpantinum-core: cannot write screen time: {error}");
        }
    }
}

/// Which limit notifications were already sent today
#[derive(Debug, Default, PartialEq)]
pub struct Notified {
    pub overall: bool,
    pub apps: BTreeSet<String>,
}

impl Notified {
    pub fn load(path: &Path, today: Date) -> Notified {
        let data: Json =
            std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        if data["date"] != today.iso().as_str() {
            return Notified::default();
        }
        let apps =
            data["apps"].as_array().into_iter().flatten().filter_map(|a| a.as_str().map(str::to_owned)).collect();
        Notified { overall: data["overall"].as_bool().unwrap_or(false), apps }
    }

    pub fn save(&self, path: &Path, today: Date) {
        let data = json!({"date": today.iso(), "overall": self.overall, "apps": self.apps});
        let _ = std::fs::write(path, data.to_string());
    }
}

/// The settings language, else the locale's, as the shell picks it
pub fn language(settings: &Json, env: &HashMap<String, String>) -> String {
    if let Some(lang) = settings["general"]["language"].as_str().filter(|l| !l.is_empty()) {
        return lang.to_owned();
    }
    let locale = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|k| env.get(*k).filter(|v| !v.is_empty()))
        .map(String::as_str)
        .unwrap_or_default();
    let lang = locale.split(['_', '.', '@']).next().unwrap_or_default().to_lowercase();
    match lang.as_str() {
        "" => "en".into(),
        "uk" => "ua".into(),
        _ => lang,
    }
}

/// Looks a dotted key up in the language file, then in English, then takes the default
pub fn translate(dir: &Path, lang: &str, key: &str, default: &str, vars: &[(&str, &str)]) -> String {
    let lookup = |code: &str| -> Option<String> {
        let data: Json = serde_json::from_str(&std::fs::read_to_string(dir.join(format!("{code}.json"))).ok()?).ok()?;
        key.split('.').try_fold(&data, |node, part| node.get(part))?.as_str().map(str::to_owned)
    };
    let mut text = lookup(lang).or_else(|| lookup("en")).unwrap_or_else(|| default.to_owned());
    for (name, value) in vars {
        text = text.replace(&format!("{{{name}}}"), value);
    }
    text
}

pub fn notify(title: &str, body: &str) {
    if let Ok(mut child) = Command::new("notify-send")
        .args(["-u", "critical", "-a", "Serpantinum", title, body])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        std::thread::spawn(move || child.wait());
    }
}

/// Tells the shell's overlay that an app reached its limit
pub fn emit_limit_event(path: &Path, class: &str, name: &str, seconds: i64, limit: i64) {
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let data = json!({"stamp": stamp as u64, "class": class, "name": name, "seconds": seconds, "limit": limit});
    let temp = path.with_extension("json.tmp");
    if std::fs::write(&temp, data.to_string()).is_ok() {
        let _ = std::fs::rename(&temp, path);
    }
}

/// Reads the date given, today when it is not one
pub fn target_date(text: Option<&str>) -> Date {
    text.and_then(Date::parse).unwrap_or_else(|| now().date)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::OptionalExtension;

    fn last_sync_row(conn: &Connection, day: &str) -> Option<i64> {
        conn.query_row("SELECT SUM(seconds) FROM focus_log WHERE log_date = ?", [day], |row| row.get(0))
            .optional()
            .ok()?
    }

    #[test]
    fn dates_round_trip_and_know_their_weekday() {
        let d = Date::from_ymd(2026, 11, 6);
        assert_eq!(d.ymd(), (2026, 11, 6));
        assert_eq!(d.iso(), "2026-11-06");
        assert_eq!(d.weekday(), 4);
        assert_eq!(Date::parse("2024-02-29").map(Date::month_length), Some(29));
        assert_eq!(Date::parse("2023-02-29"), None);
        assert_eq!(Date::parse("2026-1-06"), None);
        assert_eq!(Date::from_ymd(2026, 12, 31).add(1).iso(), "2027-01-01");
        assert_eq!(Date::from_ymd(1969, 12, 31).ymd(), (1969, 12, 31));
    }

    #[test]
    fn names_come_from_titles_like_the_focus_daemon() {
        assert_eq!(
            classify_window("firefox", "(3) Inbox - Mail — Mozilla Firefox"),
            ("firefox".into(), "Mozilla Firefox".into())
        );
        assert_eq!(classify_window("org.telegram", "[12] Telegram (5)"), ("org.telegram".into(), "Telegram".into()));
        assert_eq!(
            classify_window("code", "main.rs - a very very long project name here"),
            ("code".into(), "Code".into())
        );
        assert_eq!(classify_window("foot", ""), ("foot".into(), "foot".into()));
        assert_eq!(classify_window("", ""), ("Unknown".into(), "Unknown".into()));
        assert_eq!(classify_window("org.quickshell", "x"), ("Quickshell".into(), "Quickshell".into()));
        assert_eq!(split_title("a - - b"), ["a", "- b"]);
        assert_eq!(split_title("a-b | c"), ["a-b", "c"]);
        assert_eq!(resolve_app_name("FIREFOX", "this title is definitely longer than twenty five"), "Firefox");
    }

    #[test]
    fn rounding_matches_python() {
        assert_eq!(percent(1, 3), json!(33.3));
        assert_eq!(percent(2, 3), json!(66.7));
        assert_eq!(percent(5, 0), json!(0));
        assert_eq!(percent(1, 1), json!(100.0));
    }

    #[test]
    fn the_peak_hour_is_trimmed_to_activity() {
        let mut minutes = vec![0i64; 1440];
        assert_eq!(peak(&minutes), "N/A");
        minutes[610..640].fill(60);
        minutes[900] = 30;
        assert_eq!(peak(&minutes), "10:10 - 10:39");
    }

    #[test]
    fn ticks_are_written_and_summed_by_stats() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(&dir.path().join("focustime.db")).unwrap();
        let mut tracker = Tracker::new(conn, dir.path().join("focustime_state.json"));
        tracker.tick("foot", "Terminal", 3, true);
        tracker.tick("firefox", "Firefox", 1, false);
        tracker.tick("foot", "Terminal", 2, true);
        tracker.tick("Desktop", "Desktop", 0, true);
        let cached = tracker.cached().unwrap().clone();
        assert_eq!(cached["total"], 6);
        assert_eq!(cached["current"], "Desktop");
        assert_eq!(cached["apps"][0]["class"], "foot");
        assert_eq!(cached["apps"][0]["seconds"], 5);
        assert_eq!(cached["apps"][1]["percent"], json!(16.7));
        tracker.flush();
        let today = now().date;
        let fresh = stats(&tracker.conn, today, &today.iso(), None, "History");
        assert_eq!(fresh["total"], 6);
        assert_eq!(fresh["apps"], cached["apps"]);
        let filtered = stats(&tracker.conn, today, &today.iso(), Some("firefox"), "firefox");
        assert_eq!(filtered["total"], 1);
        assert_eq!(fresh["all_known_apps"].as_array().unwrap().len(), 2);
        assert_eq!(last_sync_row(&tracker.conn, &today.iso()), Some(6));
        let written: Json =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("focustime_state.json")).unwrap()).unwrap();
        assert_eq!(written["current"], "Desktop");
    }

    #[test]
    fn notified_state_is_kept_for_the_day() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("limit_notified.json");
        let today = Date::from_ymd(2026, 11, 6);
        let notified = Notified { overall: true, apps: ["foot".to_owned()].into() };
        notified.save(&path, today);
        assert_eq!(Notified::load(&path, today), notified);
        assert_eq!(Notified::load(&path, today.add(1)), Notified::default());
    }

    #[test]
    fn language_and_translations_fall_back() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("en.json"), r#"{"a":{"b":"Limit for {app}"}}"#).unwrap();
        std::fs::write(dir.path().join("ua.json"), r#"{"a":{}}"#).unwrap();
        let env = HashMap::from([("LANG".to_owned(), "uk_UA.UTF-8".to_owned())]);
        assert_eq!(language(&json!({}), &env), "ua");
        assert_eq!(language(&json!({"general": {"language": "de"}}), &env), "de");
        assert_eq!(translate(dir.path(), "ua", "a.b", "x", &[("app", "Foot")]), "Limit for Foot");
        assert_eq!(translate(dir.path(), "ua", "a.c", "Default", &[]), "Default");
    }
}
