//! Hyprland's focused window, for screen time: `activewindowv2` events on the event socket, and the
//! window asked for on the request socket. Workspaces and windows the shell reads natively.

use std::io::{self, Read, Write};
use std::os::fd::RawFd;
use std::path::PathBuf;

use serde_json::Value as Json;

use super::{Adapter, Lines, Session, State, missing, unix};

fn socket_dir(session: &Session) -> io::Result<PathBuf> {
    let signature = session.get("HYPRLAND_INSTANCE_SIGNATURE").ok_or_else(|| missing("HYPRLAND_INSTANCE_SIGNATURE"))?;
    let dirs = [session.runtime_dir().join("hypr").join(signature), PathBuf::from("/tmp/hypr").join(signature)];
    dirs.into_iter()
        .find(|dir| dir.join(".socket2.sock").exists())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no Hyprland socket"))
}

/// The reply to one request on the request socket
pub fn request(session: &Session, command: &str) -> io::Result<String> {
    let mut stream = unix(socket_dir(session)?.join(".socket.sock"))?;
    stream.write_all(command.as_bytes())?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply)?;
    Ok(reply)
}

/// The class and title of the window in a `j/activewindow` reply, preferring the initial ones
pub fn parse_active_window(reply: &str) -> Option<(String, String)> {
    let window: Json = serde_json::from_str(reply).ok()?;
    let text = |first: &str, then: &str| {
        [first, then]
            .iter()
            .find_map(|k| window.get(*k).and_then(Json::as_str).filter(|v| !v.is_empty()))
            .unwrap_or_default()
            .to_owned()
    };
    window.get("address").is_some().then(|| (text("initialClass", "class"), text("initialTitle", "title")))
}

pub struct Hyprland {
    dir: PathBuf,
    events: Lines,
    focus: Option<(String, String)>,
}

impl Hyprland {
    pub fn connect(session: &Session) -> io::Result<Hyprland> {
        let dir = socket_dir(session)?;
        let events = Lines::new(unix(dir.join(".socket2.sock"))?);
        events.stream.set_read_timeout(None)?;
        let mut hyprland = Hyprland { dir, events, focus: None };
        hyprland.focus = hyprland.active_window()?;
        Ok(hyprland)
    }

    fn active_window(&self) -> io::Result<Option<(String, String)>> {
        let mut stream = unix(self.dir.join(".socket.sock"))?;
        stream.write_all(b"j/activewindow")?;
        let mut reply = String::new();
        stream.read_to_string(&mut reply)?;
        Ok(parse_active_window(&reply))
    }
}

impl Adapter for Hyprland {
    fn fds(&self) -> Vec<RawFd> {
        vec![self.events.fd()]
    }

    fn read(&mut self, _: usize) -> io::Result<()> {
        let lines = self.events.read()?;
        if let Some(address) = lines.iter().rev().find_map(|l| l.strip_prefix("activewindowv2>>")) {
            self.focus = if address.trim().is_empty() { None } else { self.active_window()? };
        }
        Ok(())
    }

    fn focus(&self) -> Option<(String, String)> {
        self.focus.clone()
    }

    fn state(&self) -> State {
        State::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_initial_class_and_title_win() {
        let reply =
            r#"{"address":"0x1","class":"firefox","title":"Page","initialClass":"","initialTitle":"Mozilla Firefox"}"#;
        assert_eq!(parse_active_window(reply), Some(("firefox".into(), "Mozilla Firefox".into())));
        assert_eq!(parse_active_window("{}"), None);
        assert_eq!(parse_active_window("garbage"), None);
    }
}
