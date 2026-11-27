#!/usr/bin/env bash
# Install the wall where a person opens it from: one command, one button.
#
# Usage:
#   tools/wall-install.sh [--no-build] [--uninstall]
#
# What it installs:
#   macOS   $G3D_APP_DIR/Glyph3D Wall.app      Spotlight, Launchpad, the Dock
#           (G3D_APP_DIR defaults to ~/Applications)
#   Linux   $G3D_PREFIX/bin/glyph3d-native     the binary, with its fonts and
#           $G3D_PREFIX/share/glyph3d-wall/    emoji sheet beside it, a desktop
#           $G3D_PREFIX/share/applications/    entry and its icon
#           (G3D_PREFIX defaults to ~/.local)
#   both    g3d-wall    open the wall from a shell
#           wallctl     drive a running wall (`wallctl wall.spawn`)
#           in $G3D_BIN_DIR (default ~/.local/bin, where glyph3d-js's `make`
#           puts g3d-termd; on Linux it is $G3D_PREFIX/bin)
#
# g3d-wall:
#   g3d-wall                  open the wall (namespace rs-wall); on macOS a
#                             wall already open comes to the front instead
#   g3d-wall ARGS...          a new window with these arguments, e.g.
#                             g3d-wall --wall-extra-ns agents
#   g3d-wall --fg ARGS...     stay attached to this shell: its environment,
#                             its directory, the wall's output here
#   g3d-wall --installed      which build is installed, and where
#
# The install is a copy: it does not need this checkout afterwards. Files are
# built beside their destination and renamed over it, so a wall that is open
# keeps running the build it started with. g3d-termd is NOT installed here:
# the wall starts the one glyph3d-js installed (termd-client's `find_termd`).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DO_BUILD=1
UNINSTALL=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --no-build) DO_BUILD=0; shift ;;
        --uninstall) UNINSTALL=1; shift ;;
        -h|--help)
            sed -n '2,31p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "error: unknown argument '$1' (see --help)" >&2
            exit 1
            ;;
    esac
done

OS="$(uname -s)"
APP_NAME="Glyph3D Wall"
case "$OS" in
    Darwin)
        APP_DIR="${G3D_APP_DIR:-$HOME/Applications}"
        BIN_DIR="${G3D_BIN_DIR:-$HOME/.local/bin}"
        ;;
    Linux)
        PREFIX="${G3D_PREFIX:-$HOME/.local}"
        BIN_DIR="$PREFIX/bin"
        SHARE_DIR="$PREFIX/share/glyph3d-wall"
        DESKTOP_FILE="$PREFIX/share/applications/glyph3d-wall.desktop"
        ICON_ROOT="$PREFIX/share/icons/hicolor"
        ;;
    *)
        echo "error: tools/wall-install.sh knows macOS and Linux, not $OS" >&2
        exit 1
        ;;
esac

if [[ "$UNINSTALL" -eq 1 ]]; then
    rm -f "$BIN_DIR/g3d-wall" "$BIN_DIR/wallctl"
    if [[ "$OS" == Darwin ]]; then
        rm -rf "$APP_DIR/$APP_NAME.app"
    else
        rm -f "$BIN_DIR/glyph3d-native" "$DESKTOP_FILE"
        rm -f "$ICON_ROOT/256x256/apps/glyph3d-wall.png" "$ICON_ROOT/512x512/apps/glyph3d-wall.png"
        rm -rf "$SHARE_DIR"
    fi
    echo "==> Uninstalled. Saved layouts and running daemons are left alone."
    exit 0
fi

# One line saying what was installed: the commit, whether the tree was clean,
# and when. `g3d-wall --installed` prints it.
STAMP="$(git -C "$REPO_ROOT" describe --always --dirty 2>/dev/null || echo unknown)"
STAMP="$STAMP $(git -C "$REPO_ROOT" rev-parse --abbrev-ref HEAD 2>/dev/null || echo '?') $(date '+%Y-%m-%d %H:%M')"

# Write a file beside its destination, then rename it over: a running program
# keeps the file it opened.
install_over() {
    local mode="$1" src="$2" dst="$3"
    mkdir -p "$(dirname "$dst")"
    install -m "$mode" "$src" "$dst.new"
    mv -f "$dst.new" "$dst"
}

write_over() {
    local mode="$1" dst="$2"
    mkdir -p "$(dirname "$dst")"
    cat > "$dst.new"
    chmod "$mode" "$dst.new"
    mv -f "$dst.new" "$dst"
}

mkdir -p "$BIN_DIR"

if [[ "$OS" == Darwin ]]; then
    bundle_args=()
    [[ "$DO_BUILD" -eq 0 ]] && bundle_args+=(--no-build)
    "$REPO_ROOT/tools/wall-bundle.sh" ${bundle_args[@]+"${bundle_args[@]}"}
    BUILT="$REPO_ROOT/target/bundle/$APP_NAME.app"
    printf '%s\n' "$STAMP" > "$BUILT/Contents/Resources/BUILD"
    # The stamp is a new file in a signed bundle: seal it again.
    /usr/bin/codesign --force --sign - "$BUILT"

    APP="$APP_DIR/$APP_NAME.app"
    echo "==> Installing $APP..."
    mkdir -p "$APP_DIR"
    rm -rf "$APP_DIR/.$APP_NAME.app.new"
    /usr/bin/ditto "$BUILT" "$APP_DIR/.$APP_NAME.app.new"
    rm -rf "$APP"
    mv "$APP_DIR/.$APP_NAME.app.new" "$APP"
    # Tell LaunchServices now, so Spotlight and `open -a` find this copy.
    LSREG="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"
    [[ -x "$LSREG" ]] && "$LSREG" -f "$APP" || true

    write_over 0755 "$BIN_DIR/g3d-wall" <<EOF
