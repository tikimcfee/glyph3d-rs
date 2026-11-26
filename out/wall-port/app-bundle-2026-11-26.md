# Glyph3D Wall Application Bundle (`Glyph3D Wall.app` & Linux XDG) — 2026-11-26

## 1. What the Bundle Contains

`tools/wall-bundle.sh` builds a self-contained macOS application bundle inside
`target/bundle/` (never writing outside `target/`, notarizing, or registering
with LaunchServices):

```text
target/bundle/
├── Glyph3D Wall.app/
│   └── Contents/
│       ├── Info.plist
│       ├── PkgInfo                          ("APPL????")
│       ├── MacOS/
│       │   ├── glyph3d-native               (release main binary, ad-hoc signed)
│       │   └── wallctl                      (CLI verb client beside it, ad-hoc signed)
│       └── Resources/
│           ├── AppIcon.icns                 (16..1024 px 1x/2x iconset from sips + iconutil)
│           ├── atlas/
│           │   └── emoji-sheet.bin          (G3ES v1 sparse colour-emoji archive, ~10 MB)
│           └── fonts/
│               ├── Cousine-{Regular,Bold,Italic,BoldItalic}.ttf + Cousine-LICENSE.txt
│               ├── DejaVuSans.ttf + DejaVuSans-LICENSE.txt
│               ├── MesloLGS-NF-Mono.ttf + MesloLGS-NF-Mono-LICENSE.txt
│               ├── MonaspaceNeonVar.ttf
│               └── MonaspaceArgonVar.ttf + Monaspace-LICENSE.txt
└── glyph3d-wall.png                         (512x512 RGBA PNG preview / Linux icon)
```

### `Info.plist` Keys

- `CFBundleName` / `CFBundleDisplayName`: `"Glyph3D Wall"` (sets the bold top-left macOS menu-bar application title, Dock tooltip, and `Cmd+Tab` label, complementing `NSProcessInfo.processName` and the window title `"glyph3d — the wall"`).
- `CFBundleIdentifier`: `"dev.glyph3d.wall"` (stable bundle identity for TCC permissions and macOS window state).
- `CFBundleShortVersionString` / `CFBundleVersion`: read directly from `native/Cargo.toml` (`0.1.0`).
- `LSMinimumSystemVersion`: read from the Mach-O `LC_BUILD_VERSION` `minos` load command on `target/release/glyph3d-native` (fallback `11.0`).
- `LSApplicationCategoryType`: `"public.app-category.developer-tools"`.
- `NSHighResolutionCapable`: `true` (Retina backing surfaces) and `NSSupportsAutomaticGraphicsSwitching`: `true`.
- Signed ad-hoc (`codesign --force --sign -`) on `wallctl`, `glyph3d-native`, and the outer `.app`, verified with `codesign --verify --deep --strict`.

### Procedural Icon & How to Replace It

No hand-drawn artwork is committed yet. During `tools/wall-bundle.sh`:
1. If `--icon /path/to/icon.png` is passed, or if `assets/icon/glyph3d-wall.png` exists in the checkout, `sips` normalizes that `1024x1024` PNG and `iconutil` packs it directly into `Contents/Resources/AppIcon.icns`.
2. Otherwise, `glyph3d-native --wall-test-pattern --screenshot <tmp> --zoom 2.4` renders the wall's drafting grid, focused `#6ee7a0` terminal frame, title bar, and Slug glyphs offscreen; `tools/gen_wall_icon.py` (stdlib-only Python) crops a square detail window and masks it into an Apple HIG-proportioned `1024x1024` squircle with a soft shadow and mint inner rim (falling back to a procedural two-terminal vector tile when run headless without a GPU). `sips` produces all ten `16x16`..`512x512@2x` PNGs and `iconutil -c icns` builds `AppIcon.icns`.

**To replace the procedural icon permanently:** drop a `1024x1024` RGBA PNG at `assets/icon/glyph3d-wall.png` (or pass `--icon PATH` to `tools/wall-bundle.sh`) and re-run `tools/wall-bundle.sh`.

---

## 2. Runtime File Lookup & Every File the Wall Reads

### Single-Source Asset Lookup Order (`native/src/lib.rs`)

