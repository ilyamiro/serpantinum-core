//! Niri over its socket: the event stream starts with the full state and then reports changes,
//! which are applied the way the shell's NiriState did.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::os::fd::RawFd;

use serde_json::{Map, Value as Json, json};

use super::{Adapter, Lines, Session, State, index_map, missing, unix};

#[derive(Default)]
pub struct Model {
    workspaces: BTreeMap<i64, Json>,
    windows: BTreeMap<i64, Json>,
    layouts: Option<(Json, i64)>,
}

fn by_id(list: Option<&Json>) -> BTreeMap<i64, Json> {
    list.and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| Some((item.get("id")?.as_i64()?, item.clone())))
        .collect()
}

fn set(object: &mut Json, key: &str, value: impl Into<Json>) {
    if let Some(map) = object.as_object_mut() {
        map.insert(key.to_owned(), value.into());
    }
}

fn is_true(object: &Json, key: &str) -> bool {
    object.get(key) == Some(&Json::Bool(true))
}

fn map_of(items: &BTreeMap<i64, Json>) -> Json {
    Json::Object(items.iter().map(|(id, item)| (id.to_string(), item.clone())).collect())
}

impl Model {
    /// Applies one event; false when it is not one that changes what is shown
    pub fn apply(&mut self, event: &Json) -> bool {
        let Some((name, p)) = event.as_object().and_then(|o| o.iter().next()) else { return false };
        match name.as_str() {
            "WorkspacesChanged" => self.workspaces = by_id(p.get("workspaces")),
            "WorkspaceActivated" => {
                let (Some(id), focused) = (p.get("id").and_then(Json::as_i64), p.get("focused") != Some(&false.into()))
                else {
                    return true;
                };
                let Some(output) = self.workspaces.get(&id).map(|w| w.get("output").cloned()) else { return true };
                for (wid, w) in self.workspaces.iter_mut() {
                    if w.get("output").cloned() == output {
                        set(w, "is_active", *wid == id);
                        if focused {
                            set(w, "is_focused", *wid == id);
                        }
                    } else if focused {
                        set(w, "is_focused", false);
                    }
                }
            }
            "WorkspaceActiveWindowChanged" => {
                let id = p.get("workspace_id").and_then(Json::as_i64).unwrap_or(-1);
                let Some(w) = self.workspaces.get_mut(&id) else { return false };
                set(w, "active_window_id", p.get("active_window_id").cloned().unwrap_or(Json::Null));
            }
            "WindowsChanged" => self.windows = by_id(p.get("windows")),
            "WindowOpenedOrChanged" => {
                let Some(window) = p.get("window") else { return true };
                let Some(id) = window.get("id").and_then(Json::as_i64) else { return true };
                if is_true(window, "is_focused") {
                    for w in self.windows.values_mut().filter(|w| is_true(w, "is_focused")) {
                        set(w, "is_focused", false);
                    }
                }
                self.windows.insert(id, window.clone());
            }
            "WindowClosed" => {
                if let Some(id) = p.get("id").and_then(Json::as_i64) {
                    self.windows.remove(&id);
                }
            }
            "WindowFocusChanged" => {
                let focused = p.get("id").and_then(Json::as_i64);
                for (id, w) in self.windows.iter_mut() {
                    set(w, "is_focused", Some(*id) == focused);
                }
            }
            "KeyboardLayoutsChanged" => {
                let layouts = p.get("keyboard_layouts").cloned().unwrap_or_default();
                let names = layouts.get("names").cloned().filter(Json::is_array).unwrap_or_else(|| json!([]));
                self.layouts = Some((names, layouts.get("current_idx").and_then(Json::as_i64).unwrap_or(0)));
            }
            "KeyboardLayoutSwitched" => {
                let index = p.get("idx").and_then(Json::as_i64).unwrap_or(0);
                let names = self.layouts.take().map(|(names, _)| names).unwrap_or_else(|| json!([]));
                self.layouts = Some((names, index));
            }
            _ => return false,
        }
        true
    }

