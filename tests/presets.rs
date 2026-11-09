//! `presets` against temporary directories. The command needs no daemon or bus.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};
use tempfile::TempDir;

struct Fixture {
    root: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = TempDir::new().unwrap();
        fs::create_dir(root.path().join("presets")).unwrap();
        fs::create_dir(root.path().join("widgets")).unwrap();
        Fixture { root }
    }

    fn presets(&self) -> String {
        self.root.path().join("presets").display().to_string()
    }

    fn preset(&self, name: &str) -> String {
        format!("{}/{name}", self.presets())
    }

    fn put(&self, name: &str, content: &str) {
        fs::write(self.preset(name), content).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        let widgets = self.root.path().join("widgets").display().to_string();
        Command::new(env!("CARGO_BIN_EXE_serpantinum-core"))
            .arg("presets")
            .arg(args[0])
            .arg(self.presets())
            .arg(widgets)
            .args(&args[1..])
            .output()
            .unwrap()
    }

    fn list(&self) -> Vec<Value> {
        let output = self.run(&["list"]);
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.ends_with('\n') && text.matches('\n').count() == 1);
        serde_json::from_str(&text).unwrap()
    }

    fn exists(&self, name: &str) -> bool {
        Path::new(&self.preset(name)).exists()
    }
}

fn snapshot(prefix: &str) -> Value {
    json!({
        "bar": {"file": format!("/some/where/{prefix}_bar.png")},
        "dock": {"file": format!("{prefix}_dock.png")},
        "widgets": {"file": ""}
    })
}

