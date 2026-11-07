//! Sway over its IPC socket: one connection for events and one for requests, so the tree is
//! fetched in the daemon instead of by a `swaymsg` per event.

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use serde_json::{Map, Value as Json, json};

use super::{Adapter, Session, State, closed, index_map, missing, parse_int, uid, unix};

const MAGIC: &[u8; 6] = b"i3-ipc";
const HEADER: usize = 14;
const GET_WORKSPACES: u32 = 1;
const SUBSCRIBE: u32 = 2;
pub const GET_OUTPUTS: u32 = 3;
const GET_TREE: u32 = 4;
const GET_INPUTS: u32 = 100;
const EVENT: u32 = 0x8000_0000;
const EVENT_WORKSPACE: u32 = EVENT;
const EVENT_OUTPUT: u32 = EVENT | 1;
const EVENT_WINDOW: u32 = EVENT | 3;
const EVENT_INPUT: u32 = EVENT | 0x15;

pub fn frame(kind: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + payload.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
    out.extend_from_slice(&kind.to_ne_bytes());
    out.extend_from_slice(payload);
    out
}

/// Takes the complete messages off the front of `pending`
fn split_frames(pending: &mut Vec<u8>) -> io::Result<Vec<(u32, Vec<u8>)>> {
    let mut frames = Vec::new();
    let mut at = 0;
    while pending.len() - at >= HEADER {
        if &pending[at..at + 6] != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "not an i3-ipc message"));
        }
        let length = u32::from_ne_bytes(pending[at + 6..at + 10].try_into().unwrap()) as usize;
        let kind = u32::from_ne_bytes(pending[at + 10..at + 14].try_into().unwrap());
        if pending.len() - at - HEADER < length {
            break;
        }
        frames.push((kind, pending[at + HEADER..at + HEADER + length].to_vec()));
        at += HEADER + length;
    }
    pending.drain(..at);
    Ok(frames)
}

/// The socket of the session's Sway, or the one Sway names after this user in the runtime dir
pub fn socket(session: &Session) -> io::Result<PathBuf> {
    if let Some(path) = session.get("SWAYSOCK") {
        return Ok(path.into());
    }
    let prefix = format!("sway-ipc.{}.", uid());
    std::fs::read_dir(session.runtime_dir())?
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".sock"))
        })
        .ok_or_else(|| missing("SWAYSOCK"))
}

/// Sends one request and waits for its reply
pub fn request(stream: &mut UnixStream, kind: u32, payload: &[u8]) -> io::Result<Json> {
    stream.write_all(&frame(kind, payload))?;
    let mut pending = Vec::new();
    loop {
        if let Some((_, body)) = split_frames(&mut pending)?.into_iter().find(|(k, _)| *k == kind) {
            return serde_json::from_slice(&body).map_err(io::Error::other);
        }
        let mut buf = [0u8; 65536];
        match stream.read(&mut buf)? {
            0 => return Err(closed()),
            n => pending.extend_from_slice(&buf[..n]),
        }
    }
}

/// Asks the session's Sway once, on a connection of its own
pub fn ask(session: &Session, kind: u32) -> io::Result<Json> {
    request(&mut unix(socket(session)?)?, kind, b"")
}

/// A workspace's position: its number less one, else the given fallback
fn workspace_index(ws: &Json, fallback: i64) -> i64 {
    let num = ws
        .get("num")
        .and_then(Json::as_i64)
        .filter(|n| *n > 0)
        .or_else(|| ws.get("name").and_then(Json::as_str).and_then(parse_int));
    num.filter(|n| *n > 0).map_or(fallback, |n| n - 1)
}

/// The focused workspace's index and the visible one's on every output
pub fn active_workspaces(list: &[Json]) -> (i64, Map<String, Json>) {
    let mut active = 0;
    let mut by_output = Map::new();
    for (i, ws) in list.iter().enumerate() {
        let index = workspace_index(ws, i as i64);
        if ws.get("focused") == Some(&Json::Bool(true)) {
            active = index;
        }
        if ws.get("visible") == Some(&Json::Bool(true))
            && let Some(output) = ws.get("output").and_then(Json::as_str).filter(|o| !o.is_empty())
        {
            by_output.insert(output.to_owned(), index.into());
        }
    }
    (active, by_output)
}

fn app_id(node: &Json) -> &str {
    node.get("app_id")
        .and_then(Json::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| node.pointer("/window_properties/class").and_then(Json::as_str))
        .unwrap_or_default()
}

