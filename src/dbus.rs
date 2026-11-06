//! The objects served on the session bus. Feeds sample only while some client holds a lease,
//! taken with `Subscribe` and ended by `Unsubscribe` or by the client leaving the bus.

use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use zbus::fdo::{RequestNameFlags, RequestNameReply};
use zbus::message::Header;
use zbus::zvariant::Value;
use zbus::{fdo, interface};

use crate::brightness::{self, Backend, Change, Ddc};
use crate::clipboard;
use crate::compositor::{self, Session, Waker};
use crate::idle::{self, Event, Inhibition, SleepLock};
use crate::leases::Leases;
use crate::locks::{self, Locks};
use crate::sysinfo::{Sampler, Usage};

pub const BUS_NAME: &str = "org.serpantinum.Core";
pub const CORE_INTERFACE: &str = "org.serpantinum.Core";
pub const SYSINFO_PATH: &str = "/sysinfo";
pub const SYSINFO_INTERFACE: &str = "org.serpantinum.Core.SysInfo";
pub const LOCKS_PATH: &str = "/locks";
pub const LOCKS_INTERFACE: &str = "org.serpantinum.Core.Locks";
pub const CLIPBOARD_PATH: &str = "/clipboard";
pub const CLIPBOARD_INTERFACE: &str = "org.serpantinum.Core.Clipboard";
pub const BRIGHTNESS_PATH: &str = "/brightness";
pub const BRIGHTNESS_INTERFACE: &str = "org.serpantinum.Core.Brightness";
pub const IDLE_PATH: &str = "/idle";
pub const IDLE_INTERFACE: &str = "org.serpantinum.Core.Idle";
pub const COMPOSITOR_PATH: &str = "/compositor";
pub const COMPOSITOR_INTERFACE: &str = "org.serpantinum.Core.Compositor";
const PROPERTIES_INTERFACE: &str = "org.freedesktop.DBus.Properties";
const MIN_INTERVAL: Duration = Duration::from_millis(100);
const PRIMING: Duration = Duration::from_millis(250);
const DEVICE_SETTLE: Duration = Duration::from_millis(500);
/// How long to wait before connecting again to a compositor that went away
const RECONNECT: Duration = Duration::from_secs(1);
/// How often the polled LEDs are looked up again, in case a keyboard came without a device event
const LED_RESCAN: Duration = Duration::from_secs(30);

type Properties = Vec<(&'static str, Value<'static>)>;

fn changed(before: Properties, after: Properties) -> HashMap<&'static str, Value<'static>> {
    after.into_iter().zip(before).filter(|((_, new), (_, old))| new != old).map(|(new, _)| new).collect()
}

fn emit(bus: &zbus::blocking::Connection, path: &str, interface: &str, changed: HashMap<&str, Value>) {
    if changed.is_empty() {
        return;
    }
    let body = (interface, changed, Vec::<&str>::new());
    if let Err(error) = bus.emit_signal(None::<&str>, path, PROPERTIES_INTERFACE, "PropertiesChanged", &body) {
        eprintln!("serpantinum-core: could not signal a change on {path}: {error}");
    }
}

fn owner(header: &Header<'_>) -> fdo::Result<String> {
    header.sender().map(|s| s.to_string()).ok_or_else(|| fdo::Error::Failed("the call has no sender".into()))
}

fn interval(milliseconds: u32) -> Duration {
    Duration::from_millis(milliseconds.into()).max(MIN_INTERVAL)
}

#[derive(Default)]
struct SysState {
    usage: Usage,
    samples: u64,
}

impl SysState {
    fn properties(&self) -> Properties {
        let u = &self.usage;
        vec![
            ("Cpu", u.cpu.into()),
            ("RamPercent", u.ram_percent.into()),
            ("RamGb", u.ram_gb.into()),
            ("Temp", u.temp.into()),
            ("NetRx", u.net_rx.into()),
            ("NetTx", u.net_tx.into()),
            ("DiskPercent", u.disk.percent.into()),
            ("DiskUsedGb", u.disk.used_gb.into()),
            ("DiskTotalGb", u.disk.total_gb.into()),
            ("Samples", self.samples.into()),
        ]
    }
}