#!/bin/sh
# g3d-wall: open the wall. Written by glyph3d-rs tools/wall-install.sh; see
# its header for the forms.
APP="$APP"
case "\${1:-}" in
    --installed) echo "\$APP"; cat "\$APP/Contents/Resources/BUILD"; exit 0 ;;
    --fg) shift; exec "\$APP/Contents/MacOS/glyph3d-native" "\$@" ;;
    -h|--help) exec "\$APP/Contents/MacOS/glyph3d-native" "\$@" ;;
    "") exec /usr/bin/open -a "\$APP" ;;
    *) exec /usr/bin/open -n -a "\$APP" --args "\$@" ;;
esac
EOF
    ln -sfn "$APP/Contents/MacOS/wallctl" "$BIN_DIR/wallctl"
    WHERE="$APP"
else
    if [[ "$DO_BUILD" -eq 1 ]]; then
        echo "==> Building release binaries (glyph3d-native, wallctl)..."
        (cd "$REPO_ROOT" && cargo build --release -p glyph3d-native -p wall-control)
    fi
    for req in glyph3d-native wallctl; do
        if [[ ! -x "$REPO_ROOT/target/release/$req" ]]; then
            echo "error: missing target/release/$req (run without --no-build first)" >&2
            exit 1
        fi
    done

    echo "==> Installing into $PREFIX..."
    install_over 0755 "$REPO_ROOT/target/release/glyph3d-native" "$BIN_DIR/glyph3d-native"
    install_over 0755 "$REPO_ROOT/target/release/wallctl" "$BIN_DIR/wallctl"
    install_over 0644 "$REPO_ROOT/assets/atlas/emoji-sheet.bin" "$SHARE_DIR/atlas/emoji-sheet.bin"
    for f in "$REPO_ROOT"/assets/fonts/*.ttf "$REPO_ROOT"/assets/fonts/*-LICENSE.txt; do
        install_over 0644 "$f" "$SHARE_DIR/fonts/$(basename "$f")"
    done
    printf '%s\n' "$STAMP" | write_over 0644 "$SHARE_DIR/BUILD"

    for size in 256 512; do
        icon="$ICON_ROOT/${size}x${size}/apps/glyph3d-wall.png"
        mkdir -p "$(dirname "$icon")"
        python3 "$REPO_ROOT/tools/gen_wall_icon.py" --size "$size" --out "$icon.new"
        mv -f "$icon.new" "$icon"
    done

    # The desktop's session PATH need not hold $BIN_DIR: name the binary whole.
    sed "s|=glyph3d-native\$|=$BIN_DIR/glyph3d-native|" \
        "$REPO_ROOT/tools/linux/glyph3d-wall.desktop" | write_over 0644 "$DESKTOP_FILE"
    command -v update-desktop-database >/dev/null 2>&1 \
        && update-desktop-database "$PREFIX/share/applications" >/dev/null 2>&1 || true
    command -v gtk-update-icon-cache >/dev/null 2>&1 \
        && gtk-update-icon-cache -q -t "$ICON_ROOT" >/dev/null 2>&1 || true

    write_over 0755 "$BIN_DIR/g3d-wall" <<EOF
#!/bin/sh
# g3d-wall: open the wall. Written by glyph3d-rs tools/wall-install.sh; see
# its header for the forms.
BIN="$BIN_DIR/glyph3d-native"
case "\${1:-}" in
    --installed) echo "\$BIN"; cat "$SHARE_DIR/BUILD"; exit 0 ;;
    --fg) shift; exec "\$BIN" "\$@" ;;
    -h|--help) exec "\$BIN" "\$@" ;;
esac
LOG="\${XDG_STATE_HOME:-\$HOME/.local/state}/glyph3d/wall.log"
mkdir -p "\$(dirname "\$LOG")"
# Detached from this shell; what the wall prints (a namespace already open,
# say) lands in the log.
if command -v setsid >/dev/null 2>&1; then
    setsid -f "\$BIN" "\$@" >>"\$LOG" 2>&1 </dev/null
else
    nohup "\$BIN" "\$@" >>"\$LOG" 2>&1 </dev/null &
fi
EOF
    WHERE="$DESKTOP_FILE"
fi

echo "==> Installed: $STAMP"
echo "    button:  $WHERE"
echo "    command: $BIN_DIR/g3d-wall   (and $BIN_DIR/wallctl)"

case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) echo "    note: $BIN_DIR is not on this shell's PATH" ;;
esac

# The wall starts g3d-termd itself; say so now if it will not find one.
found_termd=""
for cand in "${G3D_TERMD_BIN:-}" "$BIN_DIR/g3d-termd" "$(command -v g3d-termd 2>/dev/null || true)" \
    "$HOME/.local/bin/g3d-termd" "$HOME/.cargo/bin/g3d-termd" \
    /opt/homebrew/bin/g3d-termd /usr/local/bin/g3d-termd /usr/bin/g3d-termd; do
    if [[ -n "$cand" && -x "$cand" ]]; then found_termd="$cand"; break; fi
done
if [[ -n "$found_termd" ]]; then
    echo "    g3d-termd: $found_termd"
else
    echo "    WARNING: no g3d-termd found; the wall cannot start terminals until"
    echo "             glyph3d-js's \`make\` installs one into $BIN_DIR"
fi
