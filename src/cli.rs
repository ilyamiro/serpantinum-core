//! The command line client. It starts the daemon when needed and prints what it serves as JSON, so
//! the shell reads it from a process's output with no D-Bus code of its own.

use std::collections::HashMap;
use std::fmt::Display;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde_json::{Map, Value as Json};
use zbus::blocking::fdo::{DBusProxy, PropertiesProxy};
use zbus::blocking::{Connection, Proxy};
use zbus::names::{BusName, InterfaceName};
use zbus::proxy::CacheProperties;
use zbus::zvariant::Value;

use crate::compositor::{SESSION_VARS, Session};
use crate::dbus::{
    BRIGHTNESS_INTERFACE, BRIGHTNESS_PATH, BUS_NAME, CLIPBOARD_INTERFACE, CLIPBOARD_PATH, COMPOSITOR_INTERFACE,
    COMPOSITOR_PATH, CORE_INTERFACE, IDLE_INTERFACE, IDLE_PATH, LOCKS_INTERFACE, LOCKS_PATH, SCREENTIME_INTERFACE,
    SCREENTIME_PATH, SCREENTIME_VARS, SYSINFO_INTERFACE, SYSINFO_PATH,
};
use crate::{launcher, monitors, screentime, system};

pub const USAGE: &str = "\
Usage: serpantinum-core [COMMAND]

Without a command, runs the daemon in the foreground. It serves org.serpantinum.Core on the
session bus. The commands below start it when it is not running, except quit.

Commands:
  start                         Start the daemon in the background unless it is running
  quit                          Stop the daemon
  sysinfo                       Print one sample of CPU, memory, temperature, network and disk usage
  watch sysinfo [--interval S]  Print a sample every S seconds (default 2) until stopped
  watch locks                   Print Caps Lock and Num Lock now and on every change
  watch compositor              Print the workspaces, windows and keyboard layout of Sway, Niri or
                                Mango now and on every change
  watch brightness              Print the brightness in percent and its backend now and on every change
  watch idle MARKER             Serve org.freedesktop.ScreenSaver while running and print
                                \"inhibit 1\" or \"inhibit 0\" when apps start or stop inhibiting idle,
                                \"sleep\" and \"wake\" around sleep, and \"log ...\" lines. Reads
                                \"sleep-lock on|off\" lines: when on, sleep waits up to 4 seconds
                                for the file MARKER, which the lock screen creates
  brightness [get]              Print the brightness in percent, or -1 when there is nothing to read
  brightness set PERCENT        Set the brightness
  brightness raise|lower        Raise or lower the brightness by 5 percent
  brightness backend            Print backlight, ddc or none
  screentime track              Count the focused app's screen time while running
  screentime stats [DATE]       Print the screen time statistics of DATE (YYYY-MM-DD, default today);
                                --app CLASS for one app, --db-dir DIR for another database
  launcher rank                 Print the launcher's app scores from screen time and launches
  launcher log NAME             Count a launch of NAME
  launcher files QUERY          Print files matching QUERY, or completing it when it is a path
  monitors                      Print the outputs in use with their size and refresh rate
  system can-hibernate          Print yes or no
  system fingerprint [PAM_DIR]  Exit with 0 when fingerprint unlock is set up, else 1
  clipboard list OFFSET LIMIT   Print a page of the clipboard history
  clipboard pin ID              Pin or unpin an entry
  clipboard delete ID           Delete an entry
  clipboard wipe                Delete the whole history

Every command prints JSON, one object or array per line.

Options:
  --cache-dir DIR  Where clipboard commands keep decoded images and pins
  -h, --help       Show this help
  -V, --version    Show the version";

const WAIT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(20);
const RESTART_DELAY: Duration = Duration::from_secs(1);
const LOCKS_INTERVAL: Duration = Duration::from_millis(250);
/// Properties served as JSON text, printed as the JSON they hold
const JSON_PROPERTIES: &[&str] = &["Workspaces", "Windows", "Keyboard"];
/// Properties that steer the output instead of being part of it
const HIDDEN_PROPERTIES: &[&str] = &["Samples", "Ready"];

/// How a watcher holds its lease
#[derive(Clone, Copy)]
enum Lease {
    Every(Duration),
    Session,
    Plain,
}

#[derive(Debug)]
enum Error {
    Usage(String),
    Failed(String),
}

type Result<T> = std::result::Result<T, Error>;

