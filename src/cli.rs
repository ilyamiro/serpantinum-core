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

use crate::dbus::{
    BUS_NAME, CLIPBOARD_INTERFACE, CLIPBOARD_PATH, CORE_INTERFACE, LOCKS_INTERFACE, LOCKS_PATH, SYSINFO_INTERFACE,
    SYSINFO_PATH,
};

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
    if let Some(name) = options.keys().find(|n| !matches!(**n, "interval" | "cache-dir")) {
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
            watch(SYSINFO_PATH, SYSINFO_INTERFACE, Duration::from_secs_f64(seconds))
        }
        ["watch", "locks"] => watch(LOCKS_PATH, LOCKS_INTERFACE, LOCKS_INTERVAL),
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
    daemon.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::inherit());
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

fn json_value(value: &Value) -> Json {
    match value {
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
        if name != "Samples" {
            state.insert(json_key(name), json_value(value));
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

/// The properties, and whether they hold a sample yet: a feed that was never sampled shows zeros
fn snapshot(props: &PropertiesProxy, interface: &str) -> Result<(Map<String, Json>, bool)> {
    let all = props.get_all(InterfaceName::try_from(interface)?)?;
    let mut state = Map::new();
    merge(&mut state, all.iter().map(|(name, value)| (name.as_str(), &**value)));
    let sampled = all.get("Samples").is_none_or(|samples| **samples != Value::U64(0));
    Ok((state, sampled))
}

fn subscribe(conn: &Connection, path: &str, interface: &str, every: Duration) -> Result<()> {
    let milliseconds = u32::try_from(every.as_millis()).unwrap_or(u32::MAX);
    Proxy::new(conn, BUS_NAME, path, interface)?.call_method("Subscribe", &(milliseconds,))?;
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
    subscribe(&conn, SYSINFO_PATH, SYSINFO_INTERFACE, WAIT)?;
    let _ = wait.recv_timeout(WAIT + Duration::from_secs(1));
    print(&Json::Object(snapshot(&props, SYSINFO_INTERFACE)?.0).to_string());
    Ok(())
}

/// Prints the properties now and after every change. When the daemon goes away it is started
/// again, and the lease taken again.
fn watch(path: &'static str, interface: &'static str, every: Duration) -> Result<()> {
    let conn = connect()?;
    start(&conn)?;
    let props = properties(&conn, path)?;
    let changes = props.receive_properties_changed()?;
    let owners = DBusProxy::new(&conn)?.receive_name_owner_changed_with_args(&[(0, BUS_NAME)])?;

    let state = Arc::new(Mutex::new(Map::new()));
    let resubscribe = {
        let (conn, state) = (conn.clone(), state.clone());
        move || -> Result<()> {
            let props = properties(&conn, path)?;
            subscribe(&conn, path, interface, every)?;
            let mut state = state.lock().unwrap();
            let sampled;
            (*state, sampled) = snapshot(&props, interface)?;
            if sampled {
                print(&Json::Object(state.clone()).to_string());
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
        let mut state = state.lock().unwrap();
        merge(&mut state, args.changed_properties().iter().map(|(name, value)| (*name, value)));
        print(&Json::Object(state.clone()).to_string());
    }
    Err(failed("lost the session bus"))
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
        let (cpu, samples) = (Value::U32(7), Value::U64(3));
        merge(&mut state, [("Cpu", &cpu), ("Samples", &samples)]);
        assert_eq!(Json::Object(state).to_string(), r#"{"cpu":7}"#);
    }
}
