//! The outputs that show something, for the shell's monitor lists: asked of Mango, Hyprland, Niri
//! or Sway over its socket, else of any Wayland compositor through wl_output, else read from the
//! connectors the kernel reports connected.

use std::path::Path;

use serde_json::{Value as Json, json};
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};

use crate::compositor::{Session, hyprland, mango, niri, sway};

/// Refresh rate when the source does not tell one
const DEFAULT_REFRESH: f64 = 60.0;
/// wl_output's name event arrived in version 4
const OUTPUT_VERSION: u32 = 4;

#[derive(Debug, Clone, PartialEq)]
pub struct Monitor {
    pub name: String,
    pub width: Option<i64>,
    pub height: Option<i64>,
    /// In Hz
    pub refresh: f64,
}

impl Monitor {
    pub fn json(&self) -> Json {
        json!({ "name": self.name, "width": self.width, "height": self.height, "refreshRate": rate(self.refresh) })
    }
}

/// Hz rounded to two decimals, whole rates as integers
fn rate(hz: f64) -> Json {
    let rounded = (hz * 100.0).round() / 100.0;
    if rounded.fract() == 0.0 && rounded.abs() < 1e15 {
        Json::from(rounded as i64)
    } else {
        serde_json::Number::from_f64(rounded).map_or(Json::Null, Json::Number)
    }
}

/// The first source that knows of any output
pub fn list(session: &Session) -> Vec<Monitor> {
    let sources: [&dyn Fn() -> Vec<Monitor>; 6] = [
        &|| mango::request(session, "get all-monitors").map(|r| from_mango(&r)).unwrap_or_default(),
        &|| hyprland::request(session, "j/monitors").map(|r| from_hyprland(&r)).unwrap_or_default(),
        &|| niri::request(session, "Outputs").map(|r| from_niri(&r)).unwrap_or_default(),
        &|| sway::ask(session, sway::GET_OUTPUTS).map(|r| from_sway(&r)).unwrap_or_default(),
        &|| from_wayland().unwrap_or_default(),
        &|| from_drm(Path::new("/")),
    ];
    sources.iter().map(|source| source()).find(|found| !found.is_empty()).unwrap_or_default()
}

fn int(value: &Json, key: &str) -> Option<i64> {
    value.get(key).and_then(Json::as_i64)
}

fn items(list: Option<&Json>) -> impl Iterator<Item = &Json> {
    list.and_then(Json::as_array).into_iter().flatten()
}

fn named(item: &Json) -> Option<String> {
    item.get("name").and_then(Json::as_str).filter(|n| !n.is_empty()).map(str::to_owned)
}

/// Mango lists the outputs in use and tells no refresh rate
pub fn from_mango(reply: &Json) -> Vec<Monitor> {
    items(reply.get("monitors"))
        .filter_map(|m| {
            Some(Monitor {
                name: named(m)?,
                width: int(m, "width"),
                height: int(m, "height"),
                refresh: DEFAULT_REFRESH,
            })
        })
        .collect()
}

/// A `j/monitors` reply, which leaves disabled monitors out
pub fn from_hyprland(reply: &str) -> Vec<Monitor> {
    let Ok(monitors) = serde_json::from_str::<Json>(reply) else { return Vec::new() };
    items(Some(&monitors))
        .filter_map(|m| {
            let refresh = m.get("refreshRate").and_then(Json::as_f64).unwrap_or(DEFAULT_REFRESH);
            Some(Monitor { name: named(m)?, width: int(m, "width"), height: int(m, "height"), refresh })
        })
        .collect()
}

/// An `Outputs` reply, keyed by name; an output without a current mode is off. Niri keeps them in
/// no order, so they are sorted by name.
pub fn from_niri(reply: &Json) -> Vec<Monitor> {
    let Some(outputs) = reply.get("Outputs").and_then(Json::as_object) else { return Vec::new() };
    let mut monitors: Vec<Monitor> = outputs
        .iter()
        .filter_map(|(key, o)| {
            let mode = o.get("modes")?.get(usize::try_from(o.get("current_mode")?.as_u64()?).ok()?)?;
            Some(Monitor {
                name: named(o).unwrap_or_else(|| key.clone()),
                width: int(mode, "width"),
                height: int(mode, "height"),
                refresh: mode.get("refresh_rate").and_then(Json::as_f64).map_or(DEFAULT_REFRESH, |mhz| mhz / 1000.0),
            })
        })
        .collect();
    monitors.sort_by(|a, b| a.name.cmp(&b.name));
    monitors
}

