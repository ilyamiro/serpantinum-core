//! `notify` against a stand-in notification server on a private session bus, which the tests start
//! with dbus-daemon. This is only how the tests get a bus: the command itself uses the user's session
//! bus, whichever program runs it. Where dbus-daemon or notify-send is missing the tests that need
//! it are skipped, unless SERPANTINUM_REQUIRE_DBUS is set, as it is in CI.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::{Duration, Instant};

use zbus::blocking::Connection;
use zbus::blocking::connection::Builder;
use zbus::zvariant::OwnedValue;

const SERVER: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
struct Received {
    app: String,
    icon: String,
    summary: String,
    body: String,
    actions: Vec<String>,
    hints: HashMap<String, OwnedValue>,
    expire: i32,
}

type Inbox = Arc<Mutex<Vec<(u32, Received)>>>;

#[derive(Default)]
struct Server {
    next: u32,
    received: Inbox,
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Server {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &mut self,
        app: String,
        _replaces: u32,
        icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire: i32,
    ) -> u32 {
        self.next += 1;
        self.received.lock().unwrap().push((self.next, Received { app, icon, summary, body, actions, hints, expire }));
        self.next
    }

    fn get_capabilities(&self) -> Vec<String> {
        vec!["actions".into(), "body".into()]
    }

    fn get_server_information(&self) -> (String, String, String, String) {
        ("mock".into(), "serpantinum".into(), "1".into(), "1.2".into())
    }

    fn close_notification(&self, _id: u32) {}
}

struct Bus {
    daemon: Child,
    address: String,
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

fn bus() -> Option<Bus> {
    let mut daemon = Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut address = String::new();
    BufReader::new(daemon.stdout.take()?).read_line(&mut address).ok()?;
    Some(Bus { daemon, address: address.trim().to_string() })
}

/// A server that numbers notifications from `first`, so ids of other notifications can be faked
fn server(bus: &Bus, first: u32) -> (Connection, Inbox) {
    let received = Arc::default();
    let server = Server { next: first - 1, received: Arc::clone(&received) };
    let conn = Builder::address(bus.address.as_str()).unwrap().name(SERVER).unwrap().serve_at(PATH, server).unwrap();
    (conn.build().unwrap(), received)
}

fn client(bus: &Bus, args: &[&str]) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_serpantinum-core"));
    command.arg("notify").args(args);
    spawn(bus, command)
}

fn spawn(bus: &Bus, mut command: Command) -> Child {
    command.env("DBUS_SESSION_BUS_ADDRESS", &bus.address).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap()
}

fn finish(mut child: Child) -> Output {
    let deadline = Instant::now() + TIMEOUT;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("notify did not exit");
        }
        sleep(Duration::from_millis(20));
    }
    let mut output = Output { status: child.wait().unwrap(), stdout: Vec::new(), stderr: Vec::new() };
    child.stdout.take().unwrap().read_to_end(&mut output.stdout).unwrap();
    child.stderr.take().unwrap().read_to_end(&mut output.stderr).unwrap();
    output
}

fn first_notification(received: &Mutex<Vec<(u32, Received)>>) -> (u32, Received) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(first) = received.lock().unwrap().first() {
            return first.clone();
        }
        assert!(Instant::now() < deadline, "the server got no notification");
        sleep(Duration::from_millis(20));
    }
}

fn invoked(conn: &Connection, id: u32, action: &str) {
    conn.emit_signal(None::<()>, PATH, SERVER, "ActionInvoked", &(id, action)).unwrap();
}

fn closed(conn: &Connection, id: u32) {
    conn.emit_signal(None::<()>, PATH, SERVER, "NotificationClosed", &(id, 2u32)).unwrap();
}

fn still_waiting(child: &mut Child) -> bool {
    sleep(Duration::from_millis(300));
    child.try_wait().unwrap().is_none()
}

fn stdout(output: &Output) -> &str {
    std::str::from_utf8(&output.stdout).unwrap()
}

macro_rules! need_bus {
    () => {
        match bus() {
            Some(bus) => bus,
            None => {
                assert!(std::env::var_os("SERPANTINUM_REQUIRE_DBUS").is_none(), "dbus-daemon is required");
                eprintln!("no dbus-daemon, skipped");
                return;
            }
        }
    };
}

