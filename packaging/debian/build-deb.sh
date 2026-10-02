#!/bin/sh
# Builds displayswarm_<version>_amd64.deb from host/target/release (run `cargo build --release`
# in host/ first, on the Debian/Ubuntu release you are targeting: the ffmpeg soname is baked in).
#   packaging/debian/build-deb.sh [version]
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root="$here/../.."
ver="${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/host/Cargo.toml" | head -1)}"
rel="$root/host/target/release"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

install -Dm755 "$rel/displayswarm" "$rel/displayswarm-hostd" -t "$stage/usr/bin/"
install -Dm644 "$here/../displayswarm-hostd.service" -t "$stage/usr/lib/systemd/user/"
install -Dm644 "$here/../displayswarm.desktop" -t "$stage/usr/share/applications/"
install -Dm644 "$root/NOTICE" "$stage/usr/share/doc/displayswarm/NOTICE"

# Dependencies come from the binaries' real shared-library needs.
deps=$(cd "$stage" && dpkg-shlibdeps -O -e usr/bin/displayswarm -e usr/bin/displayswarm-hostd 2>/dev/null \
    | sed 's/^shlibs:Depends=//' || true)
mkdir -p "$stage/DEBIAN"
cat > "$stage/DEBIAN/control" <<CTL
Package: displayswarm
Version: $ver
Section: utils
Priority: optional
Architecture: amd64
Depends: ${deps:-libavcodec-dev, libx264-dev, libpipewire-0.3-0, libopus0}
Recommends: pipewire, xdg-desktop-portal
Maintainer: ARATAN <abhishekrat123@gmail.com>
Homepage: https://github.com/ratan00/DisplaySwarm
Description: Use an Android phone as a second display and input device
 Host for the DisplaySwarm Android client: virtual monitor, touch/pen input,
 audio and clipboard over USB or Wi-Fi. Enable the background service with
 systemctl --user enable --now displayswarm-hostd.
CTL
dpkg-deb --root-owner-group --build "$stage" "$root/displayswarm_${ver}_amd64.deb"
echo "built $root/displayswarm_${ver}_amd64.deb"