pub struct SysFeed {
    pub leases: Leases,
    state: Mutex<SysState>,
}

impl SysFeed {
    pub fn new() -> Arc<SysFeed> {
        Arc::new(SysFeed { leases: Leases::default(), state: Mutex::default() })
    }

    /// Samples at the shortest interval leased, forever
    pub fn run(&self, bus: &zbus::blocking::Connection) {
        let mut sampler = Sampler::system();
        let mut last: Option<Instant> = None;
        loop {
            let every = self.leases.wait_interval();
            // CPU and network usage are differences between two samples, so after a pause a short
            // first measurement replaces the stale one
            if last.is_none_or(|at| at.elapsed() > every * 3) {
                sampler.reset_network();
                sampler.sample(Instant::now());
                std::thread::sleep(PRIMING);
            }
            let now = Instant::now();
            last = Some(now);
            let usage = sampler.sample(now);
            let changes = {
                let mut state = self.state.lock().unwrap();
                let before = state.properties();
                state.usage = usage;
                state.samples += 1;
                changed(before, state.properties())
            };
            emit(bus, SYSINFO_PATH, SYSINFO_INTERFACE, changes);
            self.leases.sleep(every);
        }
    }
}

pub struct LockFeed {
    pub leases: Leases,
    state: Mutex<Locks>,
}

fn lock_properties(locks: Locks) -> Properties {
    vec![("CapsLock", locks.caps.into()), ("NumLock", locks.num.into())]
}

impl LockFeed {
    pub fn new() -> Arc<LockFeed> {
        let state = locks::hyprland().unwrap_or_else(|| locks::read_leds(Path::new(locks::LEDS)));
        Arc::new(LockFeed { leases: Leases::default(), state: Mutex::new(state) })
    }

    /// Follows the keyboards' events when they can be opened, else polls the LEDs, and looks again
    /// whenever an input device is added or removed
    pub fn run(&self, bus: &zbus::blocking::Connection) {
        let watch = locks::DeviceWatch::new();
        loop {
            let keyboards = locks::open_keyboards();
            if keyboards.is_empty() {
                self.poll_leds(watch.as_ref(), bus);
            } else {
                self.follow(keyboards, watch.as_ref(), bus);
            }
            // udev names and permits a new device shortly after it appears
            std::thread::sleep(DEVICE_SETTLE);
        }
    }

    fn follow(
        &self,
        mut keyboards: Vec<std::fs::File>,
        watch: Option<&locks::DeviceWatch>,
        bus: &zbus::blocking::Connection,
    ) {
        use std::os::fd::AsRawFd;
        while !keyboards.is_empty() {
            let mut fds: Vec<i32> = keyboards.iter().map(|k| k.as_raw_fd()).collect();
            fds.extend(watch.map(|w| w.fd()));
            let Some(revents) = locks::wait_readable(&fds) else { return };
            if watch.is_some_and(|w| revents[keyboards.len()] != 0 && w.drain()) {
                return;
            }
            let mut seen = *self.state.lock().unwrap();
            let mut index = 0;
            keyboards.retain(|keyboard| {
                let ready = revents[index] != 0;
                index += 1;
                !ready || locks::read_keyboard(keyboard, &mut seen).is_some()
            });
            self.update(seen, bus);
        }
    }

    fn poll_leds(&self, watch: Option<&locks::DeviceWatch>, bus: &zbus::blocking::Connection) {
        let leds = locks::LedFiles::open(Path::new(locks::LEDS));
        let opened = Instant::now();
        loop {
            let every = self.leases.wait_interval();
            self.update(leds.read(), bus);
            if watch.is_some_and(|w| w.drain()) || opened.elapsed() >= LED_RESCAN {
                return;
            }
            self.leases.sleep(every);
        }
    }

