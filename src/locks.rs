//! Caps Lock and Num Lock. When the keyboards in /dev/input can be opened (the `input` group), their
//! LED events are followed and nothing runs in between. Otherwise the keyboard LEDs in
//! /sys/class/leds, which anyone may read but which send no change events, are polled while someone
//! watches. On Hyprland, every change is confirmed with Hyprland's own state of the main keyboard,
//! since it flickers the LEDs of other keyboards.

use std::ffi::CString;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const LEDS: &str = "/sys/class/leds";
const INPUT: &str = "/dev/input";
const EV_LED: u16 = 0x11;
const LED_NUML: u16 = 0;
const LED_CAPSL: u16 = 1;
const HYPRLAND_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Locks {
    pub caps: bool,
    pub num: bool,
}

fn led_dirs(root: &Path, pattern: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else { return Vec::new() };
    entries.flatten().filter(|e| e.file_name().to_string_lossy().contains(pattern)).map(|e| e.path()).collect()
}

fn lit(file: &File) -> bool {
    let mut buf = [0u8; 16];
    let n = file.read_at(&mut buf, 0).unwrap_or(0);
    std::str::from_utf8(&buf[..n]).ok().and_then(|t| t.trim().parse::<u64>().ok()).is_some_and(|v| v > 0)
}

/// The keyboard LED brightness files, kept open and re-read in place
pub struct LedFiles {
    caps: Vec<File>,
    num: Vec<File>,
}

impl LedFiles {
    pub fn open(root: &Path) -> LedFiles {
        let open =
            |pattern| led_dirs(root, pattern).iter().filter_map(|d| File::open(d.join("brightness")).ok()).collect();
        LedFiles { caps: open("capslock"), num: open("numlock") }
    }

    /// A lock is on when any keyboard has its LED lit
    pub fn read(&self) -> Locks {
        Locks { caps: self.caps.iter().any(lit), num: self.num.iter().any(lit) }
    }
}

pub fn read_leds(root: &Path) -> Locks {
    LedFiles::open(root).read()
}

/// The keyboard event devices, the way udev names them, else every event device
fn keyboard_paths(input: &Path) -> Vec<PathBuf> {
    let list = |dir: PathBuf, keep: &dyn Fn(&str) -> bool| -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| keep(&e.file_name().to_string_lossy()))
            .map(|e| e.path())
            .collect();
        paths.sort();
        paths
    };
    let kbd = |name: &str| name.ends_with("-event-kbd");
    let by_id = list(input.join("by-id"), &kbd);
    if !by_id.is_empty() {
        return by_id;
    }
    let by_path = list(input.join("by-path"), &kbd);
    if !by_path.is_empty() {
        return by_path;
    }
    list(input.to_path_buf(), &|name| name.starts_with("event"))
}

/// The keyboards this user may read, opened without blocking; empty without the permission
pub fn open_keyboards() -> Vec<File> {
    let mut seen = Vec::new();
    keyboard_paths(Path::new(INPUT))
        .into_iter()
        .filter(|p| {
            p.canonicalize().is_ok_and(|real| {
                !seen.contains(&real) && {
                    seen.push(real);
                    true
                }
            })
        })
        .filter_map(|p| std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(p).ok())
        .collect()
}

/// The Caps Lock and Num Lock changes in a buffer of `input_event`s
pub fn led_events(buf: &[u8]) -> Vec<(bool, bool)> {
    let size = std::mem::size_of::<libc::input_event>();
    buf.chunks_exact(size)
        .filter_map(|event| {
            let tail = &event[size - 8..];
            let kind = u16::from_ne_bytes([tail[0], tail[1]]);
            let code = u16::from_ne_bytes([tail[2], tail[3]]);
            let value = i32::from_ne_bytes([tail[4], tail[5], tail[6], tail[7]]);
            (kind == EV_LED && matches!(code, LED_CAPSL | LED_NUML)).then_some((code == LED_CAPSL, value > 0))
        })
        .collect()
}

