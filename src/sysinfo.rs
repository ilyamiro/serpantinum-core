//! CPU, memory, temperature, network and disk usage, read from /proc and /sys

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const DISK_REFRESH: Duration = Duration::from_secs(60);
const EXCLUDED_FS: &[&str] = &["tmpfs", "devtmpfs", "squashfs", "overlay", "efivarfs", "iso9660"];
const TEMP_DRIVERS: &[&str] = &["coretemp", "k10temp", "zenpower", "cpu_thermal", "bcm2835_thermal"];
const THERMAL_TYPES: &[&str] = &["x86_pkg_temp", "cpu_thermal", "cpu-thermal"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CpuTimes {
    pub total: u64,
    pub idle: u64,
}

pub fn parse_cpu_times(proc_stat: &str) -> Option<CpuTimes> {
    let line = proc_stat.lines().find(|l| l.split_whitespace().next() == Some("cpu"))?;
    let fields: Vec<u64> = line.split_whitespace().skip(1).take(8).filter_map(|f| f.parse().ok()).collect();
    if fields.len() < 4 {
        return None;
    }
    Some(CpuTimes { total: fields.iter().sum(), idle: fields[3] })
}

pub fn cpu_usage_percent(previous: CpuTimes, current: CpuTimes) -> u32 {
    let total = current.total.saturating_sub(previous.total);
    if total == 0 {
        return 0;
    }
    let idle = current.idle.saturating_sub(previous.idle);
    (100 * total.saturating_sub(idle) / total) as u32
}

/// Received and sent bytes over the ethernet and wireless interfaces
pub fn parse_net_totals(proc_net_dev: &str) -> (u64, u64) {
    let (mut rx, mut tx) = (0u64, 0u64);
    for line in proc_net_dev.lines().skip(2) {
        let Some((name, stats)) = line.split_once(':') else { continue };
        let name = name.trim();
        if !(name.starts_with('e') || name.starts_with('w')) {
            continue;
        }
        let fields: Vec<&str> = stats.split_whitespace().collect();
        rx += fields.first().and_then(|f| f.parse::<u64>().ok()).unwrap_or(0);
        tx += fields.get(8).and_then(|f| f.parse::<u64>().ok()).unwrap_or(0);
    }
    (rx, tx)
}

/// The used memory as a percentage and in GiB, truncated to one decimal
pub fn parse_memory(meminfo: &str) -> (u32, f64) {
    let kib = |key: &str| -> u64 {
        meminfo
            .lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))
            .and_then(|v| v.split_whitespace().next()?.parse().ok())
            .unwrap_or(0)
    };
    let (total, available) = (kib("MemTotal"), kib("MemAvailable"));
    if total == 0 {
        return (0, 0.0);
    }
    let used = total.saturating_sub(available);
    ((100 * used / total) as u32, (used / 1048576) as f64 + ((used % 1048576) * 10 / 1048576) as f64 / 10.0)
}

pub fn find_temperature_path(hwmon_root: &Path, thermal_root: &Path) -> Option<PathBuf> {
    let list = |dir: &Path, prefix: &str| -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(prefix)))
            .collect();
        paths.sort();
        paths
    };
    let read = |path: PathBuf| std::fs::read_to_string(path).ok().map(|s| s.trim().to_owned());
    for hwmon in list(hwmon_root, "hwmon") {
        if read(hwmon.join("name")).is_some_and(|n| TEMP_DRIVERS.contains(&n.as_str()))
            && hwmon.join("temp1_input").is_file()
        {
            return Some(hwmon.join("temp1_input"));
        }
    }
    for zone in list(thermal_root, "thermal_zone") {
        if read(zone.join("type")).is_some_and(|t| THERMAL_TYPES.contains(&t.as_str())) {
            return Some(zone.join("temp"));
        }
    }
    [hwmon_root.join("hwmon0/temp1_input"), thermal_root.join("thermal_zone0/temp")].into_iter().find(|p| p.is_file())
}

pub fn read_temperature(path: Option<&Path>) -> u32 {
    let raw: i64 = path.and_then(|p| std::fs::read_to_string(p).ok()).and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    (if raw > 1000 { raw / 1000 } else { raw }).max(0) as u32
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct DiskUsage {
    pub percent: u32,
    pub used_gb: f64,
    pub total_gb: f64,
}

/// Sums (device, total KiB, used KiB), counting each device once
pub fn disk_totals(entries: &[(String, u64, u64)]) -> DiskUsage {
    let (mut total, mut used) = (0u64, 0u64);
    let mut seen: HashSet<&str> = HashSet::new();
    for (device, total_kib, used_kib) in entries {
        if seen.insert(device) {
            total += total_kib;
            used += used_kib;
        }
    }
    if total == 0 {
        return DiskUsage::default();
    }
    let round1 = |v: f64| (v * 10.0).round() / 10.0;
    DiskUsage {
        percent: ((used as f64 / total as f64) * 100.0 + 0.5) as u32,
        used_gb: round1(used as f64 / 1048576.0),
        total_gb: round1(total as f64 / 1048576.0),
    }
}

pub fn eligible_mounts(proc_mounts: &str) -> Vec<(String, PathBuf)> {
    proc_mounts
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let (device, mount, fstype) = (fields.next()?, fields.next()?, fields.next()?);
            let excluded = EXCLUDED_FS.contains(&fstype)
                || device.starts_with("/dev/loop")
                || device == "udev"
                || device == "none"
                || !device.starts_with('/') && !device.contains(':') && fstype == "fuse";
            (!excluded).then(|| (device.to_owned(), PathBuf::from(mount.replace("\\040", " "))))
        })
        .collect()
}

