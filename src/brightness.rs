//! Screen brightness. A backlight in /sys/class/backlight is read directly and written directly
//! when permitted, else through logind, as brightnessctl does; percentages round the way it rounds
//! them. Without a backlight, the first DDC/CI display is driven with ddcutil, its bus and maximum
//! cached in the shell's run directory and every call serialized by the same lock file the shell's
//! script used.

use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const BACKLIGHT: &str = "/sys/class/backlight";
const DETECT_TIMEOUT: Duration = Duration::from_secs(15);
const VCP_TIMEOUT: Duration = Duration::from_secs(5);
const STEP: i32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Backlight,
    Ddc,
    None,
}

impl Backend {
    pub fn name(self) -> &'static str {
        match self {
            Backend::Backlight => "backlight",
            Backend::Ddc => "ddc",
            Backend::None => "none",
        }
    }
}

fn read_number(path: &Path) -> Option<i64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// A backlight device with its current and maximum brightness
#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub dir: PathBuf,
    pub current: i64,
    pub max: i64,
}

impl Device {
    fn read(dir: PathBuf) -> Option<Device> {
        let current = read_number(&dir.join("brightness"))?;
        let max = read_number(&dir.join("max_brightness")).filter(|m| *m > 0)?;
        Some(Device { dir, current, max })
    }

    pub fn exact_percent(&self) -> f64 {
        if self.current < 0 { 0.0 } else { self.current as f64 * 100.0 / self.max as f64 }
    }

    pub fn percent(&self) -> i32 {
        self.exact_percent().round() as i32
    }

    fn value(&self, percent: f64) -> i64 {
        (percent / 100.0 * self.max as f64).round() as i64
    }

    /// The raw value for an absolute percentage
    pub fn target(&self, percent: i32) -> i64 {
        self.value(percent.into()).clamp(0, self.max)
    }

    /// The raw value `delta` percent away, moving at least one step when the delta is not zero
    pub fn shifted(&self, delta: i32) -> i64 {
        let mut change = self.value(self.exact_percent() + f64::from(delta)) - self.current;
        if delta != 0 && change == 0 {
            change = delta.signum().into();
        }
        (self.current + change).clamp(0, self.max)
    }

    pub fn name(&self) -> String {
        self.dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }
}

/// The first readable backlight, in directory order as brightnessctl takes it
pub fn backlight(root: &Path) -> Option<Device> {
    std::fs::read_dir(root).ok()?.flatten().find_map(|e| Device::read(e.path()))
}

