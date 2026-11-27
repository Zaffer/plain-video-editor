#!/usr/bin/env bash
# Builds the editor and installs it for your user, with no root:
#
#   ./install.sh               build, then install (or update) it
#   ./install.sh --uninstall   remove what this installed
#
# A program in the applications menu is three files in standard places: the
# programs, a desktop entry (the menu item and "Open With"), and the logo.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"

id=io.github.zaffer.PlainVideoEditor
bin="$HOME/.local/bin"
apps="$HOME/.local/share/applications"
icons="$HOME/.local/share/icons/hicolor"

refresh() {
    update-desktop-database -q "$apps" 2>/dev/null || true
    gtk-update-icon-cache -q -t "$icons" 2>/dev/null || true
}

if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$bin/pve-gui" "$bin/pve" "$apps/$id.desktop" "$icons/scalable/apps/$id.svg"
    refresh
    echo "Removed Plain Video Editor."
    exit 0
fi

for tool in ffmpeg ffprobe; do
    command -v "$tool" >/dev/null || echo "Note: $tool is not installed; export needs it (sudo apt install ffmpeg)." >&2
done

. ./dev-env.sh                  # does nothing when the GTK and mpv -dev packages are installed
cargo build --release --workspace

install -Dm755 -t "$bin" target/release/pve-gui target/release/pve
install -Dm644 -t "$icons/scalable/apps" "data/$id.svg"
# The menu does not always see ~/.local/bin on the PATH, so the entry names the program in full.
mkdir -p "$apps"
sed "s|^Exec=pve-gui|Exec=$bin/pve-gui|" "data/$id.desktop" > "$apps/$id.desktop"
refresh

echo "Installed Plain Video Editor: it is in the applications menu, and under Open With for videos."
echo "Run ./install.sh again after pulling changes to update it; ./install.sh --uninstall removes it."
