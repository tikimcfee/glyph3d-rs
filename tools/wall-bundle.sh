#!/usr/bin/env bash
# Build `target/bundle/Glyph3D Wall.app` for macOS from the release binaries.
#
# Usage:
#   tools/wall-bundle.sh [--no-build] [--icon PATH] [--out DIR]
#
# Produces (all inside target/, never touching /Applications or LaunchServices):
#   target/bundle/Glyph3D Wall.app/
#     Contents/Info.plist
#     Contents/PkgInfo
#     Contents/MacOS/glyph3d-native
#     Contents/MacOS/wallctl
#     Contents/Resources/AppIcon.icns
#     Contents/Resources/atlas/emoji-sheet.bin
#     Contents/Resources/fonts/*.ttf + *-LICENSE.txt
#   target/bundle/glyph3d-wall.png   (512x512 PNG preview / Linux XDG icon)
#
# Replacing the procedural icon with custom artwork:
#   Place a 1024x1024 RGBA PNG at `assets/icon/glyph3d-wall.png`, or pass
#   `--icon /path/to/icon-1024.png` when running this script.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DO_BUILD=1
ICON_PNG=""
OUT_DIR="$REPO_ROOT/target/bundle"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --no-build)
            DO_BUILD=0
            shift
            ;;
        --icon)
            ICON_PNG="${2:?--icon requires a PNG path}"
            shift 2
            ;;
        --out)
            OUT_DIR="${2:?--out requires a directory path}"
            shift 2
            ;;
        -h|--help)
            sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "error: unknown argument '$1' (see --help)" >&2
            exit 1
            ;;
    esac
done

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "error: tools/wall-bundle.sh builds a macOS .app bundle and requires Darwin (sips, iconutil, codesign)" >&2
    exit 1
fi

if [[ -z "${DEVELOPER_DIR:-}" ]] && ! xcode-select -p >/dev/null 2>&1 && [[ -d /Library/Developer/CommandLineTools ]]; then
    export DEVELOPER_DIR=/Library/Developer/CommandLineTools
    export PATH="/Library/Developer/CommandLineTools/usr/bin:$PATH"
fi

if [[ "$DO_BUILD" -eq 1 ]]; then
    echo "==> Building release binaries (glyph3d-native, wallctl)..."
    (cd "$REPO_ROOT" && cargo build --release -p glyph3d-native -p wall-control)
fi

WALL_BIN="$REPO_ROOT/target/release/glyph3d-native"
WALLCTL_BIN="$REPO_ROOT/target/release/wallctl"
for req in "$WALL_BIN" "$WALLCTL_BIN"; do
    if [[ ! -x "$req" ]]; then
        echo "error: missing executable $req (run without --no-build first)" >&2
        exit 1
    fi
done

VERSION="$(
    python3 -c '
import pathlib, re, sys
text = pathlib.Path(sys.argv[1]).read_text()
m = re.search(r"^version\s*=\s*\"([^\"]+)\"", text, re.M)
if not m:
    raise SystemExit("could not read version from native/Cargo.toml")
print(m.group(1))
' "$REPO_ROOT/native/Cargo.toml"
)"

OTOOL_BIN="/Library/Developer/CommandLineTools/usr/bin/otool"
if [[ ! -x "$OTOOL_BIN" ]]; then
    OTOOL_BIN="otool"
fi
MIN_MACOS="$(
    "$OTOOL_BIN" -l "$WALL_BIN" 2>/dev/null \
        | awk '/LC_BUILD_VERSION/{found=1} found && $1=="minos"{print $2; exit}' \
        || true
)"
if [[ -z "$MIN_MACOS" ]]; then
    MIN_MACOS="11.0"
fi

APP_NAME="Glyph3D Wall"
APP_DIR="$OUT_DIR/$APP_NAME.app"
CONTENTS="$APP_DIR/Contents"
MACOS_DIR="$CONTENTS/MacOS"
RESOURCES_DIR="$CONTENTS/Resources"

rm -rf "$APP_DIR"
mkdir -p "$MACOS_DIR" "$RESOURCES_DIR/atlas" "$RESOURCES_DIR/fonts"

echo "==> Staging executables into $MACOS_DIR..."
cp "$WALL_BIN" "$MACOS_DIR/glyph3d-native"
cp "$WALLCTL_BIN" "$MACOS_DIR/wallctl"
chmod 0755 "$MACOS_DIR/glyph3d-native" "$MACOS_DIR/wallctl"

