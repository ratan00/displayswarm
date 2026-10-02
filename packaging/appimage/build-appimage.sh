#!/bin/sh
# Builds DisplaySwarm-<version>-x86_64.AppImage from host/target/release.
# Build on the OLDEST distro you want to support (CI uses Ubuntu 22.04): the bundle carries
# its own ffmpeg/x264/opus/pipewire client libs but uses the host's glibc, GPU and Wayland/X11 libs.
#   packaging/appimage/build-appimage.sh [version]
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root="$here/../.."
ver="${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/host/Cargo.toml" | head -1)}"
rel="$root/host/target/release"
work="$here/build"
app="$work/DisplaySwarm.AppDir"
rm -rf "$work"; mkdir -p "$app/usr/bin" "$app/usr/lib"

install -m755 "$rel/displayswarm" "$rel/displayswarm-hostd" "$app/usr/bin/"
install -m644 "$here/../displayswarm.desktop" "$app/displayswarm.desktop"
install -m644 "$root/NOTICE" "$app/NOTICE"
# AppImage needs an icon: a plain placeholder until the project has one.
printf '<svg xmlns="http://www.w3.org/2000/svg" width="256" height="256"><rect width="256" height="256" rx="40" fill="#1e1e2e"/><rect x="48" y="64" width="160" height="112" rx="10" fill="#89b4fa"/></svg>' > "$app/video-display.svg"

# Bundle the shared libraries the binaries need, except the ones that must match the host system.
skip='^(linux-vdso|ld-linux|libc|libm|libdl|libpthread|librt|libresolv|libgcc_s|libstdc\+\+|libGL|libEGL|libgbm|libdrm|libva|libX11|libxcb|libwayland|libxkbcommon|libfontconfig|libfreetype|libharfbuzz|libudev|libvulkan|libasound)'
for b in displayswarm displayswarm-hostd; do
    ldd "$rel/$b" | awk '/=> \//{print $1, $3}' | while read -r name path; do
        echo "$name" | grep -Eq "$skip" || cp -nL "$path" "$app/usr/lib/"
    done
done

cat > "$app/AppRun" <<'RUN'
#!/bin/sh
here=$(dirname "$(readlink -f "$0")")
export LD_LIBRARY_PATH="$here/usr/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
# `displayswarm-hostd` runs the daemon directly; anything else starts the UI.
case "${1:-}" in
    --hostd) shift; exec "$here/usr/bin/displayswarm-hostd" "$@" ;;
    *) exec "$here/usr/bin/displayswarm" "$@" ;;
esac
RUN
chmod +x "$app/AppRun"

tool="${APPIMAGETOOL:-$(command -v appimagetool || true)}"
if [ -z "$tool" ]; then
    tool="$work/appimagetool"
    curl -sSfL -o "$tool" https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage
    chmod +x "$tool"
fi
ARCH=x86_64 APPIMAGE_EXTRACT_AND_RUN=1 "$tool" "$app" "$root/DisplaySwarm-${ver}-x86_64.AppImage"
echo "built $root/DisplaySwarm-${ver}-x86_64.AppImage"