fn usage(message: impl Display) -> Error {
    Error::Usage(message.to_string())
}

fn failed(message: impl Display) -> Error {
    Error::Failed(message.to_string())
}

impl From<zbus::Error> for Error {
    fn from(error: zbus::Error) -> Self {
        failed(error)
    }
}

impl From<zbus::fdo::Error> for Error {
    fn from(error: zbus::fdo::Error) -> Self {
        failed(error)
    }
}

impl From<zbus::names::Error> for Error {
    fn from(error: zbus::names::Error) -> Self {
        failed(error)
    }
}

pub fn run(args: &[String]) -> ExitCode {
    match command(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Error::Usage(message)) => {
            eprintln!("serpantinum-core: {message}\n\n{USAGE}");
            ExitCode::from(2)
        }
        Err(Error::Failed(message)) => {
            eprintln!("serpantinum-core: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Splits `--name VALUE` options from the positional arguments
fn options(args: &[String]) -> Result<(Vec<&str>, HashMap<&str, &str>)> {
    let (mut positional, mut options) = (Vec::new(), HashMap::new());
    let mut iter = args.iter().map(String::as_str);
    while let Some(arg) = iter.next() {
        if let Some(name) = arg.strip_prefix("--") {
            let value = iter.next().ok_or_else(|| usage(format!("--{name} needs a value")))?;
            options.insert(name, value);
        } else {
            positional.push(arg);
        }
    }
    Ok((positional, options))
}

fn command(args: &[String]) -> Result<()> {
    let (positional, options) = options(args)?;
    if let Some(name) = options.keys().find(|n| !matches!(**n, "interval" | "cache-dir" | "app" | "db-dir")) {
        return Err(usage(format!("unknown option --{name}")));
    }
    match positional.as_slice() {
        ["start"] => start(&connect()?),
        ["quit"] => quit(),
        ["sysinfo"] => sysinfo(),
        ["watch", "sysinfo"] => {
            let seconds = match options.get("interval") {
                Some(text) => text.parse::<f64>().ok().filter(|s| s.is_finite() && *s > 0.0),
                None => Some(2.0),
            }
            .ok_or_else(|| usage("--interval takes a number of seconds"))?;
            watch(SYSINFO_PATH, SYSINFO_INTERFACE, Lease::Every(Duration::from_secs_f64(seconds)))
        }
        ["watch", "locks"] => watch(LOCKS_PATH, LOCKS_INTERFACE, Lease::Every(LOCKS_INTERVAL)),
        ["watch", "compositor"] => watch(COMPOSITOR_PATH, COMPOSITOR_INTERFACE, Lease::Session),
        ["watch", "brightness"] => watch(BRIGHTNESS_PATH, BRIGHTNESS_INTERFACE, Lease::Plain),
        ["brightness", rest @ ..] => brightness(rest),
        ["watch", "idle", marker] => watch_idle(marker),
        ["screentime", "track"] => track_screentime(),
        ["launcher", "rank"] => {
            print(&launcher::rank(&launcher::home()).to_string());
            Ok(())
        }
        ["launcher", "log", name] => {
            launcher::log_launch(&launcher::stats_dir(&launcher::home()), name, screentime::now().date);
            Ok(())
        }
        ["launcher", "files"] => {
            print(&launcher::files("", &launcher::home()).to_string());
            Ok(())
        }
        ["launcher", "files", query] => {
            print(&launcher::files(query, &launcher::home()).to_string());
            Ok(())
        }
        ["monitors"] => {
            print(
                &Json::Array(monitors::list(&Session::from_env()).iter().map(monitors::Monitor::json).collect())
                    .to_string(),
            );
            Ok(())
        }
        ["system", "can-hibernate"] => {
            print(if system::can_hibernate() { "yes" } else { "no" });
            Ok(())
        }
        ["system", "fingerprint", rest @ ..] if rest.len() <= 1 => {
            let dir = rest.first().copied().unwrap_or("/etc/pam.d");
            if system::fingerprint_available(std::path::Path::new(dir)) { Ok(()) } else { std::process::exit(1) }
        }
        ["screentime", "stats", rest @ ..] if rest.len() <= 1 => {
            screentime_stats(rest.first().copied(), options.get("app").copied(), options.get("db-dir").copied())
        }
        ["clipboard", rest @ ..] => clipboard(rest, options.get("cache-dir").copied()),
        [] => Err(usage("no command")),
        _ => Err(usage(format!("unknown command: {}", positional.join(" ")))),
    }
}

fn connect() -> Result<Connection> {
    Connection::session().map_err(|e| failed(format!("cannot connect to the session bus: {e}")))
}

fn running(conn: &Connection) -> Result<bool> {
    Ok(DBusProxy::new(conn)?.name_has_owner(BusName::try_from(BUS_NAME)?)?)
}

fn start(conn: &Connection) -> Result<()> {
    if running(conn)? {
        return Ok(());
    }
    let exe = std::env::current_exe().map_err(|e| failed(format!("cannot find this program: {e}")))?;
    let mut daemon = Command::new(exe);
    daemon.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe, so it may run between fork and exec. A session of its own
    // keeps the daemon alive when the caller's process group goes away.
    unsafe {
        daemon.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = daemon.spawn().map_err(|e| failed(format!("cannot start the daemon: {e}")))?;
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if running(conn)? {
            return Ok(());
        }
        // A daemon that lost a race to another one exits successfully, and the winner is waited for
        if let Ok(Some(status)) = child.try_wait()
            && !status.success()
        {
            return Err(failed(format!("the daemon exited at start ({status})")));
        }
        sleep(POLL);
    }
    Err(failed("the daemon did not appear on the bus"))
}

fn quit() -> Result<()> {
    let conn = connect()?;
    if !running(&conn)? {
        return Ok(());
    }
    Proxy::new(&conn, BUS_NAME, "/", CORE_INTERFACE)?.call_method("Quit", &())?;
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if !running(&conn)? {
            return Ok(());
        }
        sleep(POLL);
    }
    Err(failed("the daemon did not stop"))
}

/// `RamPercent` as `ramPercent`
fn json_key(name: &str) -> String {
    let mut chars = name.chars();
    chars.next().map(|first| first.to_lowercase().chain(chars).collect()).unwrap_or_default()
}

fn json_value(name: &str, value: &Value) -> Json {
    match value {
        Value::Str(s) if JSON_PROPERTIES.contains(&name) => serde_json::from_str(s.as_str()).unwrap_or(Json::Null),
        Value::Bool(b) => (*b).into(),
        Value::U32(n) => (*n).into(),
        Value::U64(n) => (*n).into(),
        Value::I32(n) => (*n).into(),
        Value::F64(f) => serde_json::Number::from_f64(*f).map(Json::Number).unwrap_or(Json::Null),
        Value::Str(s) => s.as_str().into(),
        _ => Json::Null,
    }
}

fn merge<'a>(state: &mut Map<String, Json>, values: impl IntoIterator<Item = (&'a str, &'a Value<'a>)>) {
    for (name, value) in values {
        if !HIDDEN_PROPERTIES.contains(&name) {
            state.insert(json_key(name), json_value(name, value));
        }
    }
}

