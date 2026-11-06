//! Mango over its IPC socket, which speaks the lines `mmsg` sends: one connection per watched
//! stream, and each starts with the current state.

use std::io::{self, Write};
use std::os::fd::RawFd;

use serde_json::{Map, Value as Json, json};

use super::{Adapter, Lines, Session, State, index_map, missing, unix};

const STREAMS: [&str; 3] = ["watch all-tags", "watch all-clients", "watch keyboardlayout"];

/// The tags of every monitor as the shell's MangoState kept them
pub fn tags(payload: &Json) -> Json {
    let monitors = payload.get("all_tags").and_then(Json::as_array).map(Vec::as_slice).unwrap_or_default();
    let (mut active, mut by_output) = (Map::new(), Map::new());
    let mut occupied = Vec::new();
    let mut count = 0;
    let mut first = -1;
    for monitor in monitors {
        let Some(list) = monitor.get("tags").and_then(Json::as_array) else { continue };
        let name = monitor.get("monitor").and_then(Json::as_str).unwrap_or_default();
        let mut here = Vec::new();
        let mut active_here = -1;
        for (j, tag) in list.iter().enumerate() {
            let index = tag.get("index").and_then(Json::as_i64).unwrap_or(j as i64 + 1) - 1;
            count = count.max(index + 1);
            if tag.get("client_count").and_then(Json::as_i64).unwrap_or(0) > 0 {
                here.push(index);
                if !occupied.contains(&index) {
                    occupied.push(index);
                }
            }
            if tag.get("is_active") == Some(&Json::Bool(true)) && active_here < 0 {
                active_here = index;
            }
        }
        by_output.insert(name.to_owned(), index_map(here));
        if active_here >= 0 {
            active.insert(name.to_owned(), active_here.into());
            if first < 0 {
                first = active_here;
            }
        }
    }
    json!({
        "activeIndex": first.max(0),
        "activeIndicesByOutput": active,
        "occupiedByOutput": by_output,
        "occupiedMap": index_map(occupied),
        "tagCount": count,
    })
}

pub struct Mango {
    streams: Vec<Lines>,
    state: State,
}

impl Mango {
    pub fn connect(session: &Session) -> io::Result<Mango> {
        let path = session.get("MANGO_INSTANCE_SIGNATURE").ok_or_else(|| missing("MANGO_INSTANCE_SIGNATURE"))?;
        let streams = STREAMS
            .iter()
            .map(|command| {
                let mut lines = Lines::new(unix(path)?);
                lines.stream.write_all(format!("{command}\n").as_bytes())?;
                lines.stream.set_read_timeout(None)?;
                Ok(lines)
            })
            .collect::<io::Result<Vec<Lines>>>()?;
        let state = State { windows: json!([]), ..State::default() };
        Ok(Mango { streams, state })
    }

    fn take(&mut self, index: usize, payload: Json) {
        match index {
            0 => self.state.workspaces = tags(&payload),
            1 => {
                self.state.windows = payload.get("clients").cloned().filter(Json::is_array).unwrap_or_else(|| json!([]))
            }
            _ => {
                let layout = payload
                    .get("layout")
                    .and_then(Json::as_str)
                    .filter(|l| !l.is_empty() && *l != "null")
                    .unwrap_or("US");
                self.state.keyboard = json!({ "layout": layout, "layouts": [], "index": 0 });
            }
        }
    }
}

impl Adapter for Mango {
    fn fds(&self) -> Vec<RawFd> {
        self.streams.iter().map(Lines::fd).collect()
    }

    fn read(&mut self, index: usize) -> io::Result<()> {
        for line in self.streams[index].read()? {
            if let Ok(payload) = serde_json::from_str::<Json>(&line) {
                self.take(index, payload);
            }
        }
        Ok(())
    }

    fn focus(&self) -> Option<(String, String)> {
        let clients = self.state.windows.as_array()?;
        let c = clients.iter().find(|c| c["is_focused"] == true && !c["id"].is_null())?;
        let text = |key: &str| c[key].as_str().unwrap_or_default().to_owned();
        Some((text("appid"), text("title")))
    }

    fn state(&self) -> State {
        self.state.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_give_active_and_occupied_per_monitor() {
        let payload = json!({"all_tags": [
            {"monitor": "DP-1", "tags": [
                {"index": 1, "is_active": false, "client_count": 2},
                {"index": 2, "is_active": true, "client_count": 0},
                {"index": 3, "is_active": true, "client_count": 1}]},
            {"monitor": "HDMI-A-1", "tags": [
                {"index": 1, "is_active": true, "client_count": 1}]},
            {"monitor": "broken"}]});
        assert_eq!(
            tags(&payload),
            json!({
                "activeIndex": 1,
                "activeIndicesByOutput": {"DP-1": 1, "HDMI-A-1": 0},
                "occupiedByOutput": {"DP-1": {"0": true, "2": true}, "HDMI-A-1": {"0": true}},
                "occupiedMap": {"0": true, "2": true},
                "tagCount": 3,
            })
        );
        assert_eq!(tags(&json!({}))["activeIndex"], 0);
    }

    #[test]
    fn layout_and_clients_are_taken_as_sent() {
        let mut mango = Mango { streams: Vec::new(), state: State::default() };
        mango.take(1, json!({"clients": [{"id": 1, "appid": "foot"}]}));
        mango.take(2, json!({"layout": "English (US)"}));
        assert_eq!(mango.state.windows, json!([{"id": 1, "appid": "foot"}]));
        assert_eq!(mango.state.keyboard["layout"], "English (US)");
        mango.take(2, json!({"layout": ""}));
        assert_eq!(mango.state.keyboard["layout"], "US");
    }
}