`crate::resolve_runtime_asset(explicit, rel)` resolves runtime resources in one place:
1. **Explicit CLI flag** (`--emoji-sheet PATH`) when provided.
2. **Installed bundle / prefix resources** relative to `std::env::current_exe()` when present on disk:
   - macOS `.app`: `<bundle>/Contents/MacOS/../Resources/<rel>`
   - Linux / Unix prefix: `<prefix>/bin/../share/glyph3d-wall/<rel>`
3. **Repository checkout fallback**: `<crate>/../assets/<rel>`.

Both `resolve_emoji_sheet` / `default_emoji_sheet` (`atlas/emoji-sheet.bin`) and `wall::glyphs::default_fonts_dir` / `wall::ui::wall_ui_font_path` (`fonts/...`) route through `resolve_runtime_asset`, so an installed `.app` (or Linux `~/.local` install) needs no repository checkout on disk.

### Complete Inventory of Files Read or Opened by the Wall at Run Time

| Category | Path(s) | Notes |
| :--- | :--- | :--- |
| **Compiled into binary** | `config/defaults.toml`, `native/src/shaders/{wall_cells,wall_paper,wall_shapes}.wgsl` | Embedded via `include_str!`; zero disk reads at run time unless dev `GLYPH_WALL_SHADER_DIR` is set. |
| **Colour-emoji sheet** | `Contents/Resources/atlas/emoji-sheet.bin` (10 MB) | Lazy `G3ES` v1 table + compressed PNG blob (`EmojiSheetArchive::load`); does **not** read `curves.bin`, `glyphmap.bin`, `glyphs.bin`, `codepoints.bin`, or the 361 MB `emoji-sheet.cache`. |
| **Runtime Slug fonts (8 TTFs)** | `Contents/Resources/fonts/`: `Cousine-{Regular,Bold,Italic,BoldItalic}.ttf`, `MesloLGS-NF-Mono.ttf`, `DejaVuSans.ttf`, `MonaspaceNeonVar.ttf`, `MonaspaceArgonVar.ttf` | Loaded by `GlyphSource::new` (`glyph-font`). `WallUi` (`egui`) also loads `Cousine-Regular.ttf`, `MesloLGS-NF-Mono.ttf`, and `DejaVuSans.ttf` from the same resolved `fonts/` directory. |
| **Optional launch config** | `--launch-config PATH`, else `./launch_config.toml` or `../launch_config.toml` | Checked relative to CWD (`/` under Finder, where neither exists, so compiled defaults apply). |
| **Saved wall state (`WallStore`)** | macOS: `~/Library/Application Support/glyph3d/wall/wall-<ns>.json`<br>Linux: `${XDG_STATE_HOME:-~/.local/state}/glyph3d/wall/wall-<ns>.json`<br>Scratch: `$G3D_TERMD_DIR/wall-<ns>.json` | Created `0700` dir / `0600` file; holds terminal positions, groups/splits, panel placements, Agent View pairings, and dialled config knobs. |
| **Live Unix sockets & log** | `termd-<ns>.sock` and `wall-<ns>.sock` under `$G3D_TERMD_DIR`, else `/run/user/<uid>/g3d` (Linux) or `/tmp/g3d-<uid>` (macOS); log at `${XDG_STATE_HOME:-~/.local/state}/g3d/termd-<ns>.log` | `0700` parent directory and same-UID peer credential check (`getpeereid` / `SO_PEERCRED`). |
| **Helper executables** | `g3d-termd` (daemon), `wallctl` (CLI verb client), `agent` (gomuxai CLI) | `g3d-termd` started on demand by `termd-client`; `wallctl` shipped in `Contents/MacOS/wallctl`; `agent` discovered on `PATH` or `~/.local/bin/agent`. |

---

## 3. Finder Environment: Window Side vs `g3d-termd` Side

When macOS Finder launches `Glyph3D Wall.app`, the process has no controlling TTY, CWD `/`, and a minimal `PATH` (`/usr/bin:/bin:/usr/sbin:/sbin` — omitting `~/.local/bin`, `~/.cargo/bin`, `/opt/homebrew/bin`, and `/usr/local/bin`).

### Fixed on the Window / `termd-client` Side