    /// Takes a change the keyboards report, confirmed by Hyprland when running under it
    fn update(&self, seen: Locks, bus: &zbus::blocking::Connection) {
        if seen == *self.state.lock().unwrap() {
            return;
        }
        let now = locks::hyprland().unwrap_or(seen);
        let changes = {
            let mut state = self.state.lock().unwrap();
            let before = lock_properties(*state);
            *state = now;
            changed(before, lock_properties(now))
        };
        emit(bus, LOCKS_PATH, LOCKS_INTERFACE, changes);
    }
}

#[derive(Default)]
struct CompositorState {
    name: String,
    ready: bool,
    state: compositor::State,
}

impl CompositorState {
    fn properties(&self) -> Properties {
        let text = |json: &serde_json::Value| Value::from(json.to_string());
        vec![
            ("Name", self.name.clone().into()),
            ("Ready", self.ready.into()),
            ("Workspaces", text(&self.state.workspaces)),
            ("Windows", text(&self.state.windows)),
            ("Keyboard", text(&self.state.keyboard)),
        ]
    }
}

pub struct CompositorFeed {
    pub leases: Leases,
    session: Mutex<Session>,
    waker: Waker,
    state: Mutex<CompositorState>,
}

impl CompositorFeed {
    pub fn new() -> Arc<CompositorFeed> {
        Arc::new(CompositorFeed {
            leases: Leases::default(),
            session: Mutex::new(Session::from_env()),
            waker: Waker::new(),
            state: Mutex::default(),
        })
    }

    pub fn remove(&self, owner: &str) {
        self.leases.remove(owner);
        self.waker.wake();
    }

    fn publish(&self, bus: &zbus::blocking::Connection, update: impl FnOnce(&mut CompositorState)) {
        let changes = {
            let mut state = self.state.lock().unwrap();
            let before = state.properties();
            update(&mut state);
            changed(before, state.properties())
        };
        emit(bus, COMPOSITOR_PATH, COMPOSITOR_INTERFACE, changes);
    }

    /// Follows the compositor of the latest session while anyone watches, and connects again a
    /// second after it goes away
    pub fn run(&self, bus: &zbus::blocking::Connection) {
        loop {
            self.leases.wait_interval();
            let session = self.session.lock().unwrap().clone();
            let name = session.compositor();
            match compositor::connect(&session) {
                Some(Ok(adapter)) => self.follow(adapter, &session, bus),
                Some(Err(error)) => eprintln!("serpantinum-core: cannot follow {name}: {error}"),
                None => {
                    self.publish(bus, |s| {
                        *s = CompositorState { name: name.to_owned(), ready: true, ..Default::default() }
                    });
                    self.wait_while(&session, &[]);
                    continue;
                }
            }
            self.publish(bus, |s| s.ready = false);
            if self.current(&session) {
                self.leases.sleep(RECONNECT);
            }
        }
    }

    fn current(&self, session: &Session) -> bool {
        self.leases.active() && *self.session.lock().unwrap() == *session
    }

    /// Waits on `fds` and the waker while the session is current; the indices of readable `fds`
    fn wait_while(&self, session: &Session, fds: &[i32]) -> Option<Vec<usize>> {
        let mut all = fds.to_vec();
        all.push(self.waker.fd());
        loop {
            if !self.current(session) {
                return None;
            }
            let revents = locks::wait_readable(&all)?;
            if revents[fds.len()] != 0 {
                self.waker.drain();
                continue;
            }
            return Some((0..fds.len()).filter(|i| revents[*i] != 0).collect());
        }
    }

