//! Caps Lock and Num Lock, from the keyboard LEDs in /sys/class/leds. Reading them needs no
//! permissions and works on every compositor, but the kernel sends no change events for them, so
//! they are polled while someone watches.

use std::path::Path;

pub const LEDS: &str = "/sys/class/leds";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Locks {
    pub caps: bool,
    pub num: bool,
}

/// Whether any LED whose name contains `pattern` is lit
fn lit(root: &Path, pattern: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(root) else { return false };
    entries.flatten().any(|entry| {
        entry.file_name().to_string_lossy().contains(pattern)
            && std::fs::read_to_string(entry.path().join("brightness"))
                .ok()
                .and_then(|text| text.trim().parse::<u64>().ok())
                .is_some_and(|value| value > 0)
    })
}

pub fn read(root: &Path) -> Locks {
    Locks { caps: lit(root, "capslock"), num: lit(root, "numlock") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn led(root: &Path, name: &str, value: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("brightness"), value).unwrap();
    }

    #[test]
    fn reads_the_led_that_matches_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        led(dir.path(), "input3::capslock", "1\n");
        led(dir.path(), "input3::numlock", "0\n");
        led(dir.path(), "input3::scrolllock", "1\n");
        assert_eq!(read(dir.path()), Locks { caps: true, num: false });
    }

    #[test]
    fn any_keyboard_with_the_led_lit_counts() {
        let dir = tempfile::tempdir().unwrap();
        led(dir.path(), "input3::numlock", "0\n");
        led(dir.path(), "input9::numlock", "1\n");
        assert!(read(dir.path()).num);
    }

    #[test]
    fn missing_root_means_off() {
        assert_eq!(read(Path::new("/nonexistent-serpantinum-leds")), Locks::default());
    }
}