1. **Daemon discovery (`termd_client::paths::find_termd`)**: searches in order:
   - `$G3D_TERMD_BIN` (if set; fails immediately if missing or non-executable)
   - Beside `current_exe()` (`Contents/MacOS/g3d-termd`)
   - `$PATH`
   - `$HOME/.local/bin/g3d-termd` and `$HOME/.cargo/bin/g3d-termd`
   - `/opt/homebrew/bin/g3d-termd`, `/usr/local/bin/g3d-termd`, `/usr/bin/g3d-termd`
   Checking Unix execute bits (`mode & 0o111 != 0`) so a non-executable file cannot shadow a valid binary.
2. **Baseline `PATH` when starting `g3d-termd` (`termd_client::client::scrub_spawn_env`)**: if `PATH` is unset or empty in the parent process (e.g. `env -i HOME=... USER=... TMPDIR=...`), `termd-client` seeds `PATH=/usr/bin:/bin:/usr/sbin:/sbin` on the `g3d-termd --daemon` child while scrubbing `G3D_TERMD_SOCKET`, `G3D_TERMD_ID`, and `G3D_RELAY_*`.
3. **Clean `Create` requests (`term_session::Sessions::spawn`)**: the wall sends `Create { argv, cols, rows, cwd: None, env: None, env_base: None, .. }`, intentionally **not** forwarding the Finder window's `/` working directory or minimal environment as `env_base`.

### What `g3d-termd` Does (Verified Against `g3d-termd/src/env.rs` & Live `g3d env`)

- **Working directory (`cwd` / `PWD`)**: when `create.cwd` is `None`, `g3d-termd` resolves the user's home directory from `getpwuid` (`account.home`, e.g. `/Users/lugo`), never inheriting the daemon's own `/` CWD.
- **Session & login environment (`--env-source auto`, `--login-shell true`, `--fill-locale true`)**:
  1. Queries the desktop session source (`launchd` on macOS for `SSH_AUTH_SOCK`, `TMPDIR`, etc.; `systemd --user` on Linux for `WAYLAND_DISPLAY`, `DISPLAY`, `XDG_*`, `SSH_AUTH_SOCK`, etc.).
  2. Scrubs launcher/multiplexer/agent vars (`__CFBundleIdentifier`, `XPC_SERVICE_NAME`, `G3D_TERMD_*`, `AGENT_*`, `CLAUDE*`, `TMUX`, etc.).
  3. Fills missing `HOME`, `USER`, `LOGNAME`, and `SHELL` from `passwd`, seeds `PATH=/usr/bin:/bin` (`fill (login)`), and fills `LANG` from macOS `AppleLocale` (`en_US.UTF-8`) or `locale -a`.
  4. Applies optional `~/.config/g3d/termd.toml` `[env] set` and `termd`'s own `TERM=xterm-256color`, `COLORTERM=truecolor`, `TERM_PROGRAM=g3d-termd`, `G3D_TERMD_ID`, `G3D_TERMD_SOCKET`, and `PWD`.
  5. Starts the user's shell as a **login shell** (`argv[0] = "-zsh"` / `"-bash"` / `"-fish"`), which runs `/etc/zprofile` (`path_helper` on macOS) and `~/.zprofile` / `~/.zshrc`, producing the exact same login `PATH` and environment as terminals opened by the browser wall.