fn print(line: &str) {
    let mut out = std::io::stdout().lock();
    if writeln!(out, "{line}").and_then(|_| out.flush()).is_err() {
        // Nobody reads any more
        std::process::exit(0);
    }
}

fn properties<'a>(conn: &Connection, path: &'a str) -> Result<PropertiesProxy<'a>> {
    Ok(PropertiesProxy::builder(conn)
        .destination(BUS_NAME)?
        .path(path)?
        .cache_properties(CacheProperties::No)
        .build()?)
}

/// The properties, and whether they are current: a sampled feed holds the last sample taken, maybe
/// long ago, until the fresh one a subscription brings, and a feed with `Ready` is current once it
/// says so
fn snapshot(props: &PropertiesProxy, interface: &str) -> Result<(Map<String, Json>, bool)> {
    let all = props.get_all(InterfaceName::try_from(interface)?)?;
    let mut state = Map::new();
    merge(&mut state, all.iter().map(|(name, value)| (name.as_str(), &**value)));
    let ready = all.get("Ready").is_none_or(|v| **v == Value::Bool(true));
    Ok((state, ready && !all.contains_key("Samples")))
}

fn subscribe(conn: &Connection, path: &str, interface: &str, lease: Lease) -> Result<()> {
    let proxy = Proxy::new(conn, BUS_NAME, path, interface)?;
    match lease {
        Lease::Every(every) => {
            let milliseconds = u32::try_from(every.as_millis()).unwrap_or(u32::MAX);
            proxy.call_method("Subscribe", &(milliseconds,))?;
        }
        Lease::Session => {
            proxy.call_method("Subscribe", &(Session::from_env().vars(),))?;
        }
        Lease::Plain => {
            proxy.call_method("Subscribe", &())?;
        }
    }
    Ok(())
}