    fn follow(&self, mut adapter: Box<dyn compositor::Adapter>, session: &Session, bus: &zbus::blocking::Connection) {
        let name = session.compositor().to_owned();
        loop {
            let state = adapter.state();
            self.publish(bus, |s| *s = CompositorState { name: name.clone(), ready: true, state });
            let Some(ready) = self.wait_while(session, &adapter.fds()) else { return };
            for index in ready {
                if let Err(error) = adapter.read(index) {
                    eprintln!("serpantinum-core: lost {name}: {error}");
                    return;
                }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
struct BrightnessState {
    backend: Backend,
    percent: i32,
    ready: bool,
}

impl BrightnessState {
    fn properties(&self) -> Properties {
        vec![("Backend", self.backend.name().into()), ("Percent", self.percent.into()), ("Ready", self.ready.into())]
    }
}

pub struct BrightnessFeed {
    pub leases: Leases,
    waker: Waker,
    ddc: Ddc,
    state: Mutex<BrightnessState>,
    published: Mutex<Properties>,
}

impl BrightnessFeed {
    pub fn new() -> Arc<BrightnessFeed> {
        let state = BrightnessState { backend: Backend::None, percent: -1, ready: false };
        Arc::new(BrightnessFeed {
            leases: Leases::default(),
            waker: Waker::new(),
            ddc: Ddc::new(&brightness::run_dir()),
            published: Mutex::new(state.properties()),
            state: Mutex::new(state),
        })
    }

    pub fn remove(&self, owner: &str) {
        self.leases.remove(owner);
        self.waker.wake();
    }

    fn update(&self, update: impl FnOnce(&mut BrightnessState)) {
        update(&mut self.state.lock().unwrap());
        self.waker.wake();
    }

    /// Signals what changed since the last signal
    fn sync(&self, bus: &zbus::blocking::Connection) {
        let now = self.state.lock().unwrap().properties();
        let before = std::mem::replace(&mut *self.published.lock().unwrap(), now.clone());
        emit(bus, BRIGHTNESS_PATH, BRIGHTNESS_INTERFACE, changed(before, now));
    }

    /// While anyone watches: reads the brightness, then follows the backlight's files, or for DDC
    /// takes only the changes made through this daemon
    pub fn run(&self, bus: &zbus::blocking::Connection) {
        loop {
            self.leases.wait_interval();
            let backend = brightness::backend();
            let percent = brightness::get(&self.ddc).unwrap_or(-1);
            *self.state.lock().unwrap() = BrightnessState { backend, percent, ready: true };
            let watch = (backend == Backend::Backlight).then(|| brightness_watch(brightness::BACKLIGHT)).flatten();
            loop {
                self.sync(bus);
                let mut fds = vec![self.waker.fd()];
                fds.extend(watch.as_ref().map(AsRawFd::as_raw_fd));
                let Some(revents) = locks::wait_readable(&fds) else { break };
                if revents[0] != 0 {
                    self.waker.drain();
                }
                if !self.leases.active() {
                    break;
                }
                if let Some(watch) = watch.as_ref().filter(|_| revents.get(1).is_some_and(|r| *r != 0)) {
                    drain(watch);
                    if let Some(device) = brightness::backlight(Path::new(brightness::BACKLIGHT)) {
                        self.state.lock().unwrap().percent = device.percent();
                    }
                }
            }
            self.state.lock().unwrap().ready = false;
            self.sync(bus);
        }
    }

    fn change(&self, change: Change) -> fdo::Result<i32> {
        match brightness::change(&self.ddc, change) {
            Ok(percent) => {
                let percent = percent.unwrap_or(-1);
                self.update(|s| s.percent = percent);
                Ok(percent)
            }
            Err(error) => {
                if brightness::backend() == Backend::Ddc {
                    let percent = self.ddc.get().unwrap_or(-1);
                    self.update(|s| s.percent = percent);
                }
                Err(fdo::Error::Failed(error))
            }
        }
    }
}

/// An inotify descriptor that turns readable when a backlight's brightness is written
fn brightness_watch(root: &str) -> Option<OwnedFd> {
    // SAFETY: inotify_init1 has no preconditions; a valid descriptor is owned from here on
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    // SAFETY: fd is a fresh descriptor nothing else owns
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let watched = brightness::backlight_files(Path::new(root)).into_iter().filter(|file| {
        let Ok(path) = std::ffi::CString::new(file.as_os_str().as_encoded_bytes()) else { return false };
        // SAFETY: path is a valid C string and fd an inotify descriptor
        unsafe { libc::inotify_add_watch(fd.as_raw_fd(), path.as_ptr(), libc::IN_MODIFY) >= 0 }
    });
    (watched.count() > 0).then_some(fd)
}

fn drain(fd: &OwnedFd) {
    let mut buf = [0u8; 4096];
    // SAFETY: buf is writable for its whole length
    while unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
}

#[derive(Default)]
struct IdleState {
    inhibition: Inhibition,
    marker: PathBuf,
    sleep_lock: bool,
    logs: Vec<String>,
    published: bool,
}

pub struct IdleFeed {
    pub leases: Leases,
    state: Mutex<IdleState>,
    events: Mutex<std::sync::mpsc::Sender<Event>>,
    receiver: Mutex<Option<std::sync::mpsc::Receiver<Event>>>,
}

impl IdleFeed {
    pub fn new() -> Arc<IdleFeed> {
        let (events, receiver) = std::sync::mpsc::channel();
        Arc::new(IdleFeed {
            leases: Leases::default(),
            state: Mutex::default(),
            events: Mutex::new(events),
            receiver: Mutex::new(Some(receiver)),
        })
    }

    fn notify(&self, event: Event) {
        let _ = self.events.lock().unwrap().send(event);
    }

    fn log(&self, line: String) {
        self.state.lock().unwrap().logs.push(line);
        self.notify(Event::Changed);
    }

    /// A client left the bus: its lease and its inhibitors end
    pub fn remove(&self, owner: &str) {
        self.leases.remove(owner);
        self.state.lock().unwrap().inhibition.cookies.drop_owner(owner);
        self.notify(Event::Changed);
    }

    fn signal(bus: &zbus::blocking::Connection, member: &str, text: Option<&str>) {
        let sent = match text {
            Some(text) => bus.emit_signal(None::<&str>, IDLE_PATH, IDLE_INTERFACE, member, &(text,)),
            None => bus.emit_signal(None::<&str>, IDLE_PATH, IDLE_INTERFACE, member, &()),
        };
        if let Err(error) = sent {
            eprintln!("serpantinum-core: could not signal {member}: {error}");
        }
    }

    /// Signals the inhibition when it changed and the log lines gathered meanwhile
    fn publish(&self, bus: &zbus::blocking::Connection) {
        let (inhibited, before, logs) = {
            let mut state = self.state.lock().unwrap();
            let inhibited = state.inhibition.inhibited(Instant::now());
            let before = std::mem::replace(&mut state.published, inhibited);
            (inhibited, before, std::mem::take(&mut state.logs))
        };
        for line in logs {
            Self::signal(bus, "Log", Some(&line));
        }
        if inhibited != before {
            emit(bus, IDLE_PATH, IDLE_INTERFACE, HashMap::from([("Inhibited", Value::from(inhibited))]));
        }
    }

    /// Owns org.freedesktop.ScreenSaver and keeps the sleep lock while anyone watches
    pub fn run(&self, bus: &zbus::blocking::Connection) {
        let receiver = self.receiver.lock().unwrap().take().expect("the idle feed runs once");
        let events = self.events.lock().unwrap().clone();
        self.follow_screensaver_name(bus, events.clone());
        let mut sleep = SleepLock::default();
        loop {
            self.leases.wait_interval();
            let flags = RequestNameFlags::AllowReplacement | RequestNameFlags::ReplaceExisting;
            let owner =
                matches!(bus.request_name_with_flags(idle::SCREENSAVER, flags), Ok(RequestNameReply::PrimaryOwner));
            self.state.lock().unwrap().inhibition.owner = owner;
            while self.leases.active() {
                let (enabled, marker) = {
                    let state = self.state.lock().unwrap();
                    (state.sleep_lock, state.marker.clone())
                };
                sleep.set_enabled(enabled, &events);
                sleep.tick(&marker, &events);
                self.publish(bus);
                let now = Instant::now();
                let deadline =
                    [self.state.lock().unwrap().inhibition.deadline(now), sleep.deadline()].into_iter().flatten().min();
                let event = match deadline {
                    Some(at) => receiver.recv_timeout(at.saturating_duration_since(now)).ok(),
                    None => receiver.recv().ok(),
                };
                match event {
                    Some(Event::PrepareForSleep(sleeping)) if sleep.prepare_for_sleep(sleeping) => {
                        Self::signal(bus, if sleeping { "Sleep" } else { "Wake" }, None);
                    }
                    Some(Event::SystemLost) => sleep.lost(),
                    Some(Event::ScreenSaverOwned(owned)) => {
                        let mut state = self.state.lock().unwrap();
                        state.inhibition.owner = owned;
                        if !owned {
                            state.inhibition.reset();
                        }
                    }
                    _ => {}
                }
            }
            let _ = bus.release_name(idle::SCREENSAVER);
            sleep.set_enabled(false, &events);
            {
                let mut state = self.state.lock().unwrap();
                state.inhibition.reset();
                state.inhibition.owner = false;
                state.sleep_lock = false;
            }
            self.publish(bus);
        }
    }

    fn follow_screensaver_name(&self, bus: &zbus::blocking::Connection, events: std::sync::mpsc::Sender<Event>) {
        let Ok(proxy) = zbus::blocking::fdo::DBusProxy::new(bus) else { return };
        let (Ok(acquired), Ok(lost)) = (proxy.receive_name_acquired(), proxy.receive_name_lost()) else { return };
        let other = events.clone();
        std::thread::spawn(move || {
            for signal in acquired {
                if signal.args().is_ok_and(|a| a.name() == idle::SCREENSAVER) {
                    let _ = other.send(Event::ScreenSaverOwned(true));
                }
            }
        });
        std::thread::spawn(move || {
            for signal in lost {
                if signal.args().is_ok_and(|a| a.name() == idle::SCREENSAVER) {
                    let _ = events.send(Event::ScreenSaverOwned(false));
                }
            }
        });
    }
}

pub struct Core;

#[interface(name = "org.serpantinum.Core")]
impl Core {
    fn quit(&self) {
        // Exits once the reply is on its way
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(100));
            std::process::exit(0);
        });
    }

    #[zbus(property)]
    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }
}

