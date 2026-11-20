//! Umbriel over its IPC socket: one subscribed connection, which starts with the current state of
//! each family and then sends the whole family again whenever it changes.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::os::fd::RawFd;

use serde_json::{Map, Value as Json, json};

use super::{Adapter, Lines, Session, State, index_map, missing, unix};

const SUBSCRIBE: &str = r#"{"cmd":"subscribe","events":["workspaces","windows","keyboard_layout"]}"#;

fn items(list: &Json) -> impl Iterator<Item = &Json> {
    list.as_array().into_iter().flatten()
}

fn text<'a>(item: &'a Json, key: &str) -> &'a str {
    item.get(key).and_then(Json::as_str).unwrap_or_default()
}

fn is_true(item: &Json, key: &str) -> bool {
    item.get(key) == Some(&Json::Bool(true))
}

type Placed<'a> = (&'a str, i64, &'a Json);

/// Each workspace with its output and its 0-based index there, and how many each output has
fn place(list: &Json) -> (Vec<Placed<'_>>, BTreeMap<String, i64>) {
    let mut counts: BTreeMap<String, i64> = BTreeMap::new();
    let placed = items(list)
        .map(|w| {
            let output = text(w, "output");
            let position = counts.entry(output.to_owned()).or_insert(0);
            *position += 1;
            (output, w.get("index").and_then(Json::as_i64).unwrap_or(*position) - 1, w)
        })
        .collect();
    (placed, counts)
}

/// The workspaces of every output in the shape the shell keeps for a compositor with one set of
/// workspaces per output
pub fn workspaces(list: &Json) -> Json {
    let (placed, counts) = place(list);
    let mut active = Map::new();
    for (output, index, w) in &placed {
        if is_true(w, "active") && !active.contains_key(*output) {
            active.insert((*output).to_owned(), (*index).into());
        }
    }
    let occupied_on = |output: Option<&str>| {
        index_map(
            placed.iter().filter(|(o, _, w)| is_true(w, "occupied") && output.is_none_or(|n| n == *o)).map(|p| p.1),
        )
    };
    let occupied: Map<String, Json> = counts.keys().map(|o| (o.clone(), occupied_on(Some(o)))).collect();
    let current =
        placed.iter().find(|p| is_true(p.2, "focused")).or_else(|| placed.iter().find(|p| is_true(p.2, "active")));
    json!({
        "activeIndex": current.map_or(0, |p| p.1.max(0)),
        "activeIndicesByOutput": active,
        "occupiedByOutput": occupied,
        "occupiedMap": occupied_on(None),
        "countByOutput": counts,
        "workspaceCount": placed.iter().map(|p| p.1 + 1).max().unwrap_or(0).max(0),
        "focusedOutput": placed.iter().find(|p| is_true(p.2, "focused")).map_or("", |p| p.0),
    })
}

/// The windows as sent, each with the output of its workspace when Umbriel does not tell it, and
/// the 0-based index of its workspace on that output as `workspace_index`, -1 when unknown
pub fn windows(list: &Json, workspaces: &Json) -> Json {
    let outputs: BTreeMap<&str, (&str, i64)> =
        place(workspaces).0.into_iter().map(|(output, index, w)| (text(w, "id"), (output, index))).collect();
    Json::Array(
        items(list)
            .map(|w| {
                let mut window = w.clone();
                let home = outputs.get(text(w, "workspace")).copied();
                if let Some(map) = window.as_object_mut() {
                    if text(w, "output").is_empty() {
                        map.insert("output".to_owned(), home.map_or("", |h| h.0).into());
                    }
                    map.insert("workspace_index".to_owned(), home.map_or(-1, |h| h.1).into());
                }
                window
            })
            .collect(),
    )
}

/// The keyboard layout the shell shows
pub fn keyboard(payload: &Json) -> Json {
    let names = payload.get("names").filter(|n| n.is_array()).cloned().unwrap_or_else(|| json!([]));
    let index = payload.get("current_index").and_then(Json::as_i64).unwrap_or(0);
    let layout = usize::try_from(index).ok().and_then(|i| names.get(i)).and_then(Json::as_str).unwrap_or_default();
    json!({ "layout": layout, "layouts": names, "index": index })
}

