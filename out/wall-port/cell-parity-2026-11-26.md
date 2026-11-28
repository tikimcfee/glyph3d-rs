# Cell renderer visual parity pass vs. the browser wall (2026-11-26)

Visual and pixel-level audit of `native/src/wall/` (`WallRenderer`, `wall_cells.wgsl`, `glyph-font`) against the browser wall (`~/localdev/viz-web/glyph3d-js/packages/glyph3d-core/` and `wall/`) on real program captures recorded through a scratch `g3d-termd` daemon (`crates/termd-client/examples/termd_record.rs`) at the wall's default **`100×30`** terminal size (plus `80×24` for `term-test-pattern.vt`).

## Fixtures captured and zooms inspected

| Fixture | Size | Real program captured | Zooms inspected (`--wall-replay --zoom`) |
| --- | --- | --- | --- |
| `native/fixtures/wall-replay/zsh-prompt.vt` | `100×30` | `/bin/zsh` with Oh My Zsh `agnoster` powerline git-status segments (`U+E0B0`, `U+E0A0`), `git status -sb`, `ls --color` | `0.22`, `0.55`, `1.0`, `2.2` |
| `native/fixtures/wall-replay/vim-rust.vt` | `100×30` | `/usr/bin/vim --clean` (`habamax`, `termguicolors`, `t_ZH`/`t_ZR` italic doc comments, bold keywords/types, line numbers, `cursorline`, statusline) on `native/src/wall/cell.rs` | `0.22`, `0.55`, `1.0`, `2.2` |
| `native/fixtures/wall-replay/top-live.vt` | `100×30` | Interactive macOS `/usr/bin/top -s 1 -n 18 -o cpu` (alt-screen, inverse `%CPU` column header, bold/underlined headers, live process table) | `0.22`, `0.55`, `1.0`, `2.2` |
| `native/fixtures/wall-replay/git-diff.vt` | `100×30` | `git --no-pager diff --color=always` (bold file headers, cyan `@@` hunk headers, ANSI red `-` deletions, ANSI green `+` additions) | `0.22`, `0.55`, `1.0`, `2.2` |
| `native/fixtures/wall-replay/man-page.vt` | `100×30` | `/usr/bin/man ls \| /usr/bin/ul -t xterm-256color` (real SGR 1 bold headings/flags and SGR 4 underlined arguments/dots) | `0.22`, `0.55`, `1.0`, `2.2` |
| `native/fixtures/wall-replay/term-test-pattern.vt` | `80×24` | Browser wall's full visual test pattern (shades, quads, eighths, box drawing, braille, sextants, wedges, powerline, emoji on fills, all underline/strike/overline/dim/inverse/bold/italic styles, combining marks) | `0.22`, `0.55`, `1.0`, `2.2` |

*Agent TUI note*: `claude` is not installed on this host, and launching interactive `agy` connects to a live backend, so no agent TUI session was captured to avoid touching a live backend.

---

## Renderer faults found and fixed (with before/after PNG pairs)

### 1. Shade sprites (`░ ▒ ▓`, legacy half-shades) blended linearly on `Rgba8UnormSrgb` — washed out and banded

- **Symptom**: On the `shades ░▒▓█` strip of `term-test-pattern.vt` (foreground `#cccccc` over panel `#0a0a1e`), `░` (25%) rendered at `rgb(109, 109, 112)` — brighter than `▒` (50%) is supposed to be (`107`) — compressing `░ → ▒ → ▓ → █` into light-grey bands with a huge jump from `#0a0a1e` to `░`.
- **Root cause**: `wall_cells.wgsl` (`fs_ink`) multiplied `cov *= f32(in.glyph_arg) / 255.0` and emitted premultiplied linear alpha onto the `Rgba8UnormSrgb` target. Linear 25% blending of `srgb(0.8) = 0.604` lands at `0.153` linear → `sRGB 0.427` (`109/255`), whereas JS WebGL2 (`glyphMaterial.js:37-39, 379`) blends in sRGB canvas space (`mix(10, 204, 0.25) = 58`).
- **Fix** (`native/src/shaders/wall_cells.wgsl`, `vs_ink`): When `mode == 3u && sprite_arg < 255u && frame.target_is_srgb != 0u`, mix `fg_rgb` toward `shade_bg` (`cell_bg`, or `resolved_cursor_color()` when under a solid block cursor) in sRGB by `f32(sprite_arg) / 255.0` and pass `sprite_arg = 255u` to `fs_ink`, matching our SGR 2 `DIM` sRGB mix and JS WebGL2.
- **Measured pixels (`term-test-pattern-z1.0.png`, row 0 shades over `#0a0a1e`)**:
  - Expected JS sRGB steps: `(58, 58, 74)` · `(107, 107, 117)` · `(156, 156, 160)` · `(204, 204, 204)`
  - **Before**: `(109, 109, 112)` (+51) · `(150, 150, 151)` (+43) · `(179, 179, 180)` (+23) · `(204, 204, 204)`
  - **After**: `(59, 59, 74)` · `(107, 107, 117)` · `(155, 155, 160)` · `(204, 204, 204)` (within ±1 LSB)
