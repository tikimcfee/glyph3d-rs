# Fidelity fault list, before the run-time font switch (2026-11-26)

From the replay agent, who rendered four recorded programs (`native/fixtures/wall-replay/`) through `--wall-replay` and looked at each. The lead confirmed the test-pattern picture (`replay-term-test-pattern-before-fonts.png` beside this file). Kept as the list the renderer is fixed against.

### Visual Fault Report (from inspecting all 4 rendered PNGs)

#### Known pre-runtime-font gaps (noted once)
- **No bold or italic faces**: [cell.rs](../../native/src/wall/cell.rs#L74-L75) discards `Face::from_bits(attrs::face(cell.attrs))`, so all `SGR 1` bold and `SGR 3` italic text renders in regular upright Cousine.
- **No cell-fitted box-drawing / block sprites**: all box-drawing and block characters currently draw from baked font outlines rather than procedural cell-edge sprites — producing 1 px horizontal and vertical hairlines/seams between adjacent `█` (`U+2588`) cells (the white `full`/`rows`/`cols` block, the rainbow `████` bars, and horizontal row splits across the orange block-art mascot), tiny inset/disconnected rounded corners `╭ ╮ ╰ ╯` (`U+256D..U+2570`), corner/junction gaps on `╔═╦═╗` double-line boxes, and vertical arm overshoots that collide across rows on the mixed single/double/heavy showcase (`rows 7–10` of `term-test-pattern.png`).

---

#### Everything else wrong in the pictures

##### 1. [term-test-pattern.png](~/.gemini/antigravity-cli/brain/b7da30ef-842b-498c-844f-014ca4215b3d/scratch/term-test-pattern.png)
- **All combining marks on decomposed grapheme clusters are lost (`row 22`)**:
  - Expected: `marks café café Việt x́ q̈̄ ñ ấ bold é x́ italic é x́ both é x́ ünder x́ inv x́|`
  - Drawn: `marks café cafe Viet x q n a bold e x italic e x both e x under x inv x|`
  - Precomposed single-codepoint `café` (`U+00E9`) draws its accent, while every base + combining mark (`U+0301`, `U+0302`, `U+0303`, `U+0304`, `U+0308`, `U+0323`) drops all marks. `glyph-vt` interns multi-scalar clusters into `Screen::clusters` (`>= CLUSTER_BASE`), and `resolve_cell_glyph` in [cell.rs](../../native/src/wall/cell.rs#L91-L127) probes only the emoji sequence table and falls back to the first covered scalar (`'e'`, `'x'`, `'q'`, `'n'`, `'a'`, `'u'`) — with neither NFC precomposition nor combining-mark positioning.
- **All Symbols for Legacy Computing (`U+1FB00..U+1FBFF`) render completely blank (`rows 13–15`)**:
  - `row 13` after `eighths `: all 20 eighth-blocks (`U+1FB70..U+1FB83`) are empty.
  - `row 14` after `sextants `: all 60 sextants (`U+1FB00..U+1FB3B`) and `U+1FB84..U+1FB8B` are empty.
  - `row 15` after `wedges `: all 52 wedges (`U+1FB3C..U+1FB6F`) are empty.
  - None of `U+1FB00..U+1FBFF` is present in `assets/atlas/codepoints.bin` (`FLAG_MISSING` → slot `0`).
- **Fallback-font metric distortion across the 3-font baked atlas (`rows 2, 13, 15–16`)**:
  - Per [FORMAT.md](../../assets/atlas/FORMAT.md#L49-L63) and [FORMAT.md](../../assets/atlas/FORMAT.md#L181-L196), `assets/atlas/curves.bin` normalizes each glyph's `[0, 1] × [0, 1]` UV box against **its own font's** `advanceFu` and `[descenderFu, ascenderFu]` (`Cousine` `1229 × 2320` baseline `0.265`, `MesloLGS NF Mono` `2584` baseline `0.226`, proportional `DejaVu Sans` `2384` baseline `0.203`), whereas `vs_ink` in [wall_cells.wgsl](../../native/src/shaders/wall_cells.wgsl#L429-L439) maps every outline glyph's `[0, 1]²` directly onto the primary `cell_size`. Visible effects:
    - **Edge eighths `▔▔▁▁▕▏` (`row 2`, cols 72–77)**: `▔` (`U+2594`, upper 1/8) does not sit at the top edge of the cell (it floats near mid-upper cell) and `▕▏` (`U+2595` + `U+258F`) collapses/misaligns instead of kissing cleanly at the cell seam.
    - **Braille (`U+2800..U+28FF`, `row 13`)**: instead of round dots balanced across the full `2×4` cell height, braille renders as tiny flat horizontal dashes compressed into the lower-middle band of the cell.
    - **Powerline (`U+E0B0..U+E0BF`, `rows 15–16`)**: 1 px dark vertical seams appear between coloured pill fills (`main`, `src`, `rounded`, `slant`, `back`) and their filled caps/arrows (`U+E0B0`, `E0B4`, `E0B6`, `E0B8`, `E0BA`, `E0BC`, `E0BE`); on `row 16`, the thin/flame/waveform glyphs (`E0B1..E0BF`) look like ordinary ASCII chevrons/parens/slashes (`> < ) ( \ / / \`) rather than full-height powerline dividers.
- **Curly underline collides with glyph ink, and straight underline on `inverse` leaves a bright gap underneath (`rows 17–21`)**:
  - In [defaults.toml](../../config/defaults.toml#L411-L414) and [renderer.rs](../../native/src/wall/renderer.rs#L233-L241), `underline_y = 0.12`, `line_thickness = 0.05`, `curly_amplitude = 1.5` (`amp = 0.075`). In [wall_cells.wgsl](../../native/src/shaders/wall_cells.wgsl#L588-L595), the curly wave reaches `0.12 + 0.075 + 0.025 = 0.22` of the cell height — touching baseline bowls (`c`, `u`, `l`) on `curly`, `curly red`, `curly yellow`, `bold curly`, `curly  under  spaces` and slicing straight through descenders (`y`, `p`, `g`) with no ink clearance.
  - On `inverse underline` (`row 20`), the straight dark underline spans `y ∈ [0.095, 0.145]`, leaving `y ∈ [0.0, 0.095)` of the bright `#d8dee9` inverse fill exposed as a bright strip underneath the underline.
- **Wide colour emoji (`row 16`)**:
  - `😀`, `🚀`, `🎉` align cleanly over their 2-column coloured fills (`span_cols = 2` in [wall_cells.wgsl](../../native/src/shaders/wall_cells.wgsl#L494-L515)) without clipping or bleeding into adjacent cells, though Noto's bitmap padding makes them appear slightly small vertically inside the row.
- **Cursor colour ignores `Vt` / `Screen` cursor colour (all 4 pictures)**:
  - Every terminal draws its cursor using uniform `[wall] cursor_color` (`#6ee7a0` neon green, [defaults.toml](../../config/defaults.toml#L407) / [renderer.rs](../../native/src/wall/renderer.rs#L115)) instead of `Theme::default().cursor` (`#d8dee9`) or `OSC 12` cursor colour overrides from `glyph-vt`.

##### 2. [ls-color.png](~/.gemini/antigravity-cli/brain/b7da30ef-842b-498c-844f-014ca4215b3d/scratch/ls-color.png)
- Multi-column tab/space layout (`3` columns across `80` cols) lands in exact cell alignment.
- **Low legibility / halation on ANSI Blue (`SGR 34`, palette index 4)**:
  - Directory names (`ast-grep`, `relax-fidelity`, `vendor`) resolve to `rgb(0x1a, 0x1a, 0xe6)` in [lib.rs](../../crates/glyph-vt/src/lib.rs#L65) against the `#0d1017` panel background — pure saturated `#1a1ae6` blue has very low luminance contrast on `#0d1017` (especially at normal zoom) compared to red executables (`#b31a1a`) and `#d8dee9` plain files.

##### 3. [git-log-graph.png](~/.gemini/antigravity-cli/brain/b7da30ef-842b-498c-844f-014ca4215b3d/scratch/git-log-graph.png)
- Primary-screen scrolling, 80-column soft wrapping (`ru` / `les`, `sid` / `ebar)`, `vendo` / `red`), and branch-lane ANSI colours all decode and position accurately.
- **Wide vertical breaks in ASCII commit-graph strokes**:
  - Cousine's `|`, `/`, and `\` glyphs sit well inside `[descender, ascender]` (`2320 fu` tall vs `~1500 fu` glyph height), leaving large vertical gaps between rows on `|` branch lanes and `/` `\` merges, and `_` in `|_|/` (`row 18`) drops noticeably below the `/` stroke ends.
- **Same dark `#1a1ae6` ANSI Blue contrast issue** on blue graph lanes (`rows 0, 10, 11`).

##### 4. [vim-screen.png](~/.gemini/antigravity-cli/brain/b7da30ef-842b-498c-844f-014ca4215b3d/scratch/vim-screen.png)
- Alternate screen (`?1049h`), yellow line-number gutter (`  1`..` 19`), wrapped continuation indent on line 4, cyan `~` empty-buffer markers (`rows 20–22`), bottom status/ruler line (`"native/src/wall/replay.rs" 116L, 3418B   1,1   Top`), and cursor placement at `(col 4, row 0)` on `/` of `//!` are all exact.
- **Focused block cursor washes out the character underneath (`row 0, col 4`)**:
  - Pass 4 (`fs_cursor`, `Z_OFFSET_CURSOR = +0.001`, [wall_cells.wgsl](../../native/src/shaders/wall_cells.wgsl#L690-L698)) alpha-blends `50%` neon-green (`cursor_fill_opacity = 0.50`) **on top of** Pass 3 glyph ink (`Z_OFFSET_INK = 0.0`). Over the magenta `/` on row 0 col 4, blending linear green over the ink cuts the glyph's contrast in half and muddies its hue into a washed-out grey-green, rather than either inverting fg/bg under a solid block cursor or drawing crisp text above the cursor fill.