pub fn read_disk_usage(proc_mounts: &str) -> DiskUsage {
    let entries: Vec<(String, u64, u64)> = eligible_mounts(proc_mounts)
        .into_iter()
        .filter_map(|(device, mount)| {
            let stat = rustix::fs::statvfs(&mount).ok()?;
            let unit = stat.f_frsize.max(1);
            let total = stat.f_blocks * unit / 1024;
            let free = stat.f_bfree * unit / 1024;
            Some((device, total, total.saturating_sub(free)))
        })
        .collect();
    disk_totals(&entries)
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Usage {
    pub cpu: u32,
    pub ram_percent: u32,
    pub ram_gb: f64,
    pub temp: u32,
    pub net_rx: f64,
    pub net_tx: f64,
    pub disk: DiskUsage,
}

pub struct Sampler {
    proc_root: PathBuf,
    previous_cpu: Option<CpuTimes>,
    previous_net: Option<(u64, u64, Instant)>,
    temperature_path: Option<PathBuf>,
    disk: DiskUsage,
    disk_at: Option<Instant>,
}

impl Sampler {
    pub fn system() -> Sampler {
        Sampler::new(Path::new("/proc"), Path::new("/sys/class/hwmon"), Path::new("/sys/class/thermal"))
    }

    pub fn new(proc_root: &Path, hwmon_root: &Path, thermal_root: &Path) -> Sampler {
        Sampler {
            proc_root: proc_root.to_path_buf(),
            previous_cpu: None,
            previous_net: None,
            temperature_path: find_temperature_path(hwmon_root, thermal_root),
            disk: DiskUsage::default(),
            disk_at: None,
        }
    }

    /// Forgets the last network totals, so rates after a pause don't average over the pause
    pub fn reset_network(&mut self) {
        self.previous_net = None;
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.proc_root.join(name)).unwrap_or_default()
    }

    pub fn sample(&mut self, now: Instant) -> Usage {
        let cpu_times = parse_cpu_times(&self.read("stat"));
        let cpu = match (self.previous_cpu, cpu_times) {
            (Some(prev), Some(cur)) => cpu_usage_percent(prev, cur),
            _ => 0,
        };
        self.previous_cpu = cpu_times.or(self.previous_cpu);

        let (rx, tx) = parse_net_totals(&self.read("net/dev"));
        let (net_rx, net_tx) = match self.previous_net {
            Some((prev_rx, prev_tx, at)) if now > at => {
                let seconds = now.duration_since(at).as_secs_f64();
                (
                    if rx >= prev_rx { (rx - prev_rx) as f64 / seconds } else { 0.0 },
                    if tx >= prev_tx { (tx - prev_tx) as f64 / seconds } else { 0.0 },
                )
            }
            _ => (0.0, 0.0),
        };
        self.previous_net = Some((rx, tx, now));

        let (ram_percent, ram_gb) = parse_memory(&self.read("meminfo"));
        if self.disk_at.is_none_or(|at| now.duration_since(at) >= DISK_REFRESH) {
            self.disk = read_disk_usage(&self.read("self/mounts"));
            self.disk_at = Some(now);
        }
        Usage {
            cpu,
            ram_percent,
            ram_gb,
            temp: read_temperature(self.temperature_path.as_deref()),
            net_rx,
            net_tx,
            disk: self.disk,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_usage_comes_from_total_and_idle_deltas() {
        let before = parse_cpu_times("cpu  100 0 100 800 0 0 0 0 0 0\ncpu0 1 1 1 1").unwrap();
        let after = parse_cpu_times("cpu  200 0 200 1000 0 0 0 0 0 0").unwrap();
        assert_eq!(before, CpuTimes { total: 1000, idle: 800 });
        assert_eq!(cpu_usage_percent(before, after), 100 * (400 - 200) / 400);
        assert_eq!(cpu_usage_percent(before, before), 0);
        assert!(parse_cpu_times("nothing").is_none());
    }

    #[test]
    fn network_totals_only_count_ethernet_and_wifi_interfaces() {
        let text = "Inter-|   Receive\n face |bytes packets errs drop fifo frame compressed multicast|bytes packets\n\
    lo: 999 1 0 0 0 0 0 0 888 1 0 0 0 0 0 0\n\
  eth0: 1000 1 0 0 0 0 0 0 200 1 0 0 0 0 0 0\n\
wlan0: 500 1 0 0 0 0 0 0 50 1 0 0 0 0 0 0\n\
docker0: 777 1 0 0 0 0 0 0 777 1 0 0 0 0 0 0\n";
        assert_eq!(parse_net_totals(text), (1500, 250));
    }

    #[test]
    fn memory_percent_and_used_gigabytes() {
        let text = "MemTotal:       16384000 kB\nMemFree: 100 kB\nMemAvailable:    8192000 kB\n";
        let (percent, gb) = parse_memory(text);
        assert_eq!(percent, 50);
        assert!((gb - 7.8).abs() < 1e-9, "{gb}");
        assert_eq!(parse_memory("garbage"), (0, 0.0));
    }

    #[test]
    fn temperature_prefers_known_cpu_drivers_then_thermal_zones() {
        let dir = tempfile::tempdir().unwrap();
        let (hwmon, thermal) = (dir.path().join("hwmon"), dir.path().join("thermal"));
        std::fs::create_dir_all(hwmon.join("hwmon0")).unwrap();
        std::fs::create_dir_all(hwmon.join("hwmon1")).unwrap();
        std::fs::write(hwmon.join("hwmon0/name"), "nvme\n").unwrap();
        std::fs::write(hwmon.join("hwmon0/temp1_input"), "30000\n").unwrap();
        std::fs::write(hwmon.join("hwmon1/name"), "k10temp\n").unwrap();
        std::fs::write(hwmon.join("hwmon1/temp1_input"), "61500\n").unwrap();
        let path = find_temperature_path(&hwmon, &thermal).unwrap();
        assert_eq!(path, hwmon.join("hwmon1/temp1_input"));
        assert_eq!(read_temperature(Some(&path)), 61);

        std::fs::remove_dir_all(&hwmon).unwrap();
        std::fs::create_dir_all(thermal.join("thermal_zone3")).unwrap();
        std::fs::write(thermal.join("thermal_zone3/type"), "x86_pkg_temp\n").unwrap();
        std::fs::write(thermal.join("thermal_zone3/temp"), "47000\n").unwrap();
        let path = find_temperature_path(&hwmon, &thermal).unwrap();
        assert_eq!(read_temperature(Some(&path)), 47);
        assert_eq!(read_temperature(None), 0);
    }

    #[test]
    fn disk_usage_deduplicates_devices_and_skips_virtual_filesystems() {
        let mounts = "/dev/nvme0n1p2 / ext4 rw 0 0\n/dev/nvme0n1p2 /nix/store ext4 ro 0 0\ntmpfs /run tmpfs rw 0 0\n/dev/loop3 /snap/x squashfs ro 0 0\nudev /dev devtmpfs rw 0 0\n/dev/sda1 /mnt/with\\040space ext4 rw 0 0\n";
        let eligible = eligible_mounts(mounts);
        assert_eq!(eligible.len(), 3);
        assert_eq!(eligible[2].1, PathBuf::from("/mnt/with space"));
        let usage = disk_totals(&[
            ("/dev/a".into(), 2 * 1048576, 1048576),
            ("/dev/a".into(), 2 * 1048576, 1048576),
            ("/dev/b".into(), 2 * 1048576, 0),
        ]);
        assert_eq!(usage, DiskUsage { percent: 25, used_gb: 1.0, total_gb: 4.0 });
        assert_eq!(disk_totals(&[]), DiskUsage::default());
    }

    #[test]
    fn sampler_computes_rates_between_samples_from_fixture_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("net")).unwrap();
        std::fs::create_dir_all(dir.path().join("self")).unwrap();
        let write = |stat: &str, rx: u64, tx: u64| {
            std::fs::write(dir.path().join("stat"), stat).unwrap();
            std::fs::write(dir.path().join("net/dev"), format!("h\nh\neth0: {rx} 0 0 0 0 0 0 0 {tx} 0 0 0 0 0 0 0\n"))
                .unwrap();
        };
        std::fs::write(dir.path().join("meminfo"), "MemTotal: 1000 kB\nMemAvailable: 500 kB\n").unwrap();
        std::fs::write(dir.path().join("self/mounts"), "").unwrap();
        let mut sampler = Sampler::new(dir.path(), &dir.path().join("none"), &dir.path().join("none"));
        let start = Instant::now();
        write("cpu 100 0 100 800 0 0 0 0", 1000, 100);
        let first = sampler.sample(start);
        assert_eq!((first.cpu, first.net_rx), (0, 0.0));
        write("cpu 200 0 200 1000 0 0 0 0", 3000, 600);
        let second = sampler.sample(start + Duration::from_secs(2));
        assert_eq!(second.cpu, 50);
        assert_eq!((second.net_rx, second.net_tx), (1000.0, 250.0));
        assert_eq!(second.ram_percent, 50);
        write("cpu 300 0 300 1100 0 0 0 0", 100, 50);
        assert_eq!(sampler.sample(start + Duration::from_secs(4)).net_rx, 0.0);
    }

    #[test]
    fn the_real_system_reports_plausible_numbers() {
        let mut sampler = Sampler::system();
        let usage = sampler.sample(Instant::now());
        assert!(usage.ram_percent > 0 && usage.ram_percent <= 100);
        assert!(usage.disk.percent <= 100);
    }
}