/// The windows in tree order, and the indices of workspaces that hold any
pub fn collect_tree(tree: &Json) -> (Vec<Json>, Vec<i64>) {
    fn walk<'a>(
        node: &'a Json,
        mut output: &'a str,
        mut index: i64,
        mut label: &'a str,
        out: &mut Vec<Json>,
        occ: &mut Vec<i64>,
    ) {
        let kind = node.get("type").and_then(Json::as_str).unwrap_or_default();
        let name = node.get("name").and_then(Json::as_str).unwrap_or_default();
        let children = |key| node.get(key).and_then(Json::as_array).map(Vec::as_slice).unwrap_or_default();
        if kind == "output" && !name.is_empty() {
            output = name;
        } else if kind == "workspace" {
            label = name;
            index = workspace_index(node, -1);
            if index >= 0 && children("nodes").len() + children("floating_nodes").len() > 0 && !occ.contains(&index) {
                occ.push(index);
            }
        }
        let app = app_id(node);
        if matches!(kind, "con" | "floating_con") && !app.is_empty() {
            out.push(json!({
                "id": node.get("id").cloned().unwrap_or(Json::Null),
                "appId": app,
                "title": name,
                "workspace": index,
                "workspaceLabel": label,
                "output": output,
                "focused": node.get("focused") == Some(&Json::Bool(true)),
            }));
        }
        for child in children("nodes").iter().chain(children("floating_nodes")) {
            walk(child, output, index, label, out, occ);
        }
    }
    let (mut windows, mut occupied) = (Vec::new(), Vec::new());
    walk(tree, "", -1, "", &mut windows, &mut occupied);
    (windows, occupied)
}

/// The first keyboard's layouts and the active one, or US when no keyboard names one
pub fn keyboard(inputs: &[Json]) -> Json {
    let found = inputs.iter().find(|d| {
        d.get("type").and_then(Json::as_str) == Some("keyboard")
            && d.get("xkb_active_layout_name").is_some_and(|n| !n.is_null())
    });
    let name = found
        .and_then(|d| d.get("xkb_active_layout_name"))
        .map(|n| n.as_str().map_or_else(|| n.to_string(), str::to_owned))
        .filter(|n| !n.is_empty() && n != "null")
        .unwrap_or_else(|| "US".to_owned());
    let layouts = found.and_then(|d| d.get("xkb_layout_names")).cloned().unwrap_or_else(|| json!([]));
    let index = found.and_then(|d| d.get("xkb_active_layout_index")).cloned().unwrap_or_else(|| json!(0));
    json!({ "layout": name, "layouts": layouts, "index": index })
}

#[derive(Default)]
struct Wanted {
    workspaces: bool,
    tree: bool,
    inputs: bool,
}

/// What an event asks to be fetched again, as the shell's SwayState did
fn wanted(kind: u32, body: &[u8], wanted: &mut Wanted) {
    let change = serde_json::from_slice::<Json>(body)
        .ok()
        .and_then(|e| e.get("change").and_then(Json::as_str).map(str::to_owned))
        .unwrap_or_default();
    match kind {
        EVENT_WORKSPACE if matches!(change.as_str(), "focus" | "move" | "rename" | "reload") => {
            wanted.workspaces = true;
            wanted.tree = true;
        }
        EVENT_OUTPUT => {
            wanted.workspaces = true;
            wanted.tree = true;
        }
        EVENT_WINDOW if matches!(change.as_str(), "new" | "close" | "move" | "title" | "focus") => wanted.tree = true,
        EVENT_INPUT if matches!(change.as_str(), "xkb_layout" | "xkb_keymap" | "added" | "removed") => {
            wanted.inputs = true
        }
        _ => {}
    }
}

pub struct Sway {
    events: UnixStream,
    requests: UnixStream,
    pending: Vec<u8>,
    active: i64,
    by_output: Map<String, Json>,
    windows: Vec<Json>,
    occupied: Vec<i64>,
    keyboard: Json,
}

impl Sway {
    pub fn connect(session: &Session) -> io::Result<Sway> {
        let path = socket(session)?;
        let mut events = unix(&path)?;
        let requests = unix(&path)?;
        let reply = request(&mut events, SUBSCRIBE, br#"["workspace","window","output","input"]"#)?;
        if reply.get("success") != Some(&Json::Bool(true)) {
            return Err(io::Error::other("sway refused the subscription"));
        }
        events.set_read_timeout(None)?;
        let mut sway = Sway {
            events,
            requests,
            pending: Vec::new(),
            active: 0,
            by_output: Map::new(),
            windows: Vec::new(),
            occupied: Vec::new(),
            keyboard: Json::Null,
        };
        sway.fetch(Wanted { workspaces: true, tree: true, inputs: true })?;
        Ok(sway)
    }

    fn fetch(&mut self, wanted: Wanted) -> io::Result<()> {
        let list = |reply: Json| reply.as_array().cloned().unwrap_or_default();
        if wanted.workspaces {
            (self.active, self.by_output) = active_workspaces(&list(request(&mut self.requests, GET_WORKSPACES, b"")?));
        }
        if wanted.tree {
            (self.windows, self.occupied) = collect_tree(&request(&mut self.requests, GET_TREE, b"")?);
        }
        if wanted.inputs {
            self.keyboard = keyboard(&list(request(&mut self.requests, GET_INPUTS, b"")?));
        }
        Ok(())
    }
}

impl Adapter for Sway {
    fn fds(&self) -> Vec<RawFd> {
        vec![self.events.as_raw_fd()]
    }

