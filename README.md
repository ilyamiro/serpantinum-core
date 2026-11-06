# serpantinum-core

A daemon that serves system state and clipboard history to the
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
