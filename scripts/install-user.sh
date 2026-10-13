#!/usr/bin/env bash
# Install uur into ~/.local: binary, hook DLLs, icon and an application
# menu entry.  Clicking "UU Remote" in the launcher then runs the whole
# stack; no PATH edits are needed (the desktop entry uses absolute paths).
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
readonly root
prefix="${UUR_INSTALL_PREFIX:-$HOME/.local}"
data_home="${XDG_DATA_HOME:-$HOME/.local/share}"
[[ "$prefix" = /* && "$data_home" = /* && "$prefix" != *$'\n'* && "$data_home" != *$'\n'* ]] || {
    printf 'install prefix and XDG_DATA_HOME must be absolute paths without newlines\n' >&2
    exit 1
}

[[ -x "$root/target/release/uur" ]] || {
    printf 'missing %s — run: cargo build --release\n' \
        "$root/target/release/uur" >&2
    exit 1
}
components=(wevtapi.dll uur-hook.dll wtsapi32.dll winlogon.exe uur-terminal-proxy.exe uur-mux-proxy.exe uur-launch-proxy.exe)
for component in "${components[@]}"; do
    [[ -f "$root/build/hook/$component" ]] || {
        printf 'missing hook component %s — run: ./hook/build.sh\n' "$component" >&2
        exit 1
    }
done
[[ -f "$root/capture/uur-pw-capture" && -f "$root/capture/uur-wl-capture" ]] || {
    printf 'missing capture helper — run: ./capture/build.sh\n' >&2
    exit 1
}

install -Dm0755 "$root/target/release/uur" "$prefix/bin/uur"
for helper in uur-pw-capture uur-wl-capture; do
    install -Dm0755 "$root/capture/$helper" "$prefix/lib/uur/$helper"
done
for dll in "${components[@]}"; do
    install -Dm0644 "$root/build/hook/$dll" "$prefix/lib/uur/hook/$dll"
done

# Menu entry: absolute Exec path so the user's PATH is irrelevant.
install -Dm0644 "$root/packaging/uur.desktop" \
    "$prefix/share/applications/uur.desktop"
# Escape Desktop Entry quoted arguments, then escape the sed replacement.
desktop_binary="$prefix/bin/uur"
desktop_binary="${desktop_binary//\\/\\\\}"
desktop_binary="${desktop_binary//\"/\\\"}"
desktop_binary="${desktop_binary//\$/\\\$}"
desktop_binary="${desktop_binary//\`/\\\`}"
sed_binary="${desktop_binary//\\/\\\\}"
sed_binary="${sed_binary//&/\\&}"
sed_binary="${sed_binary//|/\\|}"
try_binary="$prefix/bin/uur"
try_binary="${try_binary//\\/\\\\}"
try_binary="${try_binary//&/\\&}"
try_binary="${try_binary//|/\\|}"
sed -i "s|^Exec=uur |Exec=\"$sed_binary\" |; s|^TryExec=uur$|TryExec=$try_binary|" \
    "$prefix/share/applications/uur.desktop"
if [[ -f "$root/packaging/uur-icon.png" ]]; then
    install -Dm0644 "$root/packaging/uur-icon.png" \
        "$prefix/share/icons/hicolor/256x256/apps/uur.png"
fi

# Only the source installer writes this receipt. Package managers own their
# system files; uur uninstall must not remove those behind their backs.
printf 'uur-user-install-v1\n%s/uur\n' "${data_home%/}" > "$prefix/lib/uur/user-install"

printf 'installed:\n  %s/bin/uur\n  %s/lib/uur/\n  %s/share/applications/uur.desktop\n' \
    "$prefix" "$prefix" "$prefix"
UUR_COMMAND="$prefix/bin/uur" "$root/packaging/post-install-message"
