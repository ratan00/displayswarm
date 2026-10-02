#!/bin/sh
# Installs the DisplaySwarm binaries and the systemd user service.
#
#   host/dist/install.sh [--enable] [--release-dir DIR] [--prefix DIR]
#
# Copies `displayswarm` and `displayswarm-hostd` (from target/release, or --release-dir)
# to ~/.local/bin, installs the user unit and a desktop entry for the UI.
# --enable also enables the service (start on login) and starts it now.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
bindir_src="$here/../target/release"
prefix="${HOME}/.local"
enable=0
while [ $# -gt 0 ]; do
    case "$1" in
        --enable) enable=1 ;;
        --release-dir) bindir_src="$2"; shift ;;
        --prefix) prefix="$2"; shift ;;
        *) echo "unknown option $1" >&2; exit 2 ;;
    esac
    shift
done

for b in displayswarm displayswarm-hostd; do
    [ -x "$bindir_src/$b" ] || { echo "missing $bindir_src/$b (run: cargo build --release)" >&2; exit 1; }
done

mkdir -p "$prefix/bin" "$HOME/.config/systemd/user" "$prefix/share/applications"
install -m 755 "$bindir_src/displayswarm" "$bindir_src/displayswarm-hostd" "$prefix/bin/"
sed "s#^ExecStart=.*#ExecStart=$prefix/bin/displayswarm-hostd#" "$here/displayswarm-hostd.service" \
    > "$HOME/.config/systemd/user/displayswarm-hostd.service"
cat > "$prefix/share/applications/displayswarm.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=DisplaySwarm
Comment=Use your phone as a display and input device
Exec=$prefix/bin/displayswarm
Icon=video-display
Categories=Utility;
Terminal=false
EOF

systemctl --user daemon-reload
if [ "$enable" = 1 ]; then
    systemctl --user enable --now displayswarm-hostd.service
    echo "Enabled: the daemon starts at login and is running now."
else
    echo "Installed. Start on login with: systemctl --user enable --now displayswarm-hostd.service"
    echo "(or use the switch in the DisplaySwarm window)."
fi
