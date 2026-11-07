# serpantinum-core

A daemon that serves system state, compositor state, brightness, idle inhibition, screen time and
clipboard history to the
[Serpantinum](https://github.com/ilyamiro/serpantinum) shell over D-Bus. The same binary is its
command line client, which prints everything as JSON, so the shell reads it from one long-running
process instead of running scripts on a timer. It works on any compositor and needs only a session
bus. The binary links only glibc.

## Usage

```sh
serpantinum-core start                        # the daemon, in the background
serpantinum-core sysinfo                      # one sample
serpantinum-core watch sysinfo --interval 2   # a sample every 2 seconds until stopped
serpantinum-core watch locks                  # Caps Lock and Num Lock, now and on every change
serpantinum-core watch compositor             # Sway, Niri or Mango workspaces, windows and layout
serpantinum-core watch brightness             # brightness in percent and its backend
serpantinum-core brightness set 40            # also get, raise, lower and backend
serpantinum-core watch idle "$XDG_RUNTIME_DIR/serpantinum/lock/locked"
serpantinum-core screentime track             # count screen time while running
serpantinum-core screentime stats 2026-11-06  # a day's screen time statistics
serpantinum-core launcher rank                # launcher scores from screen time and launches
serpantinum-core launcher files notes         # files for the launcher's file search
serpantinum-core monitors                     # outputs in use with their size and refresh rate
serpantinum-core system can-hibernate
serpantinum-core clipboard list 0 12          # the newest 12 clipboard entries
serpantinum-core clipboard pin 42
serpantinum-core clipboard delete 42
serpantinum-core clipboard wipe
serpantinum-core quit
```

`serpantinum-core --help` lists every command. Every command except `quit` starts the daemon when
it is not running, and `watch` starts it again if it goes away.

Feeds cost nothing while nobody watches them: the daemon samples only while a client holds a lease,
and the lease ends when that client exits. When several clients watch the same feed, it samples at
the shortest interval any of them asked for.

Caps Lock and Num Lock follow the keyboards' LED events when `/dev/input` can be read, as for
members of the `input` group, and nothing runs between changes. Otherwise the LEDs in
`/sys/class/leds` are polled every 250 ms while someone watches, since the kernel signals no changes
there. On Hyprland every change is checked against Hyprland's main keyboard, which ignores LED
flicker from other keyboards.

The compositor feed speaks each compositor's own socket: Sway's i3-ipc, Niri's JSON socket and
Mango's IPC socket, so following them costs no processes. Every watcher passes the variables that
locate its compositor, because the daemon may outlive a login. Of Hyprland, which the shell reads
natively, only the focused window is followed, for screen time.

`monitors` asks the same sockets, and Hyprland's, for the outputs in use. On other compositors it
asks for the Wayland outputs (`wl_output` 4 or later), and without a compositor it lists the
connectors the kernel reports connected.

Brightness drives the first backlight in `/sys/class/backlight`, through logind when its file is
not writable, else the first DDC/CI display through `ddcutil`. `watch idle` serves
`org.freedesktop.ScreenSaver` while it runs, and with `sleep-lock on` on its standard input holds a
logind sleep delay inhibitor until the lock screen's marker file appears. Screen time counts the
focused app once a second into the shell's sqlite database, and notifies when a daily limit from
the shell's settings is reached.

The clipboard commands read the history from [cliphist](https://github.com/sentriz/cliphist), which
must be installed. Decoded images and pins are kept in `--cache-dir`, by default
`$XDG_CACHE_HOME/serpantinum/clipboard`.

## D-Bus

| Object       | Interface                        | Members                                                                                                         |
|--------------|----------------------------------|-----------------------------------------------------------------------------------------------------------------|
| `/`          | `org.serpantinum.Core`           | `Quit()`, `Version`                                                                                             |
| `/sysinfo`   | `org.serpantinum.Core.SysInfo`   | `Subscribe(u interval_ms)`, `Unsubscribe()`, `Cpu`, `RamPercent`, `RamGb`, `Temp`, `NetRx`, `NetTx`, `DiskPercent`, `DiskUsedGb`, `DiskTotalGb`, `Samples` |
| `/locks`     | `org.serpantinum.Core.Locks`     | `Subscribe(u interval_ms)`, `Unsubscribe()`, `CapsLock`, `NumLock`                                              |
| `/clipboard` | `org.serpantinum.Core.Clipboard` | `List(s cache_dir, u offset, u limit) → s`, `TogglePin(s cache_dir, s id)`, `Delete(s cache_dir, s id)`, `Wipe(s cache_dir)` |
| `/compositor` | `org.serpantinum.Core.Compositor` | `Subscribe(a{ss} session)`, `Unsubscribe()`, `Name`, `Ready`, `Workspaces`, `Windows`, `Keyboard` (the last three JSON text) |
| `/brightness` | `org.serpantinum.Core.Brightness` | `Subscribe()`, `Unsubscribe()`, `Get() → i`, `Set(u percent) → i`, `Step(b up) → i`, `Backend`, `Percent`, `Ready` |
| `/idle` | `org.serpantinum.Core.Idle` | `Subscribe(s marker)`, `Unsubscribe()`, `SetSleepLock(b)`, `Inhibited`; signals `Sleep`, `Wake`, `Log(s)` |
| `/screentime` | `org.serpantinum.Core.ScreenTime` | `Subscribe(a{ss} env)`, `Unsubscribe()` |
| `/org/freedesktop/ScreenSaver`, `/ScreenSaver` | `org.freedesktop.ScreenSaver` | `Inhibit`, `UnInhibit`, `SimulateUserActivity`, `GetActive`, while `watch idle` runs |

Properties signal `PropertiesChanged`. `Samples` counts samples, so each sample signals a change
even when the values stay the same.

```sh
busctl --user call org.serpantinum.Core /sysinfo org.serpantinum.Core.SysInfo Subscribe u 1000
busctl --user get-property org.serpantinum.Core /sysinfo org.serpantinum.Core.SysInfo Cpu
```

## Building

```sh
nix-shell        # or any Rust toolchain with edition 2024
cargo build --release
packaging/stage.sh /tmp/stage   # lays out usr/bin, the D-Bus service file and the licence
```

On NixOS, `pkgs.callPackage ./default.nix { }` installs the latest release binary, and
`{ fromSource = true; }` compiles this checkout. Adding the package to `services.dbus.packages`
lets D-Bus start the daemon on first use.

## License

AGPL-3.0-or-later