fn sysinfo() -> Result<()> {
    let conn = connect()?;
    start(&conn)?;
    let props = properties(&conn, SYSINFO_PATH)?;
    let changes = props.receive_properties_changed()?;
    let (sampled, wait) = mpsc::channel();
    std::thread::spawn(move || {
        for signal in changes {
            if signal.args().is_ok_and(|args| args.changed_properties().contains_key("Samples")) {
                let _ = sampled.send(());
                return;
            }
        }
    });
    subscribe(&conn, SYSINFO_PATH, SYSINFO_INTERFACE, Lease::Every(WAIT))?;
    let _ = wait.recv_timeout(WAIT + Duration::from_secs(1));
    print(&Json::Object(snapshot(&props, SYSINFO_INTERFACE)?.0).to_string());
    Ok(())
}

/// Prints the properties now and after every change. When the daemon goes away it is started
/// again, and the lease taken again.
fn watch(path: &'static str, interface: &'static str, lease: Lease) -> Result<()> {
    let conn = connect()?;
    start(&conn)?;
    let props = properties(&conn, path)?;
    let changes = props.receive_properties_changed()?;
    let owners = DBusProxy::new(&conn)?.receive_name_owner_changed_with_args(&[(0, BUS_NAME)])?;

    let state = Arc::new(Mutex::new((Map::new(), false)));
    let resubscribe = {
        let (conn, state) = (conn.clone(), state.clone());
        move || -> Result<()> {
            let props = properties(&conn, path)?;
            subscribe(&conn, path, interface, lease)?;
            let mut state = state.lock().unwrap();
            let (values, current) = snapshot(&props, interface)?;
            *state = (values, current || matches!(lease, Lease::Every(_)));
            if current {
                print(&Json::Object(state.0.clone()).to_string());
            }
            Ok(())
        }
    };
    resubscribe()?;

    let restarter = conn.clone();
    std::thread::spawn(move || {
        for signal in owners {
            let Ok(args) = signal.args() else { continue };
            let outcome = if args.new_owner().is_some() {
                resubscribe()
            } else {
                sleep(RESTART_DELAY);
                start(&restarter)
            };
            if let Err(Error::Failed(message) | Error::Usage(message)) = outcome {
                eprintln!("serpantinum-core: {message}");
            }
        }
    });

    for signal in changes {
        let Ok(args) = signal.args() else { continue };
        if args.interface_name().as_str() != interface {
            continue;
        }
        let changed = args.changed_properties();
        let mut state = state.lock().unwrap();
        merge(&mut state.0, changed.iter().map(|(name, value)| (*name, value)));
        if let Some(Value::Bool(ready)) = changed.get("Ready") {
            state.1 = *ready;
        }
        if state.1 {
            print(&Json::Object(state.0.clone()).to_string());
        }
    }
    Err(failed("lost the session bus"))
}

