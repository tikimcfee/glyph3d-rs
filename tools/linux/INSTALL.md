# Installing Glyph3D Wall on Linux (XDG User Install)

Nothing is installed outside `target/` by the build. To install `Glyph3D Wall` into
your user XDG directories (`~/.local`) so application launchers (KDE Kickoff/KRunner,
GNOME Overview, rofi, fuzzel) show it with its icon and launch it without a repo
checkout:

```sh
# 1. Build the release binaries
cargo build --release -p glyph3d-native -p wall-control

# 2. Install executables into ~/.local/bin (ensure g3d-termd is also in ~/.local/bin or on PATH)
install -Dm755 target/release/glyph3d-native ~/.local/bin/glyph3d-native
install -Dm755 target/release/wallctl        ~/.local/bin/wallctl

# 3. Install runtime assets into ~/.local/share/glyph3d-wall/
#    (`resolve_runtime_asset` discovers `../share/glyph3d-wall/` relative to `<prefix>/bin/glyph3d-native`)
install -Dm644 assets/atlas/emoji-sheet.bin \
    ~/.local/share/glyph3d-wall/atlas/emoji-sheet.bin
install -d ~/.local/share/glyph3d-wall/fonts
install -m644 assets/fonts/*.ttf assets/fonts/*-LICENSE.txt \
    ~/.local/share/glyph3d-wall/fonts/

# 4. Generate and install the PNG application icon into the hicolor icon theme
python3 tools/gen_wall_icon.py --size 512 \
    --out ~/.local/share/icons/hicolor/512x512/apps/glyph3d-wall.png
python3 tools/gen_wall_icon.py --size 256 \
    --out ~/.local/share/icons/hicolor/256x256/apps/glyph3d-wall.png

# 5. Install the desktop entry (rewrite Exec/TryExec to ~/.local/bin/glyph3d-native if
#    ~/.local/bin is not on your display manager's session PATH)
sed "s|=glyph3d-native|=$HOME/.local/bin/glyph3d-native|g" \
    tools/linux/glyph3d-wall.desktop \
    > ~/.local/share/applications/glyph3d-wall.desktop
chmod 0644 ~/.local/share/applications/glyph3d-wall.desktop

# Optional validation & desktop database refresh
command -v desktop-file-validate >/dev/null && \
    desktop-file-validate ~/.local/share/applications/glyph3d-wall.desktop
command -v update-desktop-database >/dev/null && \
    update-desktop-database ~/.local/share/applications
```

## Runtime XDG Paths Used on Linux

| Purpose | Path |
| :--- | :--- |
| Binaries | `~/.local/bin/glyph3d-native`, `~/.local/bin/wallctl`, `~/.local/bin/g3d-termd` |
| Bundled runtime assets | `~/.local/share/glyph3d-wall/{atlas/emoji-sheet.bin,fonts/*.ttf}` |
| Desktop entry | `${XDG_DATA_HOME:-~/.local/share}/applications/glyph3d-wall.desktop` |
| Application icon (PNG) | `${XDG_DATA_HOME:-~/.local/share}/icons/hicolor/{256x256,512x512}/apps/glyph3d-wall.png` |
| Saved wall layout & config | `${XDG_STATE_HOME:-~/.local/state}/glyph3d/wall/wall-<ns>.json` (`0700` dir, `0600` file) |
| Live sockets (`termd` & `wallctl`) | `/run/user/<uid>/g3d/{termd,wall}-<ns>.sock` (fallback `/tmp/g3d-<uid>/`) |
| Daemon log | `${XDG_STATE_HOME:-~/.local/state}/g3d/termd-<ns>.log` |
