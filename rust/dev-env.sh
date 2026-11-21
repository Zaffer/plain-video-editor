# Source this before building the window (pve-gtk) when the GTK 4 and mpv
# development packages are NOT installed:   . rust/dev-env.sh
#
# The proper way is:  sudo apt install libgtk-4-dev libmpv-dev   (then this file is not needed).
#
# Without them, this downloads the development packages (no root needed), keeps
# only their pkg-config files and library links under rust/target/sysroot, and
# points pkg-config there. Nothing is installed on the system.
_here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
_sr="$_here/target/sysroot"
if ! pkg-config --exists gtk4 mpv 2>/dev/null && [ ! -d "$_sr/usr" ]; then
    echo "building $_sr ..." >&2
    _tmp="$(mktemp -d)"
    ( cd "$_tmp" && for p in $(apt-cache depends --recurse --no-recommends --no-suggests --no-conflicts --no-breaks \
          --no-replaces --no-enhances --no-pre-depends libgtk-4-dev libmpv-dev | grep -E '^[a-z0-9].*-dev$|^wayland-protocols$' | sort -u); do
          dpkg -s "$p" >/dev/null 2>&1 || echo "$p"; done | xargs apt-get download >/dev/null )
    mkdir -p "$_sr"
    for d in "$_tmp"/*.deb; do dpkg-deb -x "$d" "$_sr"; done
    rm -rf "$_tmp"
    find "$_sr" -type f ! -name '*.pc' ! -name '*.so*' -delete
    find "$_sr" -type l | while read -r l; do          # dev links point at runtime libraries that live in /usr/lib
        if [ ! -e "$l" ]; then
            b="$(basename "$(readlink "$l")")"
            for s in /usr/lib/x86_64-linux-gnu /lib/x86_64-linux-gnu; do [ -e "$s/$b" ] && ln -sf "$s/$b" "$l" && break; done
            [ -e "$l" ] || rm -f "$l"
        fi
    done
    find "$_sr" -type d -empty -delete
    find "$_sr" -name '*.pc' -exec sed -i "s|^prefix=/usr|prefix=$_sr/usr|; s|=/usr/|=$_sr/usr/|g" {} +
fi
if [ -d "$_sr/usr" ]; then
    export PKG_CONFIG_PATH="$_sr/usr/lib/x86_64-linux-gnu/pkgconfig:$_sr/usr/share/pkgconfig:$_sr/usr/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
fi
unset _here _sr _tmp
