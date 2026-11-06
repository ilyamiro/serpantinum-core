//! What keeps the session awake or asks it to lock: apps inhibiting the screensaver through
//! org.freedesktop.ScreenSaver, and logind's sleep, which a delay inhibitor holds back until the
//! screen is locked. Both are served only while the shell watches, as its idle helper did.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

pub const SCREENSAVER: &str = "org.freedesktop.ScreenSaver";
pub const SCREENSAVER_PATHS: [&str; 2] = ["/org/freedesktop/ScreenSaver", "/ScreenSaver"];
/// How long a simulated user activity keeps the session awake
pub const ACTIVITY_HOLD: Duration = Duration::from_secs(60);
/// How long sleep waits for the lock screen at most
const LOCK_WAIT: Duration = Duration::from_secs(4);
const MARKER_POLL: Duration = Duration::from_millis(50);
const RETRY_DELAY: Duration = Duration::from_secs(10);

/// What wakes the idle feed
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Changed,
    PrepareForSleep(bool),
    SystemLost,
    ScreenSaverOwned(bool),
}

/// Inhibitor cookies and the clients that hold them
#[derive(Debug, Default)]
pub struct Cookies {
    owners: HashMap<u32, String>,
    next: u32,
}

impl Cookies {
    pub fn add(&mut self, owner: &str) -> u32 {
        while self.next == 0 || self.owners.contains_key(&self.next) {
            self.next = self.next.wrapping_add(1);
        }
        let cookie = self.next;
        self.next = self.next.wrapping_add(1);
        self.owners.insert(cookie, owner.to_owned());
        cookie
    }

    pub fn remove(&mut self, cookie: u32) -> bool {
        self.owners.remove(&cookie).is_some()
    }

    /// Ends the inhibitors of a client that left the bus
    pub fn drop_owner(&mut self, owner: &str) {
        self.owners.retain(|_, o| o != owner);
    }

    pub fn clear(&mut self) {
        self.owners.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.owners.is_empty()
    }
}

#[derive(Debug, Default)]
pub struct Inhibition {
    pub cookies: Cookies,
    pub activity_until: Option<Instant>,
    pub owner: bool,
}

impl Inhibition {
    pub fn inhibited(&self, now: Instant) -> bool {
        self.owner && (!self.cookies.is_empty() || self.activity_until.is_some_and(|until| now < until))
    }

    /// When a simulated activity runs out, if one is running
    pub fn deadline(&self, now: Instant) -> Option<Instant> {
        self.activity_until.filter(|until| *until > now)
    }

    pub fn reset(&mut self) {
        self.cookies.clear();
        self.activity_until = None;
    }
}

fn take_inhibitor(system: &zbus::blocking::Connection) -> zbus::Result<OwnedFd> {
    let reply = system.call_method(
        Some("org.freedesktop.login1"),
        "/org/freedesktop/login1",
        Some("org.freedesktop.login1.Manager"),
        "Inhibit",
        &("sleep", "Serpantinum", "Lock the screen before sleep", "delay"),
    )?;
    let fd: zbus::zvariant::OwnedFd = reply.body().deserialize()?;
    Ok(fd.into())
}

/// The logind side: the system bus, the delay inhibitor and when to let sleep go on
#[derive(Default)]
pub struct SleepLock {
    enabled: bool,
    system: Option<zbus::blocking::Connection>,
    inhibitor: Option<OwnedFd>,
    release_at: Option<Instant>,
    retry_at: Option<Instant>,
}

impl SleepLock {
    pub fn set_enabled(&mut self, enabled: bool, events: &Sender<Event>) {
        if enabled == self.enabled {
            return;
        }
        self.enabled = enabled;
        if enabled {
            self.connect(events);
        } else {
            self.disconnect();
        }
    }

    fn connect(&mut self, events: &Sender<Event>) {
        self.retry_at = None;
        if self.system.is_none() {
            match listen(events.clone()) {
                Ok(system) => self.system = Some(system),
                Err(error) => {
                    eprintln!("serpantinum-core: sleep lock unavailable: {error}");
                    self.retry_at = Some(Instant::now() + RETRY_DELAY);
                    return;
                }
            }
        }
        if let Some(system) = &self.system {
            match take_inhibitor(system) {
                Ok(fd) => self.inhibitor = Some(fd),
                Err(error) => {
                    eprintln!("serpantinum-core: sleep lock unavailable: {error}");
                    self.retry_at = Some(Instant::now() + RETRY_DELAY);
                }
            }
        }
    }