/// A `GET_OUTPUTS` reply, of which the active outputs count
pub fn from_sway(reply: &Json) -> Vec<Monitor> {
    items(Some(reply))
        .filter(|o| o.get("active") == Some(&Json::Bool(true)))
        .filter_map(|o| {
            let mode = o.get("current_mode").filter(|m| m.is_object());
            Some(Monitor {
                name: named(o)?,
                width: mode.and_then(|m| int(m, "width")),
                height: mode.and_then(|m| int(m, "height")),
                refresh: mode.and_then(|m| m.get("refresh")?.as_f64()).map_or(DEFAULT_REFRESH, |mhz| mhz / 1000.0),
            })
        })
        .collect()
}

#[derive(Default)]
struct Output {
    name: Option<String>,
    mode: Option<(i32, i32, i32)>,
}

#[derive(Default)]
struct Outputs(Vec<Output>);

impl Dispatch<WlRegistry, ()> for Outputs {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event
            && interface == "wl_output"
        {
            registry.bind::<WlOutput, usize, Self>(name, version.min(OUTPUT_VERSION), qh, state.0.len());
            state.0.push(Output::default());
        }
    }
}

impl Dispatch<WlOutput, usize> for Outputs {
    fn event(
        state: &mut Self,
        _: &WlOutput,
        event: wl_output::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let output = &mut state.0[*index];
        match event {
            wl_output::Event::Name { name } => output.name = Some(name),
            wl_output::Event::Mode { flags: WEnum::Value(flags), width, height, refresh }
                if flags.contains(wl_output::Mode::Current) =>
            {
                output.mode = Some((width, height, refresh))
            }
            _ => {}
        }
    }
}

/// The outputs the Wayland compositor announces; those too old to tell their name are left out
fn from_wayland() -> Option<Vec<Monitor>> {
    let conn = Connection::connect_to_env().ok()?;
    let mut queue = conn.new_event_queue();
    conn.display().get_registry(&queue.handle(), ());
    let mut outputs = Outputs::default();
    // The first round binds the outputs, the second brings what they tell
    queue.roundtrip(&mut outputs).ok()?;
    queue.roundtrip(&mut outputs).ok()?;
    let monitors = outputs.0.into_iter().filter_map(|o| {
        let (width, height, refresh) = match o.mode {
            Some((w, h, mhz)) => (Some(w.into()), Some(h.into()), f64::from(mhz) / 1000.0),
            None => (None, None, DEFAULT_REFRESH),
        };
        Some(Monitor { name: o.name?, width, height, refresh })
    });
    Some(monitors.collect())
}

/// `1920x1080` as its two numbers
fn size(mode: &str) -> (Option<i64>, Option<i64>) {
    match mode.split_once('x') {
        Some((w, h)) => (w.parse().ok(), h.parse().ok()),
        None => (None, None),
    }
}

