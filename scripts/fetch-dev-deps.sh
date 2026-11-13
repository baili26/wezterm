#!/bin/sh
# Download the -dev packages needed to build wezterm and unpack them into
# ~/.local/wezterm-sysroot, without requiring root.
#
# Normally you'd just run `./get-deps` (which uses apt and needs sudo).  If
# you don't have a passwordless sudo, this script achieves the same thing by
# fetching the .deb files as your user with `apt-get download` and unpacking
# them with `dpkg -x`.
#
# After running this, source scripts/dev-env.sh before building.
set -eu

SYSROOT="${WEZTERM_SYSROOT:-$HOME/.local/wezterm-sysroot}"
CACHE="${WEZTERM_DEB_CACHE:-$SYSROOT/debs}"

PACKAGES="
libegl1-mesa-dev
libx11-xcb-dev
libxcb-ewmh-dev
libxcb-icccm4-dev
libxcb-image0-dev
libxcb-keysyms1-dev
libxcb-randr0-dev
libxcb-render0-dev
libxcb-util-dev
libxcb-xkb-dev
libxkbcommon-dev
libxkbcommon-x11-dev
libwayland-dev
"

mkdir -p "$CACHE" "$SYSROOT"

cd "$CACHE"
for pkg in $PACKAGES; do
    if ls "${pkg}"_*.deb >/dev/null 2>&1; then
        echo "have  $pkg"
    else
        echo "fetch $pkg"
        apt-get download "$pkg" >/dev/null
    fi
done

for deb in *.deb; do
    dpkg -x "$deb" "$SYSROOT"
done

# The .deb symlinks (e.g. libwayland-client.so -> libwayland-client.so.0) are
# dangling because the runtime libraries live on the real system, not in the
# sysroot.  Re-point them at the system copies so the linker can resolve them.
cd "$SYSROOT/usr/lib/x86_64-linux-gnu"
for so in *.so; do
    if [ -L "$so" ] && [ ! -e "$so" ]; then
        base=$(readlink "$so")
        real=$(ls /usr/lib/x86_64-linux-gnu/"$base"* 2>/dev/null | head -1 || true)
        if [ -n "$real" ]; then
            ln -sf "$real" "$so"
            echo "relink $so -> $real"
        else
            echo "warning: no system copy of $base for $so" >&2
        fi
    fi
done

echo
echo "sysroot ready: $SYSROOT"
echo "now run: . scripts/dev-env.sh"