- **Before / After**:
  - Before: `out/wall-port/parity-2026-11-26/shades-emoji-before-z1.0.png`
  - After: `out/wall-port/parity-2026-11-26/shades-emoji-after-z1.0.png`

### 2. Colour emoji squashed horizontally (`136×128` → `1×1`) and pressed edge-to-edge vertically

- **Symptom**: In `term-test-pattern-z2.2.png` (`😀 emoji 🚀 fill 🎉`), `🚀` reached within 2 px of both the top and bottom edges of the 76 px-tall `#005f87` fill (`top_gap = 2 px`, `bot_gap = 2 px`), crowding adjacent rows, and `136×128` (`1.0625`) Noto CBDT bitmaps were mapped into a `1.0×1.0` square (6.25% horizontal squash).
- **Root cause**: JS `EmojiRasterWorker.js:50-52` rasterizes emoji centred at `0.82 × cellPx` inside each square atlas cell (leaving ~9% vertical margin), whereas `assets/atlas/emoji-sheet.bin` holds raw `136×128` Noto bitmaps drawn edge-to-edge, and `wall_cells.wgsl` (`fs_ink`, `mode == 1u`) sampled them across the full `0..1` cell height into a square width.
- **Fix** (`native/src/shaders/wall_cells.wgsl`, `fs_ink`): Centre emoji both horizontally and vertically at `emoji_h = 0.84` of cell height and `emoji_w = emoji_h * (emoji_cell.x / emoji_cell.y)` (`0.8925` of cell height), preserving exact `136×128` aspect ratio and scaling the explicit `textureSampleGrad` Jacobian accordingly.
- **Measured vertical clearance (`term-test-pattern-z2.2.png`, 76 px blue fill `y = 727..802`)**:
  - **Before**: smiley `top_gap = 4 px`, `bot_gap = 6 px`; rocket `top_gap = 2 px`, `bot_gap = 2 px`
  - **After**: smiley `top_gap = 9 px`, `bot_gap = 11 px`; rocket `top_gap = 8 px`, `bot_gap = 8 px`
- **Before / After**:
  - Before: `out/wall-port/parity-2026-11-26/emoji-closeup-before-z2.2.png`
  - After: `out/wall-port/parity-2026-11-26/emoji-closeup-after-z2.2.png`

Both fixes are pinned by Metal GPU pixel test `wall::renderer::tests::gpu_shade_sprites_srgb_ramp_and_emoji_aspect_vertical_centering`.

---

## Full 10-point parity checklist against the browser wall