    pub fn disconnect(&mut self) {
        self.release_at = None;
        self.retry_at = None;
        self.inhibitor = None;
    }

    /// The system bus went away; it is connected to again later
    pub fn lost(&mut self) {
        self.system = None;
        self.disconnect();
        if self.enabled {
            self.retry_at = Some(Instant::now() + RETRY_DELAY);
        }
    }

    /// Takes logind's announcement; true when it is to be passed on to the shell
    pub fn prepare_for_sleep(&mut self, sleeping: bool) -> bool {
        if !self.enabled {
            return false;
        }
        if sleeping {
            self.release_at = Some(Instant::now() + LOCK_WAIT);
        } else if self.inhibitor.is_none() {
            self.release_at = None;
            match self.system.as_ref().map(take_inhibitor) {
                Some(Ok(fd)) => self.inhibitor = Some(fd),
                _ => self.lost(),
            }
        }
        true
    }

    /// Lets sleep go on once the lock screen is up or the wait is over, and connects again when due
    pub fn tick(&mut self, marker: &std::path::Path, events: &Sender<Event>) {
        let now = Instant::now();
        if self.release_at.is_some_and(|at| now >= at || marker.exists()) {
            self.release_at = None;
            self.inhibitor = None;
        }
        if self.enabled && self.retry_at.is_some_and(|at| now >= at) {
            self.connect(events);
        }
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.release_at.map(|_| Instant::now() + MARKER_POLL).or(self.retry_at)
    }
}

/// Connects to the system bus and passes logind's PrepareForSleep on as events
fn listen(events: Sender<Event>) -> zbus::Result<zbus::blocking::Connection> {
    let system = zbus::blocking::Connection::system()?;
    let manager = zbus::blocking::Proxy::new(
        &system,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )?;
    let signals = manager.receive_signal("PrepareForSleep")?;
    std::thread::spawn(move || {
        for signal in signals {
            if let Ok(sleeping) = signal.body().deserialize::<bool>() {
                let _ = events.send(Event::PrepareForSleep(sleeping));
            }
        }
        let _ = events.send(Event::SystemLost);
    });
    Ok(system)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookies_skip_zero_and_ones_in_use() {
        let mut cookies = Cookies { next: u32::MAX, ..Cookies::default() };
        assert_eq!(cookies.add(":1.1"), u32::MAX);
        assert_eq!(cookies.add(":1.2"), 1);
        cookies.next = 1;
        assert_eq!(cookies.add(":1.1"), 2);
        assert!(cookies.remove(1));
        assert!(!cookies.remove(1));
        cookies.drop_owner(":1.1");
        assert!(cookies.is_empty());
    }

    #[test]
    fn inhibited_only_as_owner_and_while_held() {
        let now = Instant::now();
        let mut state = Inhibition::default();
        state.cookies.add(":1.5");
        assert!(!state.inhibited(now));
        state.owner = true;
        assert!(state.inhibited(now));
        state.reset();
        assert!(!state.inhibited(now));
        state.activity_until = Some(now + ACTIVITY_HOLD);
        assert!(state.inhibited(now));
        assert_eq!(state.deadline(now), Some(now + ACTIVITY_HOLD));
        assert!(!state.inhibited(now + ACTIVITY_HOLD));
        assert_eq!(state.deadline(now + ACTIVITY_HOLD), None);
    }

    #[test]
    fn sleep_is_passed_on_only_when_enabled() {
        let (events, _rx) = std::sync::mpsc::channel();
        let mut lock = SleepLock::default();
        assert!(!lock.prepare_for_sleep(true));
        lock.enabled = true;
        assert!(lock.prepare_for_sleep(true));
        assert!(lock.deadline().is_some());
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("locked");
        lock.tick(&marker, &events);
        assert!(lock.release_at.is_some());
        std::fs::write(&marker, "").unwrap();
        lock.tick(&marker, &events);
        assert!(lock.release_at.is_none());
    }
}