/// The shell's idle helper protocol on stdin and stdout, served by the daemon while this runs
fn watch_idle(marker: &str) -> Result<()> {
    // SAFETY: prctl with PR_SET_PDEATHSIG only sets a flag of this process
    unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) };
    let conn = connect()?;
    start(&conn)?;
    let proxy = Proxy::new(&conn, BUS_NAME, IDLE_PATH, IDLE_INTERFACE)?;
    let rule =
        zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).sender(BUS_NAME)?.path(IDLE_PATH)?.build();
    let signals = zbus::blocking::MessageIterator::for_match_rule(rule, &conn, None)?;
    let owners = DBusProxy::new(&conn)?.receive_name_owner_changed_with_args(&[(0, BUS_NAME)])?;

    let shown: Arc<Mutex<Option<bool>>> = Arc::default();
    let show = {
        let shown = shown.clone();
        move |inhibited: bool| {
            let mut shown = shown.lock().unwrap();
            if *shown != Some(inhibited) {
                *shown = Some(inhibited);
                print(if inhibited { "inhibit 1" } else { "inhibit 0" });
            }
        }
    };
    let sleep_lock: Arc<Mutex<Option<bool>>> = Arc::default();
    let marker = marker.to_owned();
    let resubscribe = {
        let (proxy, sleep_lock, show) = (proxy.clone(), sleep_lock.clone(), show.clone());
        move || -> Result<()> {
            proxy.call_method("Subscribe", &(marker.as_str(),))?;
            if let Some(on) = *sleep_lock.lock().unwrap() {
                proxy.call_method("SetSleepLock", &(on,))?;
            }
            show(proxy.get_property::<bool>("Inhibited")?);
            Ok(())
        }
    };
    resubscribe()?;

    let restarter = conn.clone();
    std::thread::spawn(move || {
        for signal in owners {
            let Ok(args) = signal.args() else { continue };
            let outcome = if args.new_owner().is_some() {
                resubscribe()
            } else {
                sleep(RESTART_DELAY);
                start(&restarter)
            };
            if let Err(Error::Failed(message) | Error::Usage(message)) = outcome {
                eprintln!("serpantinum-core: {message}");
            }
        }
    });

    std::thread::spawn(move || {
        for line in std::io::stdin().lines() {
            let Ok(line) = line else { break };
            let on = match line.trim() {
                "sleep-lock on" => true,
                "sleep-lock off" => false,
                _ => continue,
            };
            *sleep_lock.lock().unwrap() = Some(on);
            if let Err(error) = proxy.call_method("SetSleepLock", &(on,)) {
                eprintln!("serpantinum-core: {error}");
            }
        }
        // The shell closed the pipe, so it is gone
        std::process::exit(0);
    });

    for message in signals.flatten() {
        let header = message.header();
        match header.member().map(|m| m.as_str()) {
            Some("Sleep") => print("sleep"),
            Some("Wake") => print("wake"),
            Some("Log") => {
                if let Ok(line) = message.body().deserialize::<String>() {
                    print(&format!("log {line}"));
                }
            }
            Some("PropertiesChanged") => {
                let body = message.body();
                let Ok((_, changed, _)) =
                    body.deserialize::<(String, HashMap<String, zbus::zvariant::Value>, Vec<String>)>()
                else {
                    continue;
                };
                if let Some(Value::Bool(inhibited)) = changed.get("Inhibited") {
                    show(*inhibited);
                }
            }
            _ => {}
        }
    }
    Err(failed("lost the session bus"))
}

/// Holds the screen time lease for as long as this runs, taken again when the daemon restarts
fn track_screentime() -> Result<()> {
    let conn = connect()?;
    start(&conn)?;
    let owners = DBusProxy::new(&conn)?.receive_name_owner_changed_with_args(&[(0, BUS_NAME)])?;
    let env: HashMap<String, String> = SCREENTIME_VARS
        .iter()
        .chain(SESSION_VARS)
        .filter_map(|k| Some((k.to_string(), std::env::var(k).ok()?)))
        .collect();
    let subscribe = || -> Result<()> {
        Proxy::new(&conn, BUS_NAME, SCREENTIME_PATH, SCREENTIME_INTERFACE)?.call_method("Subscribe", &(&env,))?;
        Ok(())
    };
    subscribe()?;
    for signal in owners {
        let Ok(args) = signal.args() else { continue };
        let outcome = if args.new_owner().is_some() {
            subscribe()
        } else {
            sleep(RESTART_DELAY);
            start(&conn)
        };
        if let Err(Error::Failed(message) | Error::Usage(message)) = outcome {
            eprintln!("serpantinum-core: {message}");
        }
    }
    Err(failed("lost the session bus"))
}

/// The statistics of a day straight from the database, as the wellbeing page asks for them
fn screentime_stats(date: Option<&str>, app: Option<&str>, db_dir: Option<&str>) -> Result<()> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let dir = db_dir
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("QS_STATE_FOCUSTIME").map(PathBuf::from))
        .unwrap_or_else(|| home.join(".local/state/quickshell/focustime"));
    screentime::migrate(&dir, &home);
    let db = dir.join("focustime.db");
    if !db.exists() {
        print(
            &serde_json::json!({
                "total": 0, "average": 0, "week_range": "", "yesterday": 0, "current": "History",
                "apps": [], "week_apps": [], "week": [], "month": [], "hourly": vec![0; 48],
                "week_heatmap": vec![[0; 24]; 7], "peak_usage_str": "N/A", "all_known_apps": [],
            })
            .to_string(),
        );
        return Ok(());
    }
    let conn = rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| failed(format!("cannot open {}: {e}", db.display())))?;
    let target = screentime::target_date(date);
    let label = date.map_or_else(|| target.iso(), str::to_owned);
    print(&screentime::stats(&conn, target, &label, app, app.unwrap_or("History")).to_string());
    Ok(())
}