- **One `g3d-termd` nuance to note**: when a client sends an explicit non-shell `create.argv` (`Some(["my-tool", ...])`) with `env_base: None`, `g3d-termd` resolves `argv[0]` before any login shell profile runs (against `launchd`/`systemd --user`'s `PATH`, else `/usr/bin:/bin`, plus `termd.toml`). Standard wall terminals (`wall.spawn`, splits) send `argv: None` and run `-<shell>`, so they get the full login profile. If `gomux.attach` later spawns `agent` directly via `create.argv`, `agent` not on `launchd`'s `PATH` should either be invoked by resolved path (`agent_feed::find_agent_binary`) or have `~/.local/bin` configured in `termd.toml` / `launchctl config user path`.

---

## 4. How to Install It

### macOS

1. Build the bundle into `target/bundle/Glyph3D Wall.app`:
   ```sh
   tools/wall-bundle.sh
   ```
2. Ensure `g3d-termd` is installed (e.g. `~/.local/bin/g3d-termd` or `/opt/homebrew/bin/g3d-termd` — or copy `g3d-termd` into `Glyph3D Wall.app/Contents/MacOS/g3d-termd` for a single-folder bundle).
3. Copy the `.app` to `~/Applications` or `/Applications`:
   ```sh
   cp -R "target/bundle/Glyph3D Wall.app" ~/Applications/
   ```
4. *(Optional)* Put the bundled `wallctl` on your shell `PATH`:
   ```sh
   ln -sf "$HOME/Applications/Glyph3D Wall.app/Contents/MacOS/wallctl" ~/.local/bin/wallctl
   ```
   *(Note: because the bundle is ad-hoc signed with `codesign -s -` rather than notarized with an Apple Developer ID, copying it across machines over AirDrop/quarantine requires `xattr -dr com.apple.quarantine "Glyph3D Wall.app"` on the receiving Mac.)*

### Linux (XDG User Install)

 Desktop entry: `tools/linux/glyph3d-wall.desktop`; step-by-step commands: `tools/linux/INSTALL.md`. In short:
- Binaries -> `~/.local/bin/{glyph3d-native,wallctl}` (with `g3d-termd` in `~/.local/bin` or on `PATH`)
- Runtime assets -> `~/.local/share/glyph3d-wall/{atlas/emoji-sheet.bin,fonts/*.ttf}` (auto-discovered relative to `~/.local/bin/glyph3d-native`)
- PNG icons -> `~/.local/share/icons/hicolor/{256x256,512x512}/apps/glyph3d-wall.png` (generated via `python3 tools/gen_wall_icon.py`)
- Desktop entry -> `~/.local/share/applications/glyph3d-wall.desktop`

---

## 5. What Is Still Missing to Replace the Browser Wall for Daily Use

*(Audited against `wall` at `34a6ed6`, which already mounts the topbar, hovered title-bar buttons, six chrome panels, single-owner namespace gate, and `termd` status updates.)*

1. **Agent View & live gomuxai `agent-feed` integration:**
   - **Agent View cards & badges (`panels::agent_view` + `wall_core::agent_views::AgentViews`)**: both the state coordinator and the egui card/badge renderer exist and pass unit/GPU tests, but are not yet mounted alongside terminals in `mount.rs` / `app.rs`.
   - **gomuxai `agent-feed` polling**: `crates/agent-feed` reads `agent fleet --json`, `agent sessions --json`, and `agent transcript`, but `mount.rs` still passes `BusSnapshot::default()` to the palette and sidebar, so live agents do not yet appear in the sidebar/palette or auto-pair with Agent Views.
2. **Live renderer knob updates (`WallEffect::ConfigKnobChanged`):**
   - Changing `fontFamily`, `panelOpacity`, `greekPx`, `lineThickness`, `underlineRaise`, or `cursorBlinkMs` in Settings updates `Wall` and persists to `wall-<ns>.json`, but `app.rs` currently logs `ConfigKnobChanged` without applying the updated values onto `WallRenderer` until restart.
3. **Terminal text & desktop integration gaps:**
   - **Scrollback-spanning selection copy & `OSC 52`**: `app.rs` passes `screen_lookup` over the visible `front_screen` (`term_cells::Screen`), so copying a selection that scrolled back into history does not yet query `Sessions::text` / `block_text`, and `OSC 52` clipboard sequences from programs inside terminals are not forwarded to the OS clipboard.
   - **IME preedit display & URL opening**: `WindowEvent::Ime` preedit text is not drawn over the cursor cell for CJK/dead-key composition, and `Cmd+click` / `Ctrl+click` on URLs (`OSC 8` or plain-text links) is not implemented.
   - **macOS menu bar shortcuts & distribution signing**: `winit` provides only the default app menu (`About`, `Hide`, `Quit`), without `Shell` / `Edit` / `View` / `Window` menu items (`Cmd+N`, `Cmd+D`, `Cmd+F`, `Minimize`), custom hand-drawn artwork (replacing the procedural icon), or Developer ID notarization for Gatekeeper on other Macs.
