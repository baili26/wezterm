#!/bin/sh
# Source this before building: `. scripts/dev-env.sh`
#
# This machine (Linux Mint 22.3) is missing a few -dev packages that wezterm
# needs to link against (libxkbcommon-dev, libwayland-dev, libx11-xcb-dev,
# the xcb-util family, ...).  Rather than requiring root, `scripts/fetch-dev-
# deps.sh` downloads those .deb files and unpacks them into
# ~/.local/wezterm-sysroot; this script points pkg-config, the C compiler and
# the linker at that directory.
#
# Everything here is additive: the real system libraries are still found via
# the usual search paths.

WEZTERM_SYSROOT="${WEZTERM_SYSROOT:-$HOME/.local/wezterm-sysroot}"

if [ ! -d "$WEZTERM_SYSROOT" ]; then
    echo "warning: $WEZTERM_SYSROOT not found; run scripts/fetch-dev-deps.sh first" >&2
    return 1 2>/dev/null || exit 1
fi

SYS_PC="$WEZTERM_SYSROOT/usr/lib/x86_64-linux-gnu/pkgconfig:$WEZTERM_SYSROOT/usr/share/pkgconfig"
if [ -n "${PKG_CONFIG_PATH:-}" ]; then
    export PKG_CONFIG_PATH="$SYS_PC:$PKG_CONFIG_PATH"
else
    export PKG_CONFIG_PATH="$SYS_PC"
fi

SYS_INC="-I$WEZTERM_SYSROOT/usr/include"
export CFLAGS="${CFLAGS:-} $SYS_INC"
export CXXFLAGS="${CXXFLAGS:-} $SYS_INC"

SYS_LIB="$WEZTERM_SYSROOT/usr/lib/x86_64-linux-gnu"
export LIBRARY_PATH="$SYS_LIB${LIBRARY_PATH:+:$LIBRARY_PATH}"
export RUSTFLAGS="${RUSTFLAGS:-} -L $SYS_LIB"
export LD_LIBRARY_PATH="$SYS_LIB${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

# rustup lives outside the default PATH on this machine.
case ":$PATH:" in
    *":$HOME/.cargo/bin:"*) ;;
    *) export PATH="$HOME/.cargo/bin:$PATH" ;;
esac

echo "wezterm dev env: sysroot=$WEZTERM_SYSROOT"