/// What Umbriel answers to one command, inside its `ok`
pub fn request(session: &Session, command: &Json) -> io::Result<Json> {
    let path = session.get("UMBRIEL_SOCKET").ok_or_else(|| missing("UMBRIEL_SOCKET"))?;
    let mut lines = Lines::new(unix(path)?);
    lines.stream.write_all(format!("{command}\n").as_bytes())?;
    let mut reply: Json = serde_json::from_str(&lines.read_line()?).map_err(io::Error::other)?;
    if let Some(ok) = reply.get_mut("ok") {
        return Ok(ok.take());
    }
    Err(io::Error::other(reply.get("err").and_then(Json::as_str).map_or_else(|| reply.to_string(), str::to_owned)))
}

pub struct Umbriel {
    stream: Lines,
    raw_workspaces: Json,
    raw_windows: Json,
    state: State,
}

impl Umbriel {
    pub fn connect(session: &Session) -> io::Result<Umbriel> {
        let path = session.get("UMBRIEL_SOCKET").ok_or_else(|| missing("UMBRIEL_SOCKET"))?;
        let mut stream = Lines::new(unix(path)?);
        stream.stream.write_all(format!("{SUBSCRIBE}\n").as_bytes())?;
        stream.stream.set_read_timeout(None)?;
        Ok(Umbriel::new(stream))
    }

    fn new(stream: Lines) -> Umbriel {
        let state = State { windows: json!([]), ..State::default() };
        Umbriel { stream, raw_workspaces: Json::Null, raw_windows: Json::Null, state }
    }

    fn take(&mut self, event: &Json) {
        let data = event.get("data").cloned().unwrap_or(Json::Null);
        match text(event, "event") {
            "workspaces" => {
                self.state.workspaces = workspaces(&data);
                self.raw_workspaces = data;
                self.state.windows = windows(&self.raw_windows, &self.raw_workspaces);
            }
            "windows" => {
                self.state.windows = windows(&data, &self.raw_workspaces);
                self.raw_windows = data;
            }
            "keyboard_layout" => self.state.keyboard = keyboard(&data),
            _ => {}
        }
    }
}

impl Adapter for Umbriel {
    fn fds(&self) -> Vec<RawFd> {
        vec![self.stream.fd()]
    }

    fn read(&mut self, _: usize) -> io::Result<()> {
        for line in self.stream.read()? {
            if let Ok(event) = serde_json::from_str::<Json>(&line) {
                self.take(&event);
            }
        }
        Ok(())
    }

    fn focus(&self) -> Option<(String, String)> {
        let w = items(&self.state.windows).find(|w| is_true(w, "active") && !text(w, "id").is_empty())?;
        Some((text(w, "app_id").to_owned(), text(w, "title").to_owned()))
    }

