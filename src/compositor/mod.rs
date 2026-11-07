//! Workspaces, windows and the keyboard layout of the running compositor, followed over its own
//! socket while someone watches. Sway, Niri and Mango are served; of Hyprland, which the shell reads
//! natively, only the focused window is followed, for screen time.
//!
//! The daemon may outlive a login, so it does not trust its own environment: every watcher passes
//! the variables that locate its compositor, and the latest one wins.

pub mod hyprland;
pub mod mango;
pub mod niri;
pub mod sway;

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Map, Value as Json};

pub use self::sway::Sway;

/// The variables that tell which compositor runs and where its socket is
pub const SESSION_VARS: &[&str] = &[
    "NIRI_SOCKET",
    "MANGO_INSTANCE_SIGNATURE",
    "SWAYSOCK",
    "HYPRLAND_INSTANCE_SIGNATURE",
    "XDG_CURRENT_DESKTOP",
    "DESKTOP_SESSION",
    "XDG_RUNTIME_DIR",
];
const REPLY_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Session(BTreeMap<String, String>);

impl Session {
    pub fn new(vars: impl IntoIterator<Item = (String, String)>) -> Session {
        Session(vars.into_iter().filter(|(k, v)| SESSION_VARS.contains(&k.as_str()) && !v.is_empty()).collect())
    }

    pub fn from_env() -> Session {
        Session::new(SESSION_VARS.iter().filter_map(|k| Some((k.to_string(), std::env::var(k).ok()?))))
    }

    pub fn vars(&self) -> &BTreeMap<String, String> {
        &self.0
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    /// The compositor, detected in the order the shell uses
    pub fn compositor(&self) -> &'static str {
        let desktop =
            self.get("XDG_CURRENT_DESKTOP").or(self.get("DESKTOP_SESSION")).unwrap_or_default().to_lowercase();
        let found = |var: &str, name: &str| self.get(var).is_some() || desktop.contains(name);
        if found("NIRI_SOCKET", "niri") {
            "niri"
        } else if found("MANGO_INSTANCE_SIGNATURE", "mango") {
            "mango"
        } else if found("SWAYSOCK", "sway") {
            "sway"
        } else if found("HYPRLAND_INSTANCE_SIGNATURE", "hyprland") {
            "hyprland"
        } else {
            "unknown"
        }
    }

    fn runtime_dir(&self) -> PathBuf {
        self.get("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|| format!("/run/user/{}", uid()).into())
    }
}

fn uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail
    unsafe { libc::getuid() }
}

/// What the shell shows of the compositor, each part as JSON in the shape the shell's singleton
/// for that compositor keeps
#[derive(Debug, Clone, PartialEq)]
pub struct State {
    pub workspaces: Json,
    pub windows: Json,
    pub keyboard: Json,
}

impl Default for State {
    fn default() -> State {
        State { workspaces: Json::Object(Map::new()), windows: Json::Null, keyboard: Json::Null }
    }
}

/// A connection to one compositor that follows its events
pub trait Adapter: Send {
    /// The descriptors to wait on
    fn fds(&self) -> Vec<RawFd>;
    /// Takes what the descriptor at `index` has to say. An error ends the connection.
    fn read(&mut self, index: usize) -> io::Result<()>;
    fn state(&self) -> State;
    /// The class and title of the focused window, `None` when no window has focus
    fn focus(&self) -> Option<(String, String)>;
}

/// Connects to the compositor of `session`. `None` when it is not one that is served here.
pub fn connect(session: &Session) -> Option<io::Result<Box<dyn Adapter>>> {
    let adapter = match session.compositor() {
        "sway" => Sway::connect(session).map(|a| Box::new(a) as Box<dyn Adapter>),
        "niri" => niri::Niri::connect(session).map(|a| Box::new(a) as Box<dyn Adapter>),
        "mango" => mango::Mango::connect(session).map(|a| Box::new(a) as Box<dyn Adapter>),
        "hyprland" => hyprland::Hyprland::connect(session).map(|a| Box::new(a) as Box<dyn Adapter>),
        _ => return None,
    };
    Some(adapter)
}

fn missing(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, format!("{what} is not set"))
}

fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "the compositor closed the connection")
}

fn unix(path: impl AsRef<std::path::Path>) -> io::Result<UnixStream> {
    let stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(REPLY_TIMEOUT))?;
    stream.set_write_timeout(Some(REPLY_TIMEOUT))?;
    Ok(stream)
}

/// A stream of newline-separated messages, read as it becomes readable
pub struct Lines {
    pub stream: UnixStream,
    pending: Vec<u8>,
}