/// The connected connectors under `root`, in the first mode the kernel lists for each
fn from_drm(root: &Path) -> Vec<Monitor> {
    let Ok(entries) = std::fs::read_dir(root.join("sys/class/drm")) else { return Vec::new() };
    let mut connectors: Vec<(String, std::path::PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let (_, connector) = name.strip_prefix("card")?.split_once('-')?;
            Some((connector.to_owned(), e.path()))
        })
        .collect();
    connectors.sort_by_key(|(_, dir)| dir.file_name().map(|f| f.to_string_lossy().to_lowercase()));
    connectors
        .into_iter()
        .filter(|(_, dir)| std::fs::read_to_string(dir.join("status")).is_ok_and(|s| s.trim() == "connected"))
        .map(|(name, dir)| {
            let modes = std::fs::read_to_string(dir.join("modes")).unwrap_or_default();
            let (width, height) = modes.lines().next().map_or((None, None), size);
            Monitor { name, width, height, refresh: DEFAULT_REFRESH }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(name: &str, width: i64, height: i64, refresh: f64) -> Monitor {
        Monitor { name: name.into(), width: Some(width), height: Some(height), refresh }
    }

    #[test]
    fn rates_are_rounded_to_two_decimals() {
        assert_eq!(rate(144.00301), json!(144));
        assert_eq!(rate(59.951), json!(59.95));
        assert_eq!(rate(165.0), json!(165));
        assert_eq!(rate(0.0), json!(0));
        assert_eq!(
            monitor("DP-1", 2560, 1440, 59.951).json().to_string(),
            r#"{"name":"DP-1","width":2560,"height":1440,"refreshRate":59.95}"#
        );
    }

    #[test]
    fn mango_lists_every_monitor_at_sixty() {
        let reply = json!({"monitors": [
            {"name": "HEADLESS-1", "active": false, "width": 1280, "height": 720},
            {"name": "HEADLESS-2", "active": true, "width": 1920, "height": 1080}]});
        assert_eq!(
            from_mango(&reply),
            [monitor("HEADLESS-1", 1280, 720, 60.0), monitor("HEADLESS-2", 1920, 1080, 60.0)]
        );
        assert!(from_mango(&json!({})).is_empty());
    }

    #[test]
    fn hyprland_reads_the_mode_of_each_monitor() {
        let reply = r#"[{"id":0,"name":"eDP-1","width":1920,"height":1080,"refreshRate":144.00301,"disabled":false},
                        {"id":1,"name":"DP-2","width":2560,"height":1440,"refreshRate":59.951}]"#;
        assert_eq!(
            from_hyprland(reply),
            [monitor("eDP-1", 1920, 1080, 144.00301), monitor("DP-2", 2560, 1440, 59.951)]
        );
        assert!(from_hyprland("ok").is_empty());
    }

    #[test]
    fn niri_skips_outputs_that_are_off() {
        let reply = json!({"Outputs": {
            "winit": {"name": "winit", "modes": [{"width": 950, "height": 1023, "refresh_rate": 60000, "is_preferred": true}], "current_mode": 0},
            "HDMI-A-1": {"name": "HDMI-A-1", "modes": [
                {"width": 3840, "height": 2160, "refresh_rate": 60000},
                {"width": 1920, "height": 1080, "refresh_rate": 143981}], "current_mode": 1},
            "DP-3": {"name": "DP-3", "modes": [{"width": 1920, "height": 1080, "refresh_rate": 60000}], "current_mode": null}}});
        assert_eq!(from_niri(&reply), [monitor("HDMI-A-1", 1920, 1080, 143.981), monitor("winit", 950, 1023, 60.0)]);
    }

    #[test]
    fn sway_lists_the_active_outputs() {
        let reply = json!([
            {"name": "HEADLESS-3", "active": true, "current_mode": {"width": 1280, "height": 720, "refresh": 0}},
            {"name": "DP-1", "active": true, "current_mode": {"width": 2560, "height": 1440, "refresh": 164956}},
            {"name": "HEADLESS-2", "active": false, "current_mode": null}]);
        assert_eq!(from_sway(&reply), [monitor("HEADLESS-3", 1280, 720, 0.0), monitor("DP-1", 2560, 1440, 164.956)]);
    }

    #[test]
    fn drm_lists_connected_connectors() {
        let dir = tempfile::tempdir().unwrap();
        let drm = dir.path().join("sys/class/drm");
        for (name, status, modes) in [
            ("card1-eDP-1", "connected", "1920x1080\n1280x720\n"),
            ("card1-HDMI-A-1", "connected", ""),
            ("card1-DP-1", "disconnected", "2560x1440\n"),
            ("card0-Virtual-1", "connected", "1024x768i\n"),
        ] {
            std::fs::create_dir_all(drm.join(name)).unwrap();
            std::fs::write(drm.join(name).join("status"), format!("{status}\n")).unwrap();
            std::fs::write(drm.join(name).join("modes"), modes).unwrap();
        }
        std::fs::create_dir_all(drm.join("card1")).unwrap();
        let none = |name: &str| Monitor { name: name.into(), width: None, height: None, refresh: 60.0 };
        assert_eq!(
            from_drm(dir.path()),
            [Monitor { width: Some(1024), ..none("Virtual-1") }, monitor("eDP-1", 1920, 1080, 60.0), none("HDMI-A-1")]
        );
    }
}
