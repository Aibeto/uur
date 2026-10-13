#!/usr/bin/env bash
set -euo pipefail

project_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
destination=${1:?usage: packaging/stage.sh DESTDIR [PREFIX]}
prefix=${2-/usr}
uur_binary=${UUR_BINARY:-$project_root/target/release/uur}

install -Dm0755 "$uur_binary" \
    "$destination$prefix/bin/uur"
install -Dm0755 "$project_root/capture/uur-pw-capture" \
    "$destination$prefix/lib/uur/uur-pw-capture"
install -Dm0755 "$project_root/capture/uur-wl-capture" \
    "$destination$prefix/lib/uur/uur-wl-capture"
install -Dm0755 "$project_root/packaging/post-install-message" \
    "$destination$prefix/lib/uur/post-install-message"
for component in wevtapi.dll uur-hook.dll wtsapi32.dll winlogon.exe uur-terminal-proxy.exe uur-mux-proxy.exe uur-launch-proxy.exe; do
    install -Dm0644 "$project_root/build/hook/$component" \
        "$destination$prefix/lib/uur/hook/$component"
done
install -Dm0644 "$project_root/packaging/uur.desktop" \
    "$destination$prefix/share/applications/uur.desktop"
install -Dm0644 "$project_root/packaging/uur-icon.png" \
    "$destination$prefix/share/icons/hicolor/256x256/apps/uur.png"
install -Dm0644 "$project_root/packaging/io.github.panxuc.uur.metainfo.xml" \
    "$destination$prefix/share/metainfo/io.github.panxuc.uur.metainfo.xml"
install -Dm0644 "$project_root/packaging/60-uur-uinput.rules" \
    "$destination$prefix/lib/udev/rules.d/60-uur-uinput.rules"
install -Dm0644 "$project_root/packaging/modules-load.conf" \
    "$destination$prefix/lib/modules-load.d/uur.conf"
install -Dm0644 "$project_root/LICENSE" \
    "$destination$prefix/share/licenses/uur/LICENSE"
install -Dm0644 "$project_root/NOTICE" \
    "$destination$prefix/share/licenses/uur/NOTICE"
install -Dm0644 "$project_root/capture/wlr-screencopy-unstable-v1.xml" \
    "$destination$prefix/share/licenses/uur/wlr-screencopy-unstable-v1.xml"