    fn read(&mut self, _: usize) -> io::Result<()> {
        let mut buf = [0u8; 65536];
        let n = loop {
            match self.events.read(&mut buf) {
                Ok(0) => return Err(closed()),
                Ok(n) => break n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        };
        self.pending.extend_from_slice(&buf[..n]);
        let mut want = Wanted::default();
        for (kind, body) in split_frames(&mut self.pending)? {
            wanted(kind, &body, &mut want);
        }
        self.fetch(want)
    }

    fn focus(&self) -> Option<(String, String)> {
        let w = self.windows.iter().find(|w| w["focused"] == true)?;
        Some((w["appId"].as_str()?.to_owned(), w["title"].as_str().unwrap_or_default().to_owned()))
    }

    fn state(&self) -> State {
        State {
            workspaces: json!({
                "activeIndex": self.active,
                "activeIndicesByOutput": self.by_output,
                "occupiedMap": index_map(self.occupied.iter().copied()),
            }),
            windows: Json::Array(self.windows.clone()),
            keyboard: self.keyboard.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_split_and_partial_ones_kept() {
        let mut pending = frame(EVENT_WINDOW, br#"{"change":"new"}"#);
        pending.extend(frame(GET_TREE, b"{}"));
        pending.extend(&frame(EVENT_INPUT, b"{}")[..10]);
        let frames = split_frames(&mut pending).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0], (EVENT_WINDOW, br#"{"change":"new"}"#.to_vec()));
        assert_eq!(pending.len(), 10);
        assert!(split_frames(&mut b"nonsense-bytes!".to_vec()).is_err());
    }

    #[test]
    fn workspaces_are_numbered_like_the_shell() {
        assert_eq!(workspace_index(&json!({"num": 3, "name": "3"}), 9), 2);
        assert_eq!(workspace_index(&json!({"num": -1, "name": "4: web"}), 9), 3);
        assert_eq!(workspace_index(&json!({"num": -1, "name": "mail"}), 9), 9);
        let list = [
            json!({"num": 1, "name": "1", "output": "DP-1", "visible": true, "focused": false}),
            json!({"num": -1, "name": "mail", "output": "HDMI-A-1", "visible": true, "focused": true}),
            json!({"num": 5, "name": "5", "output": "DP-1", "visible": false, "focused": false}),
        ];
        let (active, by_output) = active_workspaces(&list);
        assert_eq!(active, 1);
        assert_eq!(Json::Object(by_output), json!({"DP-1": 0, "HDMI-A-1": 1}));
    }

    #[test]
    fn the_tree_gives_windows_and_occupied_workspaces() {
        let tree = json!({"type": "root", "nodes": [{"type": "output", "name": "__i3", "nodes": []},
            {"type": "output", "name": "DP-1", "nodes": [
                {"type": "workspace", "name": "2", "num": 2, "nodes": [
                    {"type": "con", "id": 7, "name": "Term", "app_id": "foot", "focused": true, "nodes": []},
                    {"type": "con", "id": 8, "name": "split", "nodes": [
                        {"type": "con", "id": 9, "name": "X", "app_id": null,
                         "window_properties": {"class": "XTerm"}, "nodes": []}]}],
                 "floating_nodes": [{"type": "floating_con", "id": 10, "name": "Pop", "app_id": "pop"}]},
                {"type": "workspace", "name": "3", "num": 3, "nodes": [], "floating_nodes": []}]}]});
        let (windows, occupied) = collect_tree(&tree);
        assert_eq!(occupied, [1]);
        let ids: Vec<i64> = windows.iter().map(|w| w["id"].as_i64().unwrap()).collect();
        assert_eq!(ids, [7, 9, 10]);
        assert_eq!(
            windows[0],
            json!({"id": 7, "appId": "foot", "title": "Term", "workspace": 1, "workspaceLabel": "2",
                   "output": "DP-1", "focused": true})
        );
        assert_eq!(windows[1]["appId"], "XTerm");
    }

    #[test]
    fn the_first_keyboard_with_a_layout_names_it() {
        let inputs = [
            json!({"type": "pointer"}),
            json!({"type": "keyboard", "xkb_active_layout_name": null}),
            json!({"type": "keyboard", "xkb_active_layout_name": "Russian",
                   "xkb_layout_names": ["English (US)", "Russian"], "xkb_active_layout_index": 1}),
        ];
        assert_eq!(keyboard(&inputs), json!({"layout": "Russian", "layouts": ["English (US)", "Russian"], "index": 1}));
        assert_eq!(keyboard(&[]), json!({"layout": "US", "layouts": [], "index": 0}));
    }

    #[test]
    fn events_ask_for_what_they_change() {
        let mut want = Wanted::default();
        wanted(EVENT_WINDOW, br#"{"change":"title"}"#, &mut want);
        assert!(want.tree && !want.workspaces && !want.inputs);
        let mut want = Wanted::default();
        wanted(EVENT_WORKSPACE, br#"{"change":"init"}"#, &mut want);
        wanted(EVENT_WINDOW, br#"{"change":"urgent"}"#, &mut want);
        assert!(!want.tree && !want.workspaces);
        wanted(EVENT_INPUT, br#"{"change":"xkb_layout"}"#, &mut want);
        assert!(want.inputs);
    }
}