| Area | Status | Evidence & comparison against `glyph3d-js` |
| --- | --- | --- |
| **1. Baseline & line-height** | **Match** | Both default to `Monaspace Neon` (`lineGap: false`, `upem = 2000`, `asc = 1890`, `desc = -400`, `em = 2290`, `baseline_y = 0.1747`, `LINE_PITCH = 1.0`, `LETTER_SPACING = 0.0`). Note: `GlyphSource::cell_metrics` uses exact `advance_fu / em_height = 0.5240` (snapped to 10 bits) rather than JS `deriveCharSize`'s `Math.ceil(0.6 * 64) / (1.145 * 64) = 0.5322` (+1.5% integer-px ceiling at 64 px); `WORLD_CELL_HEIGHT = 2.4` matches JS scale 2 × 1.2. |
| **2. Box-drawing joins** | **Match** | `glyph-font/src/sprites.rs` mirrors `cellSprites.js` with cell-clamped fractional winding (`is_sprite` in `wall_cells.wgsl`). Rounded boxes, heavy/light mixed tables, double-line junctions, braille, sextants, and wedges show zero seams at `0.22×`, `0.55×`, `1.0×`, and `2.2×`. |
| **3. Powerline separators** | **Match** | `zsh-prompt` (`U+E0B0` blue→yellow→bg, `U+E0A0` branch glyph from `MesloLGS`) and `term-test-pattern` (`U+E0B0..E0BF` rounded/slant/back/thin dividers) measure **0 dark seam pixels** across the 61 px-tall join at `2.2×`. |
| **4. Underline thickness & position** | **Match** | Read from `MonaspaceNeonVar.ttf` `post`/`OS/2` (`ul_y = 0.1127`, `thickness = 0.0314`, `strike_y = 0.3777`) and folded with `glyphMaterial.js::foldLines` (`t = max(uLineA.y, fw.y)`). Single (`man-page`), double, curly, dotted, dashed, strike, and overline all hold ≥1 px crispness at `0.55×` and match JS placement at `1.0×`/`2.2×`. |
| **5. Bold weight** | **Match** | Variable-font instance `wght = 700, slnt = 0` (`glyph-font/src/families.rs`, matching `families.js`). Verified distinct on `vim-rust` (`pub`, `const`, `u32`), `git-diff` headers, `man-page` (`NAME`, `ls`, `-@`), and `term-test-pattern`. `bold_is_bright` defaults to `false` (matching `BOLD_IS_BRIGHT_DEFAULT`). |
| **6. Italic slant** | **Match** | Variable-font instance `wght = 400/700, slnt = -11` (`families.rs`). Verified on real Vim doc comments (`vim-rust-z1.0.png`, `vim-rust-z2.2.png`) and `term-test-pattern` (`italic`, `bold italic`, `italic underline`) with zero right-edge clipping. |
| **7. Emoji size & vertical centring** | **Fixed** | Fixed in `wall_cells.wgsl` (`emoji_h = 0.84`, true `136×128` aspect ratio, centred vertically; see Fault #2 above). |
| **8. Cursor shape** | **Match** | Focused solid block fills behind ink (`Z_OFFSET_CURSOR_BACK`) and draws glyph in `cell_bg` (Ghostty cursor-text default, verified on `zsh-prompt`, `vim-rust`, `top-live`, `git-diff`, `man-page`, `term-test-pattern`); unfocused hollow outline, bar, underline, blink, OSC 12 `cursor_color`, wide trailing-half snap, and hidden cursor in scrollback all verified. |
| **9. Blank-cell backgrounds leaking** | **Match** | Pixel audit across `vim-rust` (`#1c1c1c` explicit cell fills + `#303030` cursorline + `#a8a8a8` statusline) measured **0 `#0a0a1e` seam leaks** at `0.55×`, `1.0×`, and `2.2×`. Blank cells with `bg.a == 0` collapse offscreen in `vs_fill`. |
| **10. Colour banding & shades** | **Fixed** | `wall_cells.wgsl` uses exact IEC 61966-2-1 sRGB EOTF on `Rgba8UnormSrgb` (0-LSB dark-palette crush), sRGB-mixed SGR 2 `DIM`, and now sRGB-mixed shade sprites `░ ▒ ▓` (see Fault #1 above). Far-zoom greek tier (`0.22×`, `< 8 px`) holds continuous word bars without shimmer. |

## Reference `1.0×` captures saved beside this report

- `out/wall-port/parity-2026-11-26/zsh-prompt-z1.0.png`
- `out/wall-port/parity-2026-11-26/vim-rust-z1.0.png`
- `out/wall-port/parity-2026-11-26/top-live-z1.0.png`
- `out/wall-port/parity-2026-11-26/git-diff-z1.0.png`
- `out/wall-port/parity-2026-11-26/man-page-z1.0.png`