#[test]
fn sends_everything_and_returns_at_once_without_actions() {
    let bus = need_bus!();
    let (_server, received) = server(&bus, 1);
    let output =
        finish(client(&bus, &["--app", "Shot", "--urgency", "critical", "--icon", "/tmp/a b.png", "Saved", "Body"]));
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(stdout(&output), "");
    let (_, n) = first_notification(&received);
    assert_eq!((n.app.as_str(), n.icon.as_str(), n.summary.as_str(), n.body.as_str()), ("Shot", "", "Saved", "Body"));
    assert!(n.actions.is_empty());
    assert_eq!(n.expire, -1);
    assert_eq!(u8::try_from(&n.hints["urgency"]).unwrap(), 2);
    assert_eq!(<&str>::try_from(&n.hints["image-path"]).unwrap(), "/tmp/a b.png");
    assert!(i64::try_from(&n.hints["sender-pid"]).unwrap() > 0);
}

#[test]
fn defaults_to_normal_urgency_without_an_image() {
    let bus = need_bus!();
    let (_server, received) = server(&bus, 1);
    assert!(finish(client(&bus, &["Only a summary"])).status.success());
    let (_, n) = first_notification(&received);
    assert_eq!((n.summary.as_str(), n.body.as_str()), ("Only a summary", ""));
    assert_eq!(u8::try_from(&n.hints["urgency"]).unwrap(), 1);
    assert!(!n.hints.contains_key("image-path"));
}

#[test]
fn prints_the_clicked_action_and_ignores_other_notifications() {
    let bus = need_bus!();
    let (conn, received) = server(&bus, 7);
    let mut child = client(&bus, &["--action", "default=Open", "--action", "later=Remind me", "Update", "v2"]);
    let (id, n) = first_notification(&received);
    assert_eq!(n.actions, ["default", "Open", "later", "Remind me"]);

    invoked(&conn, id + 1, "default");
    closed(&conn, id - 1);
    assert!(still_waiting(&mut child), "exited on another notification's signal");

    invoked(&conn, id, "later");
    let output = finish(child);
    assert!(output.status.success());
    assert_eq!(stdout(&output), "later\n");
}

#[test]
fn prints_the_default_action() {
    let bus = need_bus!();
    let (conn, received) = server(&bus, 1);
    let child = client(&bus, &["--action", "default=Open folder", "Screenshot saved"]);
    let (id, _) = first_notification(&received);
    invoked(&conn, id, "default");
    closed(&conn, id);
    let output = finish(child);
    assert_eq!((output.status.success(), stdout(&output)), (true, "default\n"));
}

#[test]
fn prints_nothing_when_the_notification_closes() {
    let bus = need_bus!();
    let (conn, received) = server(&bus, 1);
    let mut child = client(&bus, &["--action", "default=Open", "Dismiss me"]);
    let (id, _) = first_notification(&received);
    assert!(still_waiting(&mut child));
    closed(&conn, id);
    let output = finish(child);
    assert_eq!((output.status.success(), stdout(&output)), (true, ""));
}

#[test]
fn stops_waiting_when_the_server_leaves() {
    let bus = need_bus!();
    let (conn, received) = server(&bus, 1);
    let mut child = client(&bus, &["--action", "default=Open", "Shell restarts"]);
    first_notification(&received);
    assert!(still_waiting(&mut child));
    drop(conn);
    let output = finish(child);
    assert_eq!((output.status.success(), stdout(&output)), (true, ""));
}

#[test]
fn fails_without_a_server() {
    let bus = need_bus!();
    let output = finish(client(&bus, &["Nobody listens"]));
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot notify"));
}

#[test]
fn rejects_bad_usage_before_touching_the_bus() {
    let bus = need_bus!();
    let output = finish(client(&bus, &["--urgency", "loud", "S"]));
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(stdout(&output), "");
}

