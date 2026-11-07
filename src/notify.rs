//! Desktop notifications sent to the session's notification server, with buttons: a notification
//! with actions waits for one to be clicked and tells which, as `notify-send --action` does. The
//! call matches libnotify's: the icon goes in the `image-path` hint, not the app icon field, and
//! the app is called notify-send unless named.

use std::collections::HashMap;

use zbus::blocking::fdo::DBusProxy;
use zbus::blocking::{Connection, MessageIterator};
use zbus::message::Type;
use zbus::zvariant::Value;
use zbus::{MatchRule, Message};

const SERVER: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";

#[derive(Debug, PartialEq)]
pub struct Request {
    pub app: String,
    pub urgency: u8,
    pub icon: String,
    pub actions: Vec<(String, String)>,
    pub summary: String,
    pub body: String,
}

/// `[--app NAME] [--urgency low|normal|critical] [--icon ICON] [--action NAME=LABEL]... [--] SUMMARY
/// [BODY]`
pub fn parse(args: &[String]) -> Result<Request, String> {
    let mut request = Request {
        app: "notify-send".into(),
        urgency: 1,
        icon: String::new(),
        actions: Vec::new(),
        summary: String::new(),
        body: String::new(),
    };
    let mut text = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            text.extend(iter.by_ref().cloned());
            break;
        }
        let Some(name) = arg.strip_prefix("--") else {
            text.push(arg.clone());
            continue;
        };
        let value = iter.next().ok_or_else(|| format!("--{name} needs a value"))?.clone();
        match name {
            "app" => request.app = value,
            "icon" => request.icon = value,
            "urgency" => {
                request.urgency = match value.as_str() {
                    "low" => 0,
                    "normal" => 1,
                    "critical" => 2,
                    _ => return Err("--urgency takes low, normal or critical".into()),
                }
            }
            "action" => {
                let (key, label) = value
                    .split_once('=')
                    .filter(|(key, _)| !key.is_empty())
                    .ok_or_else(|| "--action takes NAME=LABEL".to_string())?;
                request.actions.push((key.into(), label.into()));
            }
            _ => return Err(format!("unknown option --{name}")),
        }
    }
    let mut text = text.into_iter();
    match (text.next(), text.next(), text.next()) {
        (Some(summary), body, None) => {
            request.summary = summary;
            request.body = body.unwrap_or_default();
            Ok(request)
        }
        _ => Err("notify takes a summary and an optional body".into()),
    }
}

/// Shows the notification. With actions, waits and returns the clicked one's name, or `None` when
/// the notification closes or the server leaves the bus first.
pub fn send(request: &Request) -> zbus::Result<Option<String>> {
    let conn = Connection::session()?;
    let waiting = !request.actions.is_empty();
    let messages = if waiting {
        let messages = MessageIterator::from(&conn);
        let dbus = DBusProxy::new(&conn)?;
        dbus.add_match_rule(MatchRule::builder().msg_type(Type::Signal).interface(SERVER)?.path(PATH)?.build())?;
        dbus.add_match_rule(
            MatchRule::builder()
                .msg_type(Type::Signal)
                .sender("org.freedesktop.DBus")?
                .interface("org.freedesktop.DBus")?
                .member("NameOwnerChanged")?
                .arg(0, SERVER)?
                .build(),
        )?;
        Some(messages)
    } else {
        None
    };

    let actions: Vec<&str> = request.actions.iter().flat_map(|(key, label)| [key.as_str(), label.as_str()]).collect();
    let mut hints: HashMap<&str, Value> =
        HashMap::from([("urgency", Value::U8(request.urgency)), ("sender-pid", Value::I64(std::process::id().into()))]);
    if !request.icon.is_empty() {
        hints.insert("image-path", Value::from(request.icon.as_str()));
    }
    let id: u32 = conn
        .call_method(
            Some(SERVER),
            PATH,
            Some(SERVER),
            "Notify",
            &(&request.app, 0u32, "", &request.summary, &request.body, actions, hints, -1i32),
        )?
        .body()
        .deserialize()?;

    let Some(messages) = messages else { return Ok(None) };
    for message in messages {
        if let Some(outcome) = outcome(&message?, id) {
            return Ok(outcome);
        }
    }
    Ok(None)
}

/// What a signal means for notification `id`: `None` when it is about something else
fn outcome(message: &Message, id: u32) -> Option<Option<String>> {
    let header = message.header();
    let member = header.member()?.as_str();
    let body = message.body();
    match member {
        "ActionInvoked" => body.deserialize::<(u32, String)>().ok().filter(|(i, _)| *i == id).map(|(_, a)| Some(a)),
        "NotificationClosed" => body.deserialize::<(u32, u32)>().ok().filter(|(i, _)| *i == id).map(|_| None),
        "NameOwnerChanged" => body
            .deserialize::<(String, String, String)>()
            .ok()
            .filter(|(name, _, owner)| name == SERVER && owner.is_empty())
            .map(|_| None),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_every_option() {
        let request = parse(&args(&[
            "--app",
            "Shot",
            "--urgency",
            "critical",
            "--icon",
            "/a.png",
            "--action",
            "default=Open folder",
            "--action",
            "later=",
            "Saved",
            "In ~/Pictures",
        ]))
        .unwrap();
        assert_eq!(
            request,
            Request {
                app: "Shot".into(),
                urgency: 2,
                icon: "/a.png".into(),
                actions: vec![("default".into(), "Open folder".into()), ("later".into(), String::new())],
                summary: "Saved".into(),
                body: "In ~/Pictures".into(),
            }
        );
    }

    #[test]
    fn defaults_to_normal_urgency_and_an_empty_body() {
        let request = parse(&args(&["Hello"])).unwrap();
        assert_eq!((request.app.as_str(), request.urgency, request.body.as_str()), ("notify-send", 1, ""));
        assert!(request.actions.is_empty());
    }

    #[test]
    fn takes_text_after_a_double_dash_as_is() {
        let request = parse(&args(&["--app", "A", "--", "--summary", "--body"])).unwrap();
        assert_eq!((request.summary.as_str(), request.body.as_str()), ("--summary", "--body"));
    }

    #[test]
    fn keeps_equals_signs_in_action_labels() {
        let request = parse(&args(&["--action", "go=a=b", "S"])).unwrap();
        assert_eq!(request.actions, vec![("go".into(), "a=b".into())]);
    }

    #[test]
    fn rejects_bad_arguments() {
        for bad in [
            &["--urgency", "high", "S"][..],
            &["--action", "nolabel", "S"],
            &["--action", "=Label", "S"],
            &["--color", "red", "S"],
            &["S", "--app"],
            &[],
            &["one", "two", "three"],
        ] {
            assert!(parse(&args(bad)).is_err(), "{bad:?}");
        }
    }
}