    fn workspaces_json(&self) -> Json {
        let with_windows: Vec<&Json> = self.windows.values().filter_map(|w| w.get("workspace_id")).collect();
        let (mut occupied, mut existing) = (Vec::new(), Vec::new());
        let mut max = 0;
        let (mut first, mut focused, mut active) = (-1, -1, -1);
        let mut outputs: Vec<(String, i64, Option<i64>, Option<i64>)> = Vec::new();
        for w in self.workspaces.values() {
            let idx = w.get("idx").or_else(|| w.get("id")).and_then(Json::as_i64).unwrap_or(1) - 1;
            if idx < 0 {
                continue;
            }
            existing.push(idx);
            max = max.max(idx);
            if first < 0 {
                first = idx;
            }
            let has_active_window = w.get("active_window_id").is_some_and(|a| !a.is_null());
            if has_active_window || w.get("id").is_some_and(|id| with_windows.contains(&id)) {
                occupied.push(idx);
            }
            let output = w.get("output").and_then(Json::as_str).unwrap_or_default();
            if !output.is_empty() {
                let at = match outputs.iter().position(|(o, ..)| o == output) {
                    Some(at) => at,
                    None => {
                        outputs.push((output.to_owned(), idx, None, None));
                        outputs.len() - 1
                    }
                };
                if is_true(w, "is_focused") {
                    outputs[at].2 = Some(idx);
                } else if is_true(w, "is_active") && outputs[at].3.is_none() {
                    outputs[at].3 = Some(idx);
                }
            }
            if is_true(w, "is_focused") {
                focused = idx;
            } else if is_true(w, "is_active") && active < 0 {
                active = idx;
            }
        }
        let current = [focused, active, first].into_iter().find(|i| *i >= 0).unwrap_or(0);
        let by_output: Map<String, Json> = outputs
            .into_iter()
            .map(|(output, first, focused, active)| (output, focused.or(active).unwrap_or(first).into()))
            .collect();
        json!({
            "activeIndex": current,
            "activeIndicesByOutput": by_output,
            "occupiedMap": index_map(occupied),
            "existingMap": index_map(existing),
            "maxWorkspaceIndex": max,
            "map": map_of(&self.workspaces),
        })
    }

    pub fn focus(&self) -> Option<(String, String)> {
        let w = self.windows.values().find(|w| is_true(w, "is_focused"))?;
        let text = |key: &str| w.get(key).and_then(Json::as_str).unwrap_or_default().to_owned();
        Some((text("app_id"), text("title")))
    }

    pub fn state(&self) -> State {
        State {
            workspaces: self.workspaces_json(),
            windows: map_of(&self.windows),
            keyboard: match &self.layouts {
                Some((names, index)) => json!({
                    "layout": names.get(*index as usize).cloned().unwrap_or_else(|| "".into()),
                    "layouts": names,
                    "index": index,
                }),
                None => Json::Null,
            },
        }
    }
}

/// What niri answers to one request, inside its `Ok`
pub fn request(session: &Session, request: &str) -> io::Result<Json> {
    let path = session.get("NIRI_SOCKET").ok_or_else(|| missing("NIRI_SOCKET"))?;
    let mut lines = Lines::new(unix(path)?);
    lines.stream.write_all(format!("\"{request}\"\n").as_bytes())?;
    let mut reply: Json = serde_json::from_str(&lines.read_line()?).map_err(io::Error::other)?;
    reply.get_mut("Ok").map(Json::take).ok_or_else(|| io::Error::other(format!("niri refused {request}: {reply}")))
}

pub struct Niri {
    events: Lines,
    model: Model,
}

impl Niri {
    pub fn connect(session: &Session) -> io::Result<Niri> {
        let path = session.get("NIRI_SOCKET").ok_or_else(|| missing("NIRI_SOCKET"))?;
        let mut events = Lines::new(unix(path)?);
        events.stream.write_all(b"\"EventStream\"\n")?;
        let reply: Json = serde_json::from_str(&events.read_line()?).map_err(io::Error::other)?;
        if reply.get("Ok").is_none() {
            return Err(io::Error::other(format!("niri refused the event stream: {reply}")));
        }
        events.stream.set_read_timeout(None)?;
        Ok(Niri { events, model: Model::default() })
    }
}