pub struct SysInfo(pub Arc<SysFeed>);

impl SysInfo {
    fn usage(&self) -> Usage {
        self.0.state.lock().unwrap().usage
    }
}

#[interface(name = "org.serpantinum.Core.SysInfo")]
impl SysInfo {
    /// Samples every `interval_ms` (or faster, for another client) until this client unsubscribes
    fn subscribe(&self, #[zbus(header)] header: Header<'_>, interval_ms: u32) -> fdo::Result<()> {
        self.0.leases.add(&owner(&header)?, interval(interval_ms));
        Ok(())
    }

    fn unsubscribe(&self, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.0.leases.remove(&owner(&header)?);
        Ok(())
    }

    #[zbus(property)]
    fn cpu(&self) -> u32 {
        self.usage().cpu
    }

    #[zbus(property)]
    fn ram_percent(&self) -> u32 {
        self.usage().ram_percent
    }

    #[zbus(property)]
    fn ram_gb(&self) -> f64 {
        self.usage().ram_gb
    }

    #[zbus(property)]
    fn temp(&self) -> u32 {
        self.usage().temp
    }

    #[zbus(property)]
    fn net_rx(&self) -> f64 {
        self.usage().net_rx
    }

    #[zbus(property)]
    fn net_tx(&self) -> f64 {
        self.usage().net_tx
    }

