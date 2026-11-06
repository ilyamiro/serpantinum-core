//! One-shot questions about the system: whether it can hibernate, and whether the lock screen's
//! PAM stack asks for a fingerprint.

use std::path::{Path, PathBuf};

/// Whether logind would hibernate; without logind, whether the kernel can and has a resume device
pub fn can_hibernate() -> bool {
    let answer = zbus::blocking::Connection::system().and_then(|bus| {
        bus.call_method(
            Some("org.freedesktop.login1"),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            "CanHibernate",
            &(),
        )?
        .body()
        .deserialize::<String>()
    });
    match answer {
        Ok(answer) => matches!(answer.as_str(), "yes" | "challenge"),
        Err(_) => kernel_can_hibernate(Path::new("/")),
    }
}

fn kernel_can_hibernate(root: &Path) -> bool {
    let read = |path: &str| std::fs::read_to_string(root.join(path)).ok();
    read("sys/power/state").is_some_and(|s| s.split_whitespace().any(|w| w == "disk"))
        && read("proc/swaps").is_some_and(|s| s.lines().count() > 1)
        && read("sys/power/resume").is_some_and(|s| s.trim() != "0:0")
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|d| d.join(program).is_file()))
}

/// Whether a PAM file, or one it includes for auth, loads pam_fprintd
fn scan(dir: &Path, file: &Path, depth: u32) -> bool {
    if depth > 5 {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(file) else { return false };
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or_default();
        if line.contains("pam_fprintd") {
            return true;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        let target = match words.as_slice() {
            ["auth" | "-auth", "include" | "substack", target, ..] => *target,
            ["@include", target, ..] => *target,
            _ => continue,
        };
        let target = if target.starts_with('/') { PathBuf::from(target) } else { dir.join(target) };
        if scan(dir, &target, depth + 1) {
            return true;
        }
    }
    false
}

/// Whether fingerprint unlock can work: fprintd is installed and the lock screen's PAM uses it
pub fn fingerprint_available(pam_dir: &Path) -> bool {
    if !on_path("fprintd-list") && !on_path("fprintd-enroll") {
        return false;
    }
    let own = pam_dir.join("serpantinum");
    let config = if own.exists() { own } else { pam_dir.join("login") };
    scan(pam_dir, &config, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pam_includes_are_followed() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::write(d.join("login"), "# pam_fprintd in a comment\nauth include system-auth\n").unwrap();
        std::fs::write(d.join("system-auth"), "auth sufficient pam_unix.so\n@include fingerprint\n").unwrap();
        std::fs::write(d.join("fingerprint"), "auth sufficient pam_fprintd.so\n").unwrap();
        assert!(scan(d, &d.join("login"), 0));
        std::fs::write(d.join("fingerprint"), "auth sufficient pam_unix.so\n").unwrap();
        assert!(!scan(d, &d.join("login"), 0));
        std::fs::write(d.join("loop"), "auth include loop\n").unwrap();
        assert!(!scan(d, &d.join("loop"), 0));
    }

    #[test]
    fn the_kernel_needs_disk_swap_and_resume() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::create_dir_all(d.join("sys/power")).unwrap();
        std::fs::create_dir_all(d.join("proc")).unwrap();
        std::fs::write(d.join("sys/power/state"), "freeze mem disk\n").unwrap();
        std::fs::write(d.join("proc/swaps"), "Filename\n/dev/sda2 partition\n").unwrap();
        std::fs::write(d.join("sys/power/resume"), "8:2\n").unwrap();
        assert!(kernel_can_hibernate(d));
        std::fs::write(d.join("sys/power/resume"), "0:0\n").unwrap();
        assert!(!kernel_can_hibernate(d));
    }
}
