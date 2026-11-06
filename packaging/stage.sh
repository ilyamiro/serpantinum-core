#!/bin/sh
# Lay out an installed serpantinum-core under $1 with the prefix /usr, from a release build.
# The release tarball is made from this tree.
set -eu
dest=${1:?usage: stage.sh DEST}
cd "$(dirname "$0")/.."

install -Dm755 target/release/serpantinum-core "$dest/usr/bin/serpantinum-core"
install -Dm644 assets/org.serpantinum.Core.service "$dest/usr/share/dbus-1/services/org.serpantinum.Core.service"
sed -i 's|@out@|/usr|' "$dest/usr/share/dbus-1/services/org.serpantinum.Core.service"
install -Dm644 LICENSE "$dest/usr/share/licenses/serpantinum-core/LICENSE"