    #[zbus(property)]
    fn disk_percent(&self) -> u32 {
        self.usage().disk.percent
    }

    #[zbus(property)]
    fn disk_used_gb(&self) -> f64 {
        self.usage().disk.used_gb
    }

    #[zbus(property)]
    fn disk_total_gb(&self) -> f64 {
        self.usage().disk.total_gb
    }

    /// Counts samples, so every sample signals a change even when the values stay the same
    #[zbus(property)]
    fn samples(&self) -> u64 {
        self.0.state.lock().unwrap().samples
    }
}

pub struct LockState(pub Arc<LockFeed>);

#[interface(name = "org.serpantinum.Core.Locks")]
impl LockState {
    fn subscribe(&self, #[zbus(header)] header: Header<'_>, interval_ms: u32) -> fdo::Result<()> {
        self.0.leases.add(&owner(&header)?, interval(interval_ms));
        Ok(())
    }

    fn unsubscribe(&self, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.0.leases.remove(&owner(&header)?);
        Ok(())
    }

    #[zbus(property)]
    fn caps_lock(&self) -> bool {
        self.0.state.lock().unwrap().caps
    }

    #[zbus(property)]
    fn num_lock(&self) -> bool {
        self.0.state.lock().unwrap().num
    }
}

pub struct BrightnessControl(pub Arc<BrightnessFeed>);