    fn state(&self) -> State {
        self.state.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream;

    use super::*;

    fn two_outputs() -> Json {
        json!([
            {"active": true, "focused": true, "id": "HEADLESS-2:1", "index": 1, "layout": "scrolling", "name": "1", "named": false, "occupied": true, "output": "HEADLESS-2"},
            {"active": false, "focused": false, "id": "HEADLESS-2:2", "index": 2, "layout": "scrolling", "name": "2", "named": false, "occupied": false, "output": "HEADLESS-2"},
            {"active": true, "focused": false, "id": "HEADLESS-1:1", "index": 1, "layout": "scrolling", "name": "1", "named": false, "occupied": false, "output": "HEADLESS-1"}])
    }

    fn old_windows() -> Json {
        json!([
            {"active": true, "app_id": "kitty", "content_type": "none", "floating": false, "focused": true, "h": 700, "id": "c80bcdc6188bdfb3fffb276ca4304582", "pid": 102681, "scratchpad": "", "title": "user@host:~", "workspace": "HEADLESS-2:1", "w": 640, "x": 630, "y": 10},
            {"active": false, "app_id": "kitty", "focused": false, "id": "16491fb940e91656cac26331a5820bf8", "workspace": "HEADLESS-2:1", "title": "user@host:~"}])
    }

    fn umbriel() -> Umbriel {
        Umbriel::new(Lines::new(UnixStream::pair().unwrap().0))
    }

    fn event(family: &str, data: &Json) -> Json {
        json!({"event": family, "data": data})
    }

    #[test]
    fn workspaces_are_mapped_per_output() {
        assert_eq!(
            workspaces(&two_outputs()),
            json!({
                "activeIndex": 0,
                "activeIndicesByOutput": {"HEADLESS-1": 0, "HEADLESS-2": 0},
                "occupiedByOutput": {"HEADLESS-1": {}, "HEADLESS-2": {"0": true}},
                "occupiedMap": {"0": true},
                "countByOutput": {"HEADLESS-1": 1, "HEADLESS-2": 2},
                "workspaceCount": 2,
                "focusedOutput": "HEADLESS-2",
            })
        );
    }

    #[test]
    fn the_focused_workspace_wins_over_the_active_ones() {
        let list = json!([
            {"id": "a:1", "index": 1, "output": "DP-1", "active": true, "focused": false, "occupied": true},
            {"id": "a:2", "index": 2, "output": "DP-1", "active": false, "focused": false, "occupied": false},
            {"id": "b:1", "index": 1, "output": "DP-2", "active": false, "focused": false, "occupied": false},
            {"id": "b:3", "index": 3, "output": "DP-2", "active": true, "focused": true, "occupied": true}]);
        let w = workspaces(&list);
        assert_eq!(w["activeIndex"], 2);
        assert_eq!(w["focusedOutput"], "DP-2");
        assert_eq!(w["activeIndicesByOutput"], json!({"DP-1": 0, "DP-2": 2}));
        assert_eq!(w["occupiedMap"], json!({"0": true, "2": true}));
        assert_eq!(w["workspaceCount"], 3);
        let unfocused = json!([
            {"index": 2, "output": "DP-1", "active": false},
            {"index": 4, "output": "DP-1", "active": true}]);
        let w = workspaces(&unfocused);
        assert_eq!(w["activeIndex"], 3);
        assert_eq!(w["focusedOutput"], "");
    }

    #[test]
    fn a_missing_index_is_the_position_on_the_output() {
        let w = workspaces(
            &json!([{"output": "A", "active": false}, {"output": "B", "occupied": true}, {"output": "A", "active": true, "occupied": true}]),
        );
        assert_eq!(w["activeIndicesByOutput"], json!({"A": 1}));
        assert_eq!(w["occupiedByOutput"], json!({"A": {"1": true}, "B": {"0": true}}));
        assert_eq!(w["countByOutput"], json!({"A": 2, "B": 1}));
    }

    #[test]
    fn nothing_or_garbage_gives_empty_workspaces() {
        let empty = json!({
            "activeIndex": 0,
            "activeIndicesByOutput": {},
            "occupiedByOutput": {},
            "occupiedMap": {},
            "countByOutput": {},
            "workspaceCount": 0,
            "focusedOutput": "",
        });
        assert_eq!(workspaces(&json!([])), empty);
        assert_eq!(workspaces(&Json::Null), empty);
        assert_eq!(workspaces(&json!("workspaces")), empty);
        assert_eq!(workspaces(&json!([1, "x", null]))["countByOutput"], json!({"": 3}));
    }

    #[test]
    fn windows_without_an_output_take_that_of_their_workspace() {
        let list = windows(&old_windows(), &two_outputs());
        assert_eq!(list[0]["output"], "HEADLESS-2");
        assert_eq!(list[1]["output"], "HEADLESS-2");
        assert_eq!(list[0]["workspace_index"], 0);
        assert_eq!(list[0]["pid"], 102681);
        assert_eq!(list[1]["title"], "user@host:~");
        assert_eq!(windows(&old_windows(), &Json::Null)[0]["output"], "");
        assert_eq!(windows(&old_windows(), &Json::Null)[0]["workspace_index"], -1);
        assert_eq!(windows(&json!({"x": 1}), &two_outputs()), json!([]));
    }

    #[test]
    fn windows_with_an_output_are_left_alone() {
        let list = json!([
            {"id": "a", "workspace": "HEADLESS-2:1", "output": "DP-9"},
            {"id": "b", "workspace": "HEADLESS-2:1", "output": ""}]);
        let filled = windows(&list, &two_outputs());
        assert_eq!(filled[0]["output"], "DP-9");
        assert_eq!(filled[0]["workspace_index"], 0);
        assert_eq!(filled[1]["output"], "HEADLESS-2");
    }

    #[test]
    fn windows_know_the_index_of_their_workspace() {
        let list = json!([{"id": "a", "workspace": "HEADLESS-2:2"}, {"id": "b", "workspace": "gone"}, {"id": "c"}]);
        let found = windows(&list, &two_outputs());
        assert_eq!(found[0]["workspace_index"], 1);
        assert_eq!(found[1]["workspace_index"], -1);
        assert_eq!(found[2]["workspace_index"], -1);
    }

    #[test]
    fn the_keyboard_layout_is_named_by_its_index() {
        let names = json!(["English (US)", "Russian"]);
        assert_eq!(
            keyboard(&json!({"names": names, "current_index": 1})),
            json!({"layout": "Russian", "layouts": names, "index": 1})
        );
        assert_eq!(
            keyboard(&json!({"names": names, "current_index": 5})),
            json!({"layout": "", "layouts": names, "index": 5})
        );
        assert_eq!(keyboard(&json!({})), json!({"layout": "", "layouts": [], "index": 0}));
    }

    #[test]
    fn focus_follows_the_active_window_not_the_remembered_one() {
        let mut umbriel = umbriel();
        let mut list = old_windows();
        list[0]["active"] = false.into();
        list[1]["active"] = true.into();
        list[1]["title"] = "htop".into();
        umbriel.take(&event("windows", &list));
        assert_eq!(umbriel.focus(), Some(("kitty".to_owned(), "htop".to_owned())));
        list[1]["active"] = false.into();
        umbriel.take(&event("windows", &list));
        assert_eq!(umbriel.focus(), None);
    }

    #[test]
    fn windows_arriving_before_workspaces_get_their_outputs_once_those_come() {
        let mut umbriel = umbriel();
        assert_eq!(umbriel.state().keyboard, Json::Null);
        assert_eq!(umbriel.state().windows, json!([]));
        umbriel.take(&event("windows", &old_windows()));
        assert_eq!(umbriel.state().windows[0]["output"], "");
        assert_eq!(umbriel.state().windows[0]["workspace_index"], -1);
        umbriel.take(&event("workspaces", &two_outputs()));
        let state = umbriel.state();
        assert_eq!(state.windows[0]["output"], "HEADLESS-2");
        assert_eq!(state.windows[0]["workspace_index"], 0);
        assert_eq!(state.workspaces["focusedOutput"], "HEADLESS-2");
        assert_eq!(state.keyboard, Json::Null);
        umbriel.take(&event("keyboard_layout", &json!({"names": ["us", "ru"], "current_index": 1})));
        umbriel.take(&event("pointer", &json!({})));
        assert_eq!(umbriel.state().keyboard["layout"], "ru");
        umbriel.take(&event("windows", &json!([])));
        assert_eq!(umbriel.state().windows, json!([]));
    }

    #[test]
    fn a_burst_of_lines_is_applied_in_order() {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let mut umbriel = Umbriel::new(Lines::new(ours));
        let lines = [
            r#"{"data":[{"active":true,"app_id":"kitty","id":"c8","title":"t","workspace":"HEADLESS-2:1"}],"event":"windows"}"#.to_owned(),
            "not json".to_owned(),
            format!("{}", event("workspaces", &two_outputs())),
        ];
        theirs.write_all(format!("{}\n", lines.join("\n")).as_bytes()).unwrap();
        umbriel.read(0).unwrap();
        assert_eq!(umbriel.state().windows[0]["output"], "HEADLESS-2");
        assert_eq!(umbriel.focus(), Some(("kitty".to_owned(), "t".to_owned())));
        drop(theirs);
        assert!(umbriel.read(0).is_err());
    }
}