/// The backlight's brightness files, to watch for changes
pub fn backlight_files(root: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(root).into_iter().flatten().flatten().map(|e| e.path().join("brightness")).collect()
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

pub fn backend() -> Backend {
    if backlight(Path::new(BACKLIGHT)).is_some() {
        Backend::Backlight
    } else if on_path("ddcutil") {
        Backend::Ddc
    } else {
        Backend::None
    }
}

/// Writes the raw value to the device, or asks logind to when the file is not writable
pub fn write_backlight(device: &Device, value: i64) -> Result<(), String> {
    let file = device.dir.join("brightness");
    if rustix::fs::access(&file, rustix::fs::Access::WRITE_OK).is_ok() {
        return std::fs::write(&file, value.to_string()).map_err(|e| e.to_string());
    }
    let bus = zbus::blocking::Connection::system().map_err(|e| e.to_string())?;
    let set = |session: &zbus::zvariant::ObjectPath| -> zbus::Result<()> {
        bus.call_method(
            Some("org.freedesktop.login1"),
            session,
            Some("org.freedesktop.login1.Session"),
            "SetBrightness",
            &("backlight", device.name(), value as u32),
        )?;
        Ok(())
    };
    let auto = zbus::zvariant::ObjectPath::from_static_str_unchecked("/org/freedesktop/login1/session/auto");
    if set(&auto).is_ok() {
        return Ok(());
    }
    // Started outside a session, the daemon has no "auto" session; the user's display session will do
    let user = zbus::blocking::Proxy::new(
        &bus,
        "org.freedesktop.login1",
        "/org/freedesktop/login1/user/self",
        "org.freedesktop.login1.User",
    )
    .map_err(|e| e.to_string())?;
    let (_, session): (String, zbus::zvariant::OwnedObjectPath) =
        user.get_property("Display").map_err(|e| e.to_string())?;
    set(&session).map_err(|e| e.to_string())
}

/// Runs a command and returns its output, or `None` if it fails or outlasts `timeout`
fn output(command: &mut Command, timeout: Duration) -> Option<String> {
    let mut child = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let text = reader.join().ok()?;
    status.success().then_some(text)
}

/// The I2C bus of the first valid display in `ddcutil detect --brief`
pub fn parse_detect(text: &str) -> Option<u32> {
    let mut valid = false;
    for line in text.lines() {
        if line.starts_with("Invalid display") {
            valid = false;
        } else if line.starts_with("Display ") {
            valid = true;
        }
        if valid && line.contains("I2C bus:") {
            let field = line.split_whitespace().nth(2)?;
            return field.rsplit_once("/i2c-").map_or(field, |(_, n)| n).parse().ok();
        }
    }
    None
}

/// The current and maximum value in `ddcutil --brief getvcp 10`
pub fn parse_vcp(text: &str) -> Option<(i64, i64)> {
    let fields: Vec<&str> = text.split_whitespace().collect();
    let current = fields.get(3)?.parse().ok()?;
    let max = fields.get(4)?.parse().ok().filter(|m: &i64| *m > 0)?;
    Some((current, max))
}

/// The ddcutil side, with its cache file of `bus [max]` and its lock file
pub struct Ddc {
    cache: PathBuf,
    lock: PathBuf,
}

impl Ddc {
    pub fn new(run_dir: &Path) -> Ddc {
        Ddc { cache: run_dir.join("brightness-ddc"), lock: run_dir.join("brightness-ddc.lock") }
    }

    /// Runs `f` holding the lock, shared with any other ddcutil caller of the shell
    fn locked<T>(&self, f: impl FnOnce() -> Option<T>) -> Option<T> {
        if let Some(dir) = self.lock.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let file = File::create(&self.lock).ok()?;
        // SAFETY: the descriptor belongs to `file`, which outlives the lock
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return None;
        }
        f()
    }

    fn cached(&self) -> (Option<u32>, Option<i64>) {
        let text = std::fs::read_to_string(&self.cache).unwrap_or_default();
        let mut fields = text.split_whitespace();
        (fields.next().and_then(|b| b.parse().ok()), fields.next().and_then(|m| m.parse().ok()).filter(|m| *m > 0))
    }

    fn bus(&self) -> Option<u32> {
        if let (Some(bus), _) = self.cached()
            && Path::new(&format!("/dev/i2c-{bus}")).exists()
        {
            return Some(bus);
        }
        let text = output(Command::new("ddcutil").args(["detect", "--brief"]), DETECT_TIMEOUT)?;
        let bus = parse_detect(&text)?;
        let _ = std::fs::write(&self.cache, format!("{bus}\n"));
        Some(bus)
    }

    fn read(&self) -> Option<(u32, i64, i64)> {
        for _ in 0..2 {
            let bus = self.bus()?;
            let Some(text) = output(
                Command::new("ddcutil").args(["--bus", &bus.to_string(), "--brief", "getvcp", "10"]),
                VCP_TIMEOUT,
            ) else {
                let _ = std::fs::remove_file(&self.cache);
                continue;
            };
            let (current, max) = parse_vcp(&text)?;
            let _ = std::fs::write(&self.cache, format!("{bus} {max}\n"));
            return Some((bus, current, max));
        }
        None
    }

    fn get_locked(&self) -> Option<i32> {
        self.read().map(|(_, current, max)| (current * 100 / max) as i32)
    }

    fn set_locked(&self, percent: i32) -> Option<()> {
        for _ in 0..2 {
            let (bus, max) = match self.cached() {
                (Some(bus), Some(max)) => (bus, max),
                _ => {
                    let (bus, _, max) = self.read()?;
                    (bus, max)
                }
            };
            let raw = i64::from(percent) * max / 100;
            let args = ["--bus", &bus.to_string(), "setvcp", "10", &raw.to_string()];
            if output(Command::new("ddcutil").args(args), VCP_TIMEOUT).is_some() {
                return Some(());
            }
            let _ = std::fs::remove_file(&self.cache);
        }
        None
    }

    pub fn get(&self) -> Option<i32> {
        self.locked(|| self.get_locked())
    }

    pub fn set(&self, percent: i32) -> Option<i32> {
        let percent = percent.clamp(0, 100);
        self.locked(|| self.set_locked(percent)).map(|_| percent)
    }

    pub fn adjust(&self, delta: i32) -> Option<i32> {
        self.locked(|| {
            let target = (self.get_locked()? + delta).clamp(0, 100);
            self.set_locked(target).map(|_| target)
        })
    }
}