#[interface(name = "org.serpantinum.Core.Brightness")]
impl BrightnessControl {
    /// Follows the brightness until this client unsubscribes
    fn subscribe(&self, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.0.leases.add(&owner(&header)?, MIN_INTERVAL);
        self.0.waker.wake();
        Ok(())
    }

    fn unsubscribe(&self, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.0.remove(&owner(&header)?);
        Ok(())
    }

    /// The brightness now, read afresh, in percent; -1 when there is nothing to read
    async fn get(&self) -> i32 {
        let feed = self.0.clone();
        blocking::unblock(move || brightness::get(&feed.ddc).unwrap_or(-1)).await
    }

    /// Sets the brightness in percent and returns what it became
    async fn set(&self, percent: u32) -> fdo::Result<i32> {
        let feed = self.0.clone();
        let percent = percent.min(100) as i32;
        blocking::unblock(move || feed.change(Change::Set(percent))).await
    }

    /// Raises (or lowers) the brightness by five percent and returns what it became
    async fn step(&self, up: bool) -> fdo::Result<i32> {
        let feed = self.0.clone();
        blocking::unblock(move || feed.change(Change::Step(up))).await
    }

    #[zbus(property)]
    fn backend(&self) -> String {
        brightness::backend().name().to_owned()
    }

    /// The brightness last read or set, in percent; -1 when unknown
    #[zbus(property)]
    fn percent(&self) -> i32 {
        self.0.state.lock().unwrap().percent
    }

    /// False while nobody watches, when `Percent` may be stale
    #[zbus(property)]
    fn ready(&self) -> bool {
        self.0.state.lock().unwrap().ready
    }
}

pub struct Idle(pub Arc<IdleFeed>);

#[interface(name = "org.serpantinum.Core.Idle")]
impl Idle {
    /// Serves org.freedesktop.ScreenSaver until this client unsubscribes. `marker` is the file the
    /// lock screen creates, which lets a held-back sleep go on.
    fn subscribe(&self, #[zbus(header)] header: Header<'_>, marker: String) -> fdo::Result<()> {
        self.0.state.lock().unwrap().marker = marker.into();
        self.0.leases.add(&owner(&header)?, MIN_INTERVAL);
        self.0.notify(Event::Changed);
        Ok(())
    }

    fn unsubscribe(&self, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.0.remove(&owner(&header)?);
        Ok(())
    }

    /// Holds sleep back until the screen is locked, and signals `Sleep` and `Wake`
    fn set_sleep_lock(&self, enabled: bool) {
        self.0.state.lock().unwrap().sleep_lock = enabled;
        self.0.notify(Event::Changed);
    }

    /// Whether an app asks the session not to idle
    #[zbus(property)]
    fn inhibited(&self) -> bool {
        self.0.state.lock().unwrap().inhibition.inhibited(Instant::now())
    }
}

#[derive(Clone)]
pub struct ScreenSaver(pub Arc<IdleFeed>);

#[interface(name = "org.freedesktop.ScreenSaver")]
impl ScreenSaver {
    fn inhibit(
        &self,
        #[zbus(header)] header: Header<'_>,
        application_name: String,
        reason_for_inhibit: String,
    ) -> fdo::Result<u32> {
        let sender = owner(&header)?;
        let cookie = self.0.state.lock().unwrap().inhibition.cookies.add(&sender);
        self.0.log(format!("inhibit {cookie} from {sender} ({application_name}: {reason_for_inhibit})"));
        Ok(cookie)
    }

