#!/usr/bin/env bash
# Build the PipeWire capture helper (host C, links libpipewire).
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
readonly root
out="$root/capture"
cc="${CC:-gcc}"
flags=(-std=c11 -O2 -Wall -Wextra -Werror)

command -v "$cc" >/dev/null 2>&1 || { printf 'missing tool: %s\n' "$cc" >&2; exit 1; }

"$cc" "${flags[@]}" -o "$out/uur-pw-capture" \
    "$root/capture/uur-pw-capture.c" \
    $(pkg-config --cflags --libs libpipewire-0.3)

printf 'built: %s\n' "$out/uur-pw-capture"

generated="$root/build/capture"
mkdir -p "$generated"
wayland-scanner client-header "$root/capture/wlr-screencopy-unstable-v1.xml" \
    "$generated/wlr-screencopy-unstable-v1-client-protocol.h"
wayland-scanner private-code "$root/capture/wlr-screencopy-unstable-v1.xml" \
    "$generated/wlr-screencopy-unstable-v1-protocol.c"
"$cc" "${flags[@]}" -I"$generated" -o "$out/uur-wl-capture" \
    "$root/capture/uur-wl-capture.c" \
    "$generated/wlr-screencopy-unstable-v1-protocol.c" \
    $(pkg-config --cflags --libs wayland-client)
printf 'built: %s\n' "$out/uur-wl-capture"