/// The shell's run directory, where the DDC cache lives
pub fn run_dir() -> PathBuf {
    let env = |name| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    env("QS_RUN_DIR").unwrap_or_else(|| env("XDG_RUNTIME_DIR").unwrap_or_else(|| "/tmp".into()).join("serpantinum"))
}

/// The brightness now, in percent; `None` when there is nothing to read
pub fn get(ddc: &Ddc) -> Option<i32> {
    match backend() {
        Backend::Backlight => backlight(Path::new(BACKLIGHT)).map(|d| d.percent()),
        Backend::Ddc => ddc.get(),
        Backend::None => None,
    }
}

/// Sets an absolute percentage, or moves by `STEP` with `raise`; the percentage reached
pub fn change(ddc: &Ddc, change: Change) -> Result<Option<i32>, String> {
    match backend() {
        Backend::Backlight => {
            let device = backlight(Path::new(BACKLIGHT)).ok_or("the backlight went away")?;
            let value = match change {
                Change::Set(percent) => device.target(percent.clamp(0, 100)),
                Change::Step(up) => device.shifted(if up { STEP } else { -STEP }),
            };
            write_backlight(&device, value)?;
            Ok(Device::read(device.dir).map(|d| d.percent()))
        }
        Backend::Ddc => match change {
            Change::Set(percent) => ddc.set(percent),
            Change::Step(up) => ddc.adjust(if up { STEP } else { -STEP }),
        }
        .map(Some)
        .ok_or_else(|| "ddcutil could not set the brightness".to_owned()),
        Backend::None => Err("no backlight and no ddcutil".to_owned()),
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Change {
    Set(i32),
    Step(bool),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(current: i64, max: i64) -> Device {
        Device { dir: PathBuf::from("/x/intel_backlight"), current, max }
    }

    #[test]
    fn percentages_round_like_brightnessctl() {
        assert_eq!(device(0, 255).percent(), 0);
        assert_eq!(device(128, 255).percent(), 50);
        assert_eq!(device(19393, 19393).percent(), 100);
        assert_eq!(device(10, 255).target(50), 128);
        assert_eq!(device(10, 255).target(100), 255);
    }

    #[test]
    fn steps_move_at_least_one_unit_and_stay_in_range() {
        assert_eq!(device(128, 255).shifted(5), 141);
        assert_eq!(device(128, 255).shifted(-5), 115);
        assert_eq!(device(3, 10).shifted(5), 4);
        assert_eq!(device(3, 10).shifted(-5), 2);
        assert_eq!(device(254, 255).shifted(5), 255);
        assert_eq!(device(2, 255).shifted(-5), 0);
    }

    #[test]
    fn the_first_readable_backlight_is_used() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("broken")).unwrap();
        let good = dir.path().join("acpi_video0");
        std::fs::create_dir_all(&good).unwrap();
        std::fs::write(good.join("brightness"), "7\n").unwrap();
        std::fs::write(good.join("max_brightness"), "10\n").unwrap();
        assert_eq!(backlight(dir.path()), Some(Device { dir: good, current: 7, max: 10 }));
        assert_eq!(backlight(&dir.path().join("missing")), None);
    }

    #[test]
    fn ddcutil_output_is_parsed() {
        let detect = "Invalid display\n   I2C bus:  /dev/i2c-3\n\nDisplay 1\n   I2C bus:  /dev/i2c-6\n   Monitor: X\n";
        assert_eq!(parse_detect(detect), Some(6));
        assert_eq!(parse_detect("No displays found"), None);
        assert_eq!(parse_vcp("VCP 10 C 50 100\n"), Some((50, 100)));
        assert_eq!(parse_vcp("VCP 10 ERR"), None);
        assert_eq!(parse_vcp("VCP 10 C 50 0"), None);
    }

    #[test]
    fn ddc_cache_holds_bus_and_maximum() {
        let dir = tempfile::tempdir().unwrap();
        let ddc = Ddc::new(dir.path());
        assert_eq!(ddc.cached(), (None, None));
        std::fs::write(dir.path().join("brightness-ddc"), "6 100\n").unwrap();
        assert_eq!(ddc.cached(), (Some(6), Some(100)));
        assert_eq!(ddc.locked(|| Some(1)), Some(1));
    }
}