impl Adapter for Niri {
    fn fds(&self) -> Vec<RawFd> {
        vec![self.events.fd()]
    }

    fn read(&mut self, _: usize) -> io::Result<()> {
        for line in self.events.read()? {
            if let Ok(event) = serde_json::from_str::<Json>(&line) {
                self.model.apply(&event);
            }
        }
        Ok(())
    }

    fn focus(&self) -> Option<(String, String)> {
        self.model.focus()
    }

    fn state(&self) -> State {
        self.model.state()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> Model {
        let mut model = Model::default();
        model.apply(&json!({"WorkspacesChanged": {"workspaces": [
            {"id": 1, "idx": 1, "output": "DP-1", "is_active": true, "is_focused": true, "active_window_id": 5},
            {"id": 2, "idx": 2, "output": "DP-1", "is_active": false, "is_focused": false, "active_window_id": null},
            {"id": 3, "idx": 1, "output": "HDMI-A-1", "is_active": true, "is_focused": false, "active_window_id": null},
            {"id": 4, "idx": 2, "output": "HDMI-A-1", "is_active": false, "is_focused": false, "active_window_id": null}]}}));
        model.apply(&json!({"WindowsChanged": {"windows": [
            {"id": 5, "app_id": "foot", "workspace_id": 1, "is_focused": true},
            {"id": 6, "app_id": "mpv", "workspace_id": 4, "is_focused": false}]}}));
        model
    }

    #[test]
    fn workspaces_are_derived_per_output() {
        let w = model().state().workspaces;
        assert_eq!(w["activeIndex"], 0);
        assert_eq!(w["activeIndicesByOutput"], json!({"DP-1": 0, "HDMI-A-1": 0}));
        assert_eq!(w["occupiedMap"], json!({"0": true, "1": true}));
        assert_eq!(w["existingMap"], json!({"0": true, "1": true}));
        assert_eq!(w["maxWorkspaceIndex"], 1);
        assert_eq!(w["map"]["3"]["output"], "HDMI-A-1");
    }

    #[test]
    fn activation_moves_focus_across_outputs() {
        let mut model = model();
        model.apply(&json!({"WorkspaceActivated": {"id": 4, "focused": true}}));
        let w = model.state().workspaces;
        assert_eq!(w["activeIndex"], 1);
        assert_eq!(w["activeIndicesByOutput"], json!({"DP-1": 0, "HDMI-A-1": 1}));
        assert_eq!(w["map"]["1"]["is_focused"], false);
        assert_eq!(w["map"]["1"]["is_active"], true);
    }

    #[test]
    fn window_events_keep_one_focused_window() {
        let mut model = model();
        model.apply(&json!({"WindowOpenedOrChanged": {"window": {"id": 7, "app_id": "x", "workspace_id": 2, "is_focused": true}}}));
        let windows = model.state().windows;
        assert_eq!(windows["5"]["is_focused"], false);
        assert_eq!(windows["7"]["is_focused"], true);
        model.apply(&json!({"WindowFocusChanged": {"id": null}}));
        model.apply(&json!({"WindowClosed": {"id": 6}}));
        let windows = model.state().windows;
        assert!(windows.as_object().unwrap().values().all(|w| w["is_focused"] == false));
        assert!(windows.get("6").is_none());
        assert_eq!(model.state().workspaces["occupiedMap"], json!({"0": true, "1": true}));
    }

    #[test]
    fn keyboard_layouts_follow_switches() {
        let mut model = Model::default();
        assert_eq!(model.state().keyboard, Json::Null);
        model.apply(&json!({"KeyboardLayoutsChanged": {"keyboard_layouts": {"names": ["English (US)", "Russian"], "current_idx": 0}}}));
        model.apply(&json!({"KeyboardLayoutSwitched": {"idx": 1}}));
        assert_eq!(
            model.state().keyboard,
            json!({"layout": "Russian", "layouts": ["English (US)", "Russian"], "index": 1})
        );
        assert!(!model.apply(&json!({"OverviewOpenedOrClosed": {"is_open": true}})));
    }
}