impl Lines {
    pub fn new(stream: UnixStream) -> Lines {
        Lines { stream, pending: Vec::new() }
    }

    pub fn fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    /// One read, and the complete lines it finished
    pub fn read(&mut self) -> io::Result<Vec<String>> {
        let mut buf = [0u8; 16384];
        let n = loop {
            match self.stream.read(&mut buf) {
                Ok(0) => return Err(closed()),
                Ok(n) => break n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        };
        self.pending.extend_from_slice(&buf[..n]);
        Ok(split_lines(&mut self.pending))
    }

    /// Reads until a whole line arrives
    pub fn read_line(&mut self) -> io::Result<String> {
        loop {
            if let Some(line) = split_lines(&mut self.pending).into_iter().next() {
                return Ok(line);
            }
            let mut buf = [0u8; 16384];
            match self.stream.read(&mut buf)? {
                0 => return Err(closed()),
                n => self.pending.extend_from_slice(&buf[..n]),
            }
        }
    }
}

/// Takes the complete lines off the front of `pending`, skipping blank ones
fn split_lines(pending: &mut Vec<u8>) -> Vec<String> {
    let Some(end) = pending.iter().rposition(|&b| b == b'\n') else { return Vec::new() };
    let rest = pending.split_off(end + 1);
    let done = std::mem::replace(pending, rest);
    String::from_utf8_lossy(&done).lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect()
}

/// `{"3": true}` for the indices given, the way the shell keys its maps
fn index_map(indices: impl IntoIterator<Item = i64>) -> Json {
    Json::Object(indices.into_iter().map(|i| (i.to_string(), Json::Bool(true))).collect())
}

/// A descriptor that wakes a waiting feed thread
pub struct Waker(OwnedFd);

impl Waker {
    pub fn new() -> Waker {
        // SAFETY: eventfd has no preconditions; a valid descriptor is owned from here on
        let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        assert!(fd >= 0, "cannot create an eventfd: {}", io::Error::last_os_error());
        // SAFETY: fd is a fresh descriptor nothing else owns
        Waker(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    pub fn fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }

    pub fn wake(&self) {
        let one = 1u64.to_ne_bytes();
        // SAFETY: the buffer holds the 8 bytes an eventfd write takes
        unsafe { libc::write(self.fd(), one.as_ptr().cast(), one.len()) };
    }

    pub fn drain(&self) {
        let mut buf = [0u8; 8];
        // SAFETY: buf is writable for its whole length
        unsafe { libc::read(self.fd(), buf.as_mut_ptr().cast(), buf.len()) };
    }
}

impl Default for Waker {
    fn default() -> Waker {
        Waker::new()
    }
}

/// `parseInt` as JavaScript does it: leading digits after optional blanks and a sign
fn parse_int(text: &str) -> Option<i64> {
    let text = text.trim_start();
    let (sign, digits) = match text.as_bytes().first() {
        Some(b'-') => (-1, &text[1..]),
        Some(b'+') => (1, &text[1..]),
        _ => (1, text),
    };
    let end = digits.find(|c: char| !c.is_ascii_digit()).unwrap_or(digits.len());
    digits[..end].parse::<i64>().ok().map(|n| sign * n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_follows_the_shell_order() {
        let session = |vars: &[(&str, &str)]| Session::new(vars.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        assert_eq!(session(&[("SWAYSOCK", "/s"), ("NIRI_SOCKET", "/n")]).compositor(), "niri");
        assert_eq!(session(&[("XDG_CURRENT_DESKTOP", "sway")]).compositor(), "sway");
        assert_eq!(session(&[("DESKTOP_SESSION", "Mango")]).compositor(), "mango");
        assert_eq!(session(&[("HYPRLAND_INSTANCE_SIGNATURE", "x")]).compositor(), "hyprland");
        assert_eq!(session(&[("SWAYSOCK", "")]).compositor(), "unknown");
        assert_eq!(session(&[("HOME", "/root")]).vars().len(), 0);
    }

    #[test]
    fn lines_are_split_when_complete() {
        let mut pending = b"one\n\ntwo\nthr".to_vec();
        assert_eq!(split_lines(&mut pending), ["one", "two"]);
        assert_eq!(pending, b"thr");
        assert!(split_lines(&mut pending).is_empty());
    }

    #[test]
    fn parse_int_reads_leading_digits() {
        assert_eq!(parse_int("3: www"), Some(3));
        assert_eq!(parse_int(" -2x"), Some(-2));
        assert_eq!(parse_int("mail"), None);
        assert_eq!(parse_int(""), None);
    }
}
