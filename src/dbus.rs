//! The objects served on the session bus. Feeds sample only while some client holds a lease,
//! taken with `Subscribe` and ended by `Unsubscribe` or by the client leaving the bus.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use zbus::message::Header;
use zbus::zvariant::Value;
use zbus::{fdo, interface};

use crate::clipboard;
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
const PROPERTIES_INTERFACE: &str = "org.freedesktop.DBus.Properties";
const MIN_INTERVAL: Duration = Duration::from_millis(100);

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
            let now = Instant::now();
            if last.is_none_or(|at| now.duration_since(at) > every * 3) {
                sampler.reset_network();
            }
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
        Arc::new(LockFeed { leases: Leases::default(), state: Mutex::new(locks::read(Path::new(locks::LEDS))) })
    }

    pub fn run(&self, bus: &zbus::blocking::Connection) {
        loop {
            let every = self.leases.wait_interval();
            let now = locks::read(Path::new(locks::LEDS));
            let changes = {
                let mut state = self.state.lock().unwrap();
                let before = lock_properties(*state);
                *state = now;
                changed(before, lock_properties(now))
            };
            emit(bus, LOCKS_PATH, LOCKS_INTERFACE, changes);
            self.leases.sleep(every);
        }
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
