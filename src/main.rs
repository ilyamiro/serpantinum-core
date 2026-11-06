mod brightness;
mod cli;
mod clipboard;
mod compositor;
mod dbus;
mod idle;
mod leases;
mod locks;
mod sysinfo;

use std::path::PathBuf;
use std::process::ExitCode;

use zbus::blocking::fdo::DBusProxy;
use zbus::fdo::{RequestNameFlags, RequestNameReply};

use crate::dbus::{
    BRIGHTNESS_PATH, BUS_NAME, BrightnessControl, BrightnessFeed, CLIPBOARD_PATH, COMPOSITOR_PATH, Clipboard,
    Compositor, CompositorFeed, Core, IDLE_PATH, Idle, IdleFeed, LOCKS_PATH, LockFeed, LockState, SYSINFO_PATH,
    ScreenSaver, SysFeed, SysInfo,
};

enum Outcome {
    AlreadyRunning,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => {}
        Some("-h" | "--help") => {
            println!("{}", cli::USAGE);
            return ExitCode::SUCCESS;
        }
        Some("-V" | "--version") => {
            println!("serpantinum-core {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Some(_) => return cli::run(&args),
    }
    match run() {
        Ok(Outcome::AlreadyRunning) => {
            eprintln!("serpantinum-core: {BUS_NAME} is already served on this bus");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("serpantinum-core: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<Outcome, String> {
    let bus = zbus::blocking::Connection::session().map_err(|e| format!("cannot connect to the session bus: {e}"))?;
    let sysinfo = SysFeed::new();
    let locks = LockFeed::new();
    let compositor = CompositorFeed::new();
    let brightness = BrightnessFeed::new();
    let idle = IdleFeed::new();
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();

    let server = bus.object_server();
    server.at("/", Core).map_err(|e| e.to_string())?;
    server.at(SYSINFO_PATH, SysInfo(sysinfo.clone())).map_err(|e| e.to_string())?;
    server.at(LOCKS_PATH, LockState(locks.clone())).map_err(|e| e.to_string())?;
    server.at(CLIPBOARD_PATH, Clipboard { home }).map_err(|e| e.to_string())?;
    server.at(BRIGHTNESS_PATH, BrightnessControl(brightness.clone())).map_err(|e| e.to_string())?;
    server.at(IDLE_PATH, Idle(idle.clone())).map_err(|e| e.to_string())?;
    for path in idle::SCREENSAVER_PATHS {
        server.at(path, ScreenSaver(idle.clone())).map_err(|e| e.to_string())?;
    }
    server.at(COMPOSITOR_PATH, Compositor(compositor.clone())).map_err(|e| e.to_string())?;

    // Listen before taking the name, so no client can subscribe and leave unnoticed
    let departures = DBusProxy::new(&bus).and_then(|p| p.receive_name_owner_changed()).map_err(|e| e.to_string())?;

    match bus.request_name_with_flags(BUS_NAME, RequestNameFlags::DoNotQueue.into()) {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {}
        Ok(RequestNameReply::Exists | RequestNameReply::InQueue) | Err(zbus::Error::NameTaken) => {
            return Ok(Outcome::AlreadyRunning);
        }
        Err(error) => return Err(format!("cannot take the name {BUS_NAME}: {error}")),
    }

    {
        let (feed, bus) = (sysinfo.clone(), bus.clone());
        std::thread::spawn(move || feed.run(&bus));
    }
    {
        let (feed, bus) = (locks.clone(), bus.clone());
        std::thread::spawn(move || feed.run(&bus));
    }

    {
        let (feed, bus) = (compositor.clone(), bus.clone());
        std::thread::spawn(move || feed.run(&bus));
    }

    {
        let (feed, bus) = (brightness.clone(), bus.clone());
        std::thread::spawn(move || feed.run(&bus));
    }

    {
        let (feed, bus) = (idle.clone(), bus.clone());
        std::thread::spawn(move || feed.run(&bus));
    }

    for signal in departures {
        let Ok(args) = signal.args() else { continue };
        if args.new_owner().is_none() {
            sysinfo.leases.remove(args.name());
            locks.leases.remove(args.name());
            compositor.remove(args.name());
            brightness.remove(args.name());
            idle.remove(args.name());
        }
    }
    Err("lost the session bus".to_owned())
}