fn brightness(args: &[&str]) -> Result<()> {
    let conn = connect()?;
    start(&conn)?;
    let proxy = Proxy::new(&conn, BUS_NAME, BRIGHTNESS_PATH, BRIGHTNESS_INTERFACE)?;
    let percent: i32 = match args {
        [] | ["get"] => proxy.call("Get", &())?,
        ["set", value] => {
            let value = value.parse::<i64>().map_err(|_| usage(format!("not a number: {value}")))?;
            proxy.call("Set", &(value.clamp(0, 100) as u32,))?
        }
        ["raise"] => proxy.call("Step", &(true,))?,
        ["lower"] => proxy.call("Step", &(false,))?,
        ["backend"] => {
            print(&proxy.get_property::<String>("Backend")?);
            return Ok(());
        }
        _ => return Err(usage("brightness takes get, set PERCENT, raise, lower or backend")),
    };
    print(&percent.to_string());
    Ok(())
}

fn default_cache_dir() -> PathBuf {
    let env = |name| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    env("QS_CACHE_CLIPBOARD").unwrap_or_else(|| {
        env("QS_CACHE_DIR")
            .or_else(|| env("XDG_CACHE_HOME").map(|c| c.join("serpantinum")))
            .unwrap_or_else(|| env("HOME").unwrap_or_default().join(".cache/serpantinum"))
            .join("clipboard")
    })
}

fn clipboard(args: &[&str], cache_dir: Option<&str>) -> Result<()> {
    let cache = cache_dir.map(PathBuf::from).unwrap_or_else(default_cache_dir).to_string_lossy().into_owned();
    let number = |text: &str| text.parse::<u32>().map_err(|_| usage(format!("not a number: {text}")));
    let conn = connect()?;
    let call = |method: &str, body: &dyn Fn(&Proxy) -> zbus::Result<()>| -> Result<()> {
        start(&conn)?;
        let proxy = Proxy::new(&conn, BUS_NAME, CLIPBOARD_PATH, CLIPBOARD_INTERFACE)?;
        body(&proxy).map_err(|e| failed(format!("{method} failed: {e}")))
    };
    match args {
        ["list", offset, limit] => {
            let (offset, limit) = (number(offset)?, number(limit)?);
            call("List", &|proxy| {
                let page: String = proxy.call("List", &(cache.as_str(), offset, limit))?;
                print(&page);
                Ok(())
            })
        }
        ["pin", id] => call("TogglePin", &|proxy| proxy.call("TogglePin", &(cache.as_str(), *id))),
        ["delete", id] => call("Delete", &|proxy| proxy.call("Delete", &(cache.as_str(), *id))),
        ["wipe"] => call("Wipe", &|proxy| proxy.call("Wipe", &(cache.as_str(),))),
        _ => Err(usage("clipboard takes list OFFSET LIMIT, pin ID, delete ID or wipe")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn property_names_become_camel_case_keys() {
        assert_eq!(json_key("RamPercent"), "ramPercent");
        assert_eq!(json_key("Cpu"), "cpu");
        assert_eq!(json_key(""), "");
    }

    #[test]
    fn options_are_split_from_positional_arguments() {
        let args: Vec<String> = ["watch", "--interval", "4", "sysinfo"].map(String::from).to_vec();
        let (positional, options) = options(&args).unwrap();
        assert_eq!(positional, ["watch", "sysinfo"]);
        assert_eq!(options.get("interval"), Some(&"4"));
        assert!(super::options(&["--cache-dir".to_owned()]).is_err());
    }

    #[test]
    fn the_sample_counter_stays_out_of_the_output() {
        let mut state = Map::new();
        let (cpu, samples, ready) = (Value::U32(7), Value::U64(3), Value::Bool(true));
        merge(&mut state, [("Cpu", &cpu), ("Samples", &samples), ("Ready", &ready)]);
        assert_eq!(Json::Object(state).to_string(), r#"{"cpu":7}"#);
    }

    #[test]
    fn json_properties_are_printed_as_json() {
        let mut state = Map::new();
        let (windows, name) = (Value::from(r#"[{"id":1}]"#), Value::from("[sway]"));
        merge(&mut state, [("Windows", &windows), ("Name", &name)]);
        assert_eq!(Json::Object(state).to_string(), r#"{"name":"[sway]","windows":[{"id":1}]}"#);
    }
}