/// Reads what a keyboard has to say. `None` when the device went away.
pub fn read_keyboard(mut keyboard: &File, locks: &mut Locks) -> Option<()> {
    let mut buf = [0u8; 64 * 24];
    loop {
        match keyboard.read(&mut buf) {
            Ok(0) => return None,
            Ok(n) => {
                for (caps, on) in led_events(&buf[..n]) {
                    if caps {
                        locks.caps = on;
                    } else {
                        locks.num = on;
                    }
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Some(()),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
}

/// Tells when an input device is added or its permissions change, so it can be opened
pub struct DeviceWatch(OwnedFd);

impl DeviceWatch {
    pub fn new() -> Option<DeviceWatch> {
        // SAFETY: inotify_init1 has no preconditions; a valid descriptor is owned from here on
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return None;
        }
        // SAFETY: fd is a fresh descriptor nothing else owns
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let mask = libc::IN_CREATE | libc::IN_ATTRIB | libc::IN_DELETE;
        let watched = [INPUT, "/dev/input/by-id", "/dev/input/by-path"].into_iter().filter(|dir| {
            let path = CString::new(*dir).unwrap();
            // SAFETY: path is a valid C string and fd an inotify descriptor
            unsafe { libc::inotify_add_watch(fd.as_raw_fd(), path.as_ptr(), mask) >= 0 }
        });
        (watched.count() > 0).then_some(DeviceWatch(fd))
    }

    pub fn fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }

    /// Reads every pending event; true if there was any
    pub fn drain(&self) -> bool {
        let mut buf = [0u8; 4096];
        let mut any = false;
        // SAFETY: buf is writable for its whole length
        while unsafe { libc::read(self.fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {
            any = true;
        }
        any
    }
}

/// Waits until one of `fds` is readable; returns their `revents`, or `None` if interrupted
pub fn wait_readable(fds: &[RawFd]) -> Option<Vec<i16>> {
    let mut polls: Vec<libc::pollfd> =
        fds.iter().map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
    // SAFETY: polls is a valid array of pollfd of the given length
    let n = unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, -1) };
    (n > 0).then(|| polls.iter().map(|p| p.revents).collect())
}

/// Caps Lock and Num Lock of Hyprland's main keyboard, from the reply to `j/devices`
pub fn parse_hyprland_devices(reply: &str) -> Option<Locks> {
    let devices: serde_json::Value = serde_json::from_str(reply).ok()?;
    let keyboards = devices.get("keyboards")?.as_array()?;
    let keyboard = keyboards.iter().find(|k| k.get("main") == Some(&true.into())).or(keyboards.first())?;
    Some(Locks { caps: keyboard.get("capsLock")?.as_bool()?, num: keyboard.get("numLock")?.as_bool()? })
}

/// Asks Hyprland over its socket, when running under it
pub fn hyprland() -> Option<Locks> {
    let signature = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").filter(|s| !s.is_empty())?;
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|| "/tmp".into());
    let sockets = [runtime.join("hypr").join(&signature), Path::new("/tmp/hypr").join(&signature)];
    let mut stream = sockets.iter().find_map(|dir| UnixStream::connect(dir.join(".socket.sock")).ok())?;
    stream.set_read_timeout(Some(HYPRLAND_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(HYPRLAND_TIMEOUT)).ok()?;
    std::io::Write::write_all(&mut stream, b"j/devices").ok()?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply).ok()?;
    parse_hyprland_devices(&reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn led(root: &Path, name: &str, value: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("brightness"), value).unwrap();
    }

    #[test]
    fn reads_the_led_that_matches_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        led(dir.path(), "input3::capslock", "1\n");
        led(dir.path(), "input3::numlock", "0\n");
        led(dir.path(), "input3::scrolllock", "1\n");
        assert_eq!(read_leds(dir.path()), Locks { caps: true, num: false });
    }

    #[test]
    fn open_led_files_see_later_changes() {
        let dir = tempfile::tempdir().unwrap();
        led(dir.path(), "input3::numlock", "0\n");
        led(dir.path(), "input9::numlock", "0\n");
        let leds = LedFiles::open(dir.path());
        assert!(!leds.read().num);
        std::fs::write(dir.path().join("input9::numlock/brightness"), "1\n").unwrap();
        assert!(leds.read().num);
    }

    #[test]
    fn missing_root_means_off() {
        assert_eq!(read_leds(Path::new("/nonexistent-serpantinum-leds")), Locks::default());
    }

    fn event(kind: u16, code: u16, value: i32) -> Vec<u8> {
        let size = std::mem::size_of::<libc::input_event>();
        let mut bytes = vec![0u8; size - 8];
        bytes.extend(kind.to_ne_bytes());
        bytes.extend(code.to_ne_bytes());
        bytes.extend(value.to_ne_bytes());
        bytes
    }

    #[test]
    fn only_caps_and_num_led_events_count() {
        let mut buf = event(EV_LED, LED_CAPSL, 1);
        buf.extend(event(1, 58, 1));
        buf.extend(event(EV_LED, 2, 1));
        buf.extend(event(EV_LED, LED_NUML, 0));
        assert_eq!(led_events(&buf), [(true, true), (false, false)]);
    }

    #[test]
    fn a_keyboard_is_read_until_it_would_block_and_dropped_when_gone() {
        let mut fds = [0; 2];
        // SAFETY: fds has room for the two descriptors pipe2 writes
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) }, 0);
        // SAFETY: both descriptors are fresh and owned here
        let (reader, mut writer) = unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) };
        let mut events = event(EV_LED, LED_CAPSL, 1);
        events.extend(event(EV_LED, LED_NUML, 1));
        std::io::Write::write_all(&mut writer, &events).unwrap();
        let mut locks = Locks::default();
        assert_eq!(read_keyboard(&reader, &mut locks), Some(()));
        assert_eq!(locks, Locks { caps: true, num: true });
        drop(writer);
        assert_eq!(read_keyboard(&reader, &mut locks), None);
    }

    #[test]
    fn keyboards_prefer_udev_names() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("by-id")).unwrap();
        std::fs::create_dir_all(dir.path().join("by-path")).unwrap();
        for name in ["event0", "event1", "by-path/pci-kbd-event-kbd", "by-id/usb-mouse-event-mouse"] {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        assert_eq!(keyboard_paths(dir.path()), [dir.path().join("by-path/pci-kbd-event-kbd")]);
        std::fs::remove_file(dir.path().join("by-path/pci-kbd-event-kbd")).unwrap();
        assert_eq!(keyboard_paths(dir.path()), [dir.path().join("event0"), dir.path().join("event1")]);
    }

    #[test]
    fn hyprland_reports_the_main_keyboard() {
        let reply = r#"{"mice":[],"keyboards":[
            {"name":"virtual","capsLock":true,"numLock":true,"main":false},
            {"name":"at-kbd","capsLock":false,"numLock":true,"main":true}]}"#;
        assert_eq!(parse_hyprland_devices(reply), Some(Locks { caps: false, num: true }));
        let no_main = r#"{"keyboards":[{"capsLock":true,"numLock":false}]}"#;
        assert_eq!(parse_hyprland_devices(no_main), Some(Locks { caps: true, num: false }));
        assert_eq!(parse_hyprland_devices("unknown request"), None);
    }
}