/// Sends one notification with `program`, clicks `click` or closes it, and returns what the server
/// got and what the program printed
fn sent_by(bus: &Bus, mut program: Command, waits: bool, click: Option<&str>) -> (Received, bool, String) {
    let (conn, received) = server(bus, 1);
    program.env("DBUS_SESSION_BUS_ADDRESS", &bus.address).stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = program.spawn().unwrap();
    let (id, n) = first_notification(&received);
    if waits {
        sleep(Duration::from_millis(200));
        if let Some(action) = click {
            invoked(&conn, id, action);
        }
        closed(&conn, id);
    }
    let output = finish(child);
    (n, output.status.success(), stdout(&output).to_string())
}

/// Hints libnotify only sends from 0.8.8, which also moved the icon from the app icon field to the
/// `image-path` hint. `notify` sends what 0.8.8 sends, and against an older notify-send only the
/// icon's place and these hints may differ.
const OLD_LIBNOTIFY_HINTS: &[&str] = &["sender-pid", "image-path"];

fn libnotify_version() -> (u32, u32, u32) {
    let output = Command::new("notify-send").arg("--version").output().unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    let mut parts = text.split_whitespace().last().unwrap_or_default().split('.').map(|p| p.parse().unwrap_or(0));
    (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0))
}

#[test]
fn matches_notify_send() {
    let bus = need_bus!();
    if Command::new("notify-send").arg("--version").output().is_err() {
        assert!(std::env::var_os("SERPANTINUM_REQUIRE_DBUS").is_none(), "notify-send is required");
        eprintln!("no notify-send, skipped");
        return;
    }
    let modern = libnotify_version() >= (0, 8, 8);
    let cases: &[(&[&str], &[&str], Option<&str>)] = &[
        (&["--app", "Shot", "Saved", "Body"], &["-a", "Shot", "Saved", "Body"], None),
        (&["--icon", "dialog-warning", "--urgency", "low", "S"], &["-i", "dialog-warning", "-u", "low", "S"], None),
        (
            &["--icon", "/tmp/a b.png", "--urgency", "critical", "S", "B"],
            &["-i", "/tmp/a b.png", "-u", "critical", "S", "B"],
            None,
        ),
        (&["--urgency", "normal", "S"], &["-u", "normal", "S"], None),
        (
            &["--app", "Shot", "--icon", "/tmp/a.png", "--action", "default=Open", "--action", "later=Later", "S", "B"],
            &["-a", "Shot", "-i", "/tmp/a.png", "-A", "default=Open", "-A", "later=Later", "S", "B"],
            Some("later"),
        ),
        (&["--action", "default=Open", "S"], &["-A", "default=Open", "S"], Some("default")),
        (&["--action", "default=Open", "S"], &["-A", "default=Open", "S"], None),
    ];
    for (ours, theirs, click) in cases {
        let waits = ours.contains(&"--action");
        let mut our_program = Command::new(env!("CARGO_BIN_EXE_serpantinum-core"));
        our_program.arg("notify").args(*ours);
        let mut their_program = Command::new("notify-send");
        their_program.args(*theirs);
        let (a, a_ok, a_out) = sent_by(&bus, our_program, waits, *click);
        let (b, b_ok, b_out) = sent_by(&bus, their_program, waits, *click);
        assert_eq!((a_ok, &a_out), (b_ok, &b_out), "{ours:?}");
        assert_eq!(a_out, click.map(|c| format!("{c}\n")).unwrap_or_default(), "{ours:?}");
        assert_eq!(
            (&a.app, &a.summary, &a.body, &a.actions, a.expire, a.hints.get("urgency")),
            (&b.app, &b.summary, &b.body, &b.actions, b.expire, b.hints.get("urgency")),
            "{ours:?}"
        );
        let hint_names = |n: &Received| {
            let mut names: Vec<String> =
                n.hints.keys().filter(|k| modern || !OLD_LIBNOTIFY_HINTS.contains(&k.as_str())).cloned().collect();
            names.sort();
            names
        };
        assert_eq!(hint_names(&a), hint_names(&b), "{ours:?}");
        let icon = |n: &Received| {
            n.hints.get("image-path").map(|v| <&str>::try_from(v).unwrap().to_string()).unwrap_or_default()
        };
        if modern {
            assert_eq!((&a.icon, icon(&a)), (&b.icon, icon(&b)), "{ours:?}");
        } else {
            let theirs = if b.icon.is_empty() { icon(&b) } else { b.icon.clone() };
            assert_eq!(icon(&a), theirs, "{ours:?}");
        }
    }
}