    fn un_inhibit(&self, cookie: u32) {
        if self.0.state.lock().unwrap().inhibition.cookies.remove(cookie) {
            self.0.log(format!("uninhibit {cookie}"));
        } else {
            self.0.notify(Event::Changed);
        }
    }

    fn simulate_user_activity(&self, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        let sender = owner(&header)?;
        let now = Instant::now();
        let quiet = {
            let mut state = self.0.state.lock().unwrap();
            let quiet = state.inhibition.deadline(now).is_none();
            state.inhibition.activity_until = Some(now + idle::ACTIVITY_HOLD);
            quiet
        };
        if quiet {
            self.0.log(format!("user activity from {sender}"));
        } else {
            self.0.notify(Event::Changed);
        }
        Ok(())
    }

    fn get_active(&self) -> bool {
        false
    }
}

pub struct Compositor(pub Arc<CompositorFeed>);

#[interface(name = "org.serpantinum.Core.Compositor")]
impl Compositor {
    /// Follows the compositor that `session` names until this client unsubscribes. The variables
    /// are those of `compositor::SESSION_VARS`; the latest subscriber's session is followed.
    fn subscribe(&self, #[zbus(header)] header: Header<'_>, session: HashMap<String, String>) -> fdo::Result<()> {
        *self.0.session.lock().unwrap() = Session::new(session);
        self.0.leases.add(&owner(&header)?, MIN_INTERVAL);
        self.0.waker.wake();
        Ok(())
    }

    fn unsubscribe(&self, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.0.remove(&owner(&header)?);
        Ok(())
    }

    #[zbus(property)]
    fn name(&self) -> String {
        self.0.state.lock().unwrap().name.clone()
    }

    /// False while not connected, when the other properties may be stale
    #[zbus(property)]
    fn ready(&self) -> bool {
        self.0.state.lock().unwrap().ready
    }

    /// JSON: the active workspace, the active one per output and the occupied ones
    #[zbus(property)]
    fn workspaces(&self) -> String {
        self.0.state.lock().unwrap().state.workspaces.to_string()
    }

    /// JSON: the windows, in the shape the compositor's singleton in the shell keeps
    #[zbus(property)]
    fn windows(&self) -> String {
        self.0.state.lock().unwrap().state.windows.to_string()
    }

    /// JSON: the keyboard layouts and the active one
    #[zbus(property)]
    fn keyboard(&self) -> String {
        self.0.state.lock().unwrap().state.keyboard.to_string()
    }
}

pub struct Clipboard {
    pub home: PathBuf,
}

#[interface(name = "org.serpantinum.Core.Clipboard")]
impl Clipboard {
    /// A page of the history as a JSON array; images are decoded into `cache_dir`
    async fn list(&self, cache_dir: String, offset: u32, limit: u32) -> String {
        let home = self.home.clone();
        blocking::unblock(move || clipboard::list(Path::new(&cache_dir), &home, offset as usize, limit as usize)).await
    }

    async fn toggle_pin(&self, cache_dir: String, id: String) {
        blocking::unblock(move || clipboard::toggle_pin(Path::new(&cache_dir), &id)).await
    }

    async fn delete(&self, cache_dir: String, id: String) {
        blocking::unblock(move || clipboard::delete(Path::new(&cache_dir), &id)).await
    }

    async fn wipe(&self, cache_dir: String) {
        blocking::unblock(move || clipboard::wipe(Path::new(&cache_dir))).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_moved_properties_are_signalled() {
        let before = SysState::default();
        let mut after = SysState { samples: 1, ..SysState::default() };
        after.usage.cpu = 12;
        let changes = changed(before.properties(), after.properties());
        let mut names: Vec<&str> = changes.keys().copied().collect();
        names.sort();
        assert_eq!(names, ["Cpu", "Samples"]);
    }

    #[test]
    fn intervals_have_a_floor() {
        assert_eq!(interval(0), MIN_INTERVAL);
        assert_eq!(interval(2000), Duration::from_secs(2));
    }
}