echo "==> Staging runtime resources (emoji sheet + fonts) into $RESOURCES_DIR..."
cp "$REPO_ROOT/assets/atlas/emoji-sheet.bin" "$RESOURCES_DIR/atlas/emoji-sheet.bin"
cp "$REPO_ROOT/assets/fonts/"*.ttf "$RESOURCES_DIR/fonts/"
cp "$REPO_ROOT/assets/fonts/"*-LICENSE.txt "$RESOURCES_DIR/fonts/"

WORK_TMP="$(mktemp -d "${TMPDIR:-/tmp}/glyph3d-bundle.XXXXXX")"
trap 'rm -rf "$WORK_TMP"' EXIT

MASTER_PNG="$WORK_TMP/icon-1024.png"
DEFAULT_CUSTOM_ICON="$REPO_ROOT/assets/icon/glyph3d-wall.png"
if [[ -n "$ICON_PNG" ]]; then
    if [[ ! -f "$ICON_PNG" ]]; then
        echo "error: --icon '$ICON_PNG' does not exist" >&2
        exit 1
    fi
    echo "==> Using explicit icon artwork: $ICON_PNG"
    /usr/bin/sips -s format png -z 1024 1024 "$ICON_PNG" --out "$MASTER_PNG" >/dev/null
elif [[ -f "$DEFAULT_CUSTOM_ICON" ]]; then
    echo "==> Using repository icon artwork: $DEFAULT_CUSTOM_ICON"
    /usr/bin/sips -s format png -z 1024 1024 "$DEFAULT_CUSTOM_ICON" --out "$MASTER_PNG" >/dev/null
else
    echo "==> Generating procedural icon from offscreen wall chrome pattern..."
    RAW_PATTERN="$WORK_TMP/wall-chrome-pattern.png"
    if "$WALL_BIN" --wall-chrome-pattern --screenshot "$RAW_PATTERN" >/dev/null 2>&1 && [[ -f "$RAW_PATTERN" ]]; then
        python3 "$REPO_ROOT/tools/gen_wall_icon.py" --source-png "$RAW_PATTERN" --size 1024 --out "$MASTER_PNG"
    else
        echo "    (offscreen GPU render unavailable; using procedural vector-grid fallback)"
        python3 "$REPO_ROOT/tools/gen_wall_icon.py" --size 1024 --out "$MASTER_PNG"
    fi
fi

ICONSET="$WORK_TMP/AppIcon.iconset"
mkdir -p "$ICONSET"
for pair in \
    "16:icon_16x16.png" \
    "32:icon_16x16@2x.png" \
    "32:icon_32x32.png" \
    "64:icon_32x32@2x.png" \
    "128:icon_128x128.png" \
    "256:icon_128x128@2x.png" \
    "256:icon_256x256.png" \
    "512:icon_256x256@2x.png" \
    "512:icon_512x512.png" \
    "1024:icon_512x512@2x.png"
do
    px="${pair%%:*}"
    name="${pair#*:}"
    /usr/bin/sips -s format png -z "$px" "$px" "$MASTER_PNG" --out "$ICONSET/$name" >/dev/null
done
/usr/bin/iconutil -c icns "$ICONSET" -o "$RESOURCES_DIR/AppIcon.icns"
cp "$ICONSET/icon_256x256@2x.png" "$OUT_DIR/glyph3d-wall.png"

printf 'APPL????' > "$CONTENTS/PkgInfo"

cat > "$CONTENTS/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleDisplayName</key>
    <string>Glyph3D Wall</string>
    <key>CFBundleExecutable</key>
    <string>glyph3d-native</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundleIdentifier</key>
    <string>dev.glyph3d.wall</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>Glyph3D Wall</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>${VERSION}</string>
    <key>CFBundleSignature</key>
    <string>????</string>
    <key>CFBundleVersion</key>
    <string>${VERSION}</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.developer-tools</string>
    <key>LSMinimumSystemVersion</key>
    <string>${MIN_MACOS}</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSSupportsAutomaticGraphicsSwitching</key>
    <true/>
</dict>
</plist>
EOF

/usr/bin/plutil -lint "$CONTENTS/Info.plist" >/dev/null

echo "==> Ad-hoc codesigning bundle..."
/usr/bin/codesign --force --sign - "$MACOS_DIR/wallctl"
/usr/bin/codesign --force --sign - "$MACOS_DIR/glyph3d-native"
/usr/bin/codesign --force --sign - "$APP_DIR"
/usr/bin/codesign --verify --deep --strict "$APP_DIR"

echo "==> Built: $APP_DIR (version $VERSION, macOS >= $MIN_MACOS)"
echo "    Icon PNG: $OUT_DIR/glyph3d-wall.png"