#[test]
fn list_orders_by_creation_and_skips_what_is_not_a_preset() {
    let fx = Fixture::new();
    fx.put("c.json", r#"{"settings": {}, "created": 300}"#);
    fx.put("a.json", r#"{"settings": {}, "created": 100.5}"#);
    fx.put("none.json", r#"{"settings": {"x": 1}}"#);
    fx.put("broken.json", "{not json");
    fx.put("list.json", r#"{"settings": [], "created": 1}"#);
    fx.put("array.json", "[1]");
    fx.put("notes.txt", r#"{"settings": {}}"#);
    fx.put(".hidden.json", r#"{"settings": {}}"#);
    let ids: Vec<String> = fx.list().iter().map(|p| p["id"].as_str().unwrap().to_owned()).collect();
    assert_eq!(ids, ["none", "a", "c"]);
}

#[test]
fn list_of_a_missing_directory_is_empty() {
    let fx = Fixture::new();
    fs::remove_dir(fx.presets()).unwrap();
    assert!(fx.list().is_empty());
}

#[test]
fn list_keeps_key_order_and_large_integers() {
    let fx = Fixture::new();
    fx.put("p.json", r#"{"zeta": 1, "settings": {"b": 1, "a": 2}, "created": 1793919171191}"#);
    let output = fx.run(&["list"]);
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(text.trim(), r#"[{"zeta":1,"settings":{"b":1,"a":2},"created":1793919171191,"id":"p"}]"#);
}

#[test]
fn save_embeds_widgets_and_drops_orphaned_screenshots() {
    let fx = Fixture::new();
    let widgets = fx.root.path().join("widgets");
    for (name, layout) in [("eDP-1", "[1, 2]"), ("bad", "{}"), ("broken", "[")] {
        fs::create_dir(widgets.join(name)).unwrap();
        fs::write(widgets.join(name).join("layout.json"), layout).unwrap();
    }
    fs::create_dir(widgets.join("empty")).unwrap();
    for png in ["old_bar.png", "keep_dock.png", "old_dock.png"] {
        fx.put(png, "x");
    }
    fx.put(
        "p.json",
        r#"{"settings": {}, "snapshot": {"bar": {"file": "/x/old_bar.png"}, "dock": {"file": "keep_dock.png"}}}"#,
    );
    let payload = json!({
        "name": "Ünïcode ✓",
        "created": 5,
        "settings": {"a": 1},
        "snapshot": {"dock": {"file": "keep_dock.png"}}
    });
    let output = fx.run(&["save", &fx.preset("p.json"), &payload.to_string()]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(!fx.exists("old_bar.png") && fx.exists("keep_dock.png") && fx.exists("old_dock.png"));
    assert!(!fx.exists("p.json.tmp"));

    let written = fs::read_to_string(fx.preset("p.json")).unwrap();
    assert!(written.ends_with("}\n") && written.contains("Ünïcode ✓") && written.contains("\n  \"name\""));
    let stored: Value = serde_json::from_str(&written).unwrap();
    assert_eq!(stored["widgets"], json!({"eDP-1": [1, 2]}));
    let keys: Vec<&String> = stored.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["name", "created", "settings", "snapshot", "widgets"]);

    let listed: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(listed[0]["id"], "p");
    assert_eq!(listed[0]["widgets"], json!({"eDP-1": [1, 2]}));
}

#[test]
fn save_creates_the_directory_and_rejects_bad_json() {
    let fx = Fixture::new();
    fs::remove_dir(fx.presets()).unwrap();
    let ok = fx.run(&["save", &fx.preset("p.json"), r#"{"settings": {}}"#]);
    assert!(ok.status.success() && fx.exists("p.json"));
    let bad = fx.run(&["save", &fx.preset("q.json"), "{oops"]);
    assert!(!bad.status.success() && !fx.exists("q.json"));
    let not_object = fx.run(&["save", &fx.preset("q.json"), "[1]"]);
    assert!(!not_object.status.success() && !fx.exists("q.json"));
}

#[test]
fn widgets_replaces_the_snapshot_and_prints_nothing() {
    let fx = Fixture::new();
    fx.put("p.json", r#"{"settings": {}, "snapshot": {"bar": {"file": "b.png"}, "widgets": {"file": "w.png"}}}"#);
    let output = fx.run(&["widgets", &fx.preset("p.json"), r#"{"file": "new.png"}"#]);
    assert!(output.status.success() && output.stdout.is_empty());
    let stored: Value = serde_json::from_str(&fs::read_to_string(fx.preset("p.json")).unwrap()).unwrap();
    assert_eq!(stored["snapshot"]["widgets"], json!({"file": "new.png"}));
    assert_eq!(stored["snapshot"]["bar"], json!({"file": "b.png"}));
}

#[test]
fn widgets_leaves_a_preset_without_a_snapshot_object_alone() {
    let fx = Fixture::new();
    let original = r#"{"settings": {}, "snapshot": [1]}"#;
    fx.put("p.json", original);
    let output = fx.run(&["widgets", &fx.preset("p.json"), "{}"]);
    assert!(output.status.success() && output.stdout.is_empty());
    assert_eq!(fs::read_to_string(fx.preset("p.json")).unwrap(), original);
    assert!(!fx.run(&["widgets", &fx.preset("missing.json"), "{}"]).status.success());
}

#[test]
fn delete_removes_the_preset_and_its_screenshots() {
    let fx = Fixture::new();
    for name in ["x_bar.png", "x_dock.png", "other.png"] {
        fx.put(name, "x");
    }
    let preset = json!({"settings": {}, "snapshot": snapshot("x")});
    fx.put("x.json", &preset.to_string());
    fx.put("y.json", r#"{"settings": {}}"#);
    let output = fx.run(&["delete", &fx.preset("x.json")]);
    assert!(output.status.success());
    assert!(!fx.exists("x.json") && !fx.exists("x_bar.png") && !fx.exists("x_dock.png"));
    assert!(fx.exists("other.png"));
    let listed: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], "y");
}

#[test]
fn delete_of_a_missing_preset_succeeds() {
    let fx = Fixture::new();
    let output = fx.run(&["delete", &fx.preset("gone.json")]);
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "[]\n");
}
