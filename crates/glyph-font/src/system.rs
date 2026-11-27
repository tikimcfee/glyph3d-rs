//! System font family lookup for macOS and Linux (zero external dependencies).
//!
//! Resolves a font family name (such as `"monaspace-neon"`, `"cousine"`, `"Menlo"`,
//! `"SF Mono"`, `"Monaco"`, `"JetBrains Mono"`, or `"DejaVu Sans Mono"`) into a
//! [`FontSpec`] with Regular, Bold, Italic, and Bold Italic faces:
//!
//! 1. **Bundled wall families** in `fonts_dir` (`assets/fonts/`): `"cousine"`,
//!    `"monaspace-neon"`, `"monaspace-argon"` (matched case/space/hyphen-insensitively).
//! 2. **System font directories**, walked once and cached in-process as lightweight
//!    `(path, face_index, weight, italic, variable axes)` records (no font bytes retained):
//!    - **macOS:** `~/Library/Fonts`, `/Library/Fonts`, `/System/Library/Fonts`,
//!      `/System/Library/Fonts/Supplemental`.
//!    - **Linux:** `$XDG_DATA_HOME/fonts` (or `~/.local/share/fonts`), `~/.fonts`,
//!      `/usr/local/share/fonts`, `/usr/share/fonts`.
//!      Every `.ttf`, `.otf`, `.ttc`, and `.otc` file is inspected via `ttf-parser` across
//!      all collection face indices, reading both Unicode (`platform 0/3`) and Macintosh Roman
//!      (`platform 1, encoding 0`) `name` table entries (required on macOS for `Menlo.ttc`,
//!      `Monaco.ttf`, and `Courier.ttc`).
//! 3. **Linux `fc-list` fallback:** if direct directory scanning finds no match on Linux,
//!    queries `fc-list : family file index` (when `fc-list` is on `PATH`) so custom
//!    `fonts.conf` directories and fontconfig aliases work too.
//!
//! If no bundled or installed font matches, returns [`FontError::FamilyNotFound`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use ttf_parser::{name_id, Face as TtfFace, PlatformId, Tag};

use crate::atlas::FontError;
use crate::families::{
    bundled_family_spec, fallback_font_chain, FaceSpec, FontSource, FontSpec, Variation,
};

#[derive(Clone, Debug)]
struct ScannedFace {
    path: PathBuf,
    face_index: u32,
    display_name: String,
    weight: u16,
    italic: bool,
    wght_range: Option<(f32, f32)>,
    slnt_range: Option<(f32, f32)>,
}

struct SystemFontIndex {
    by_key: HashMap<String, Vec<ScannedFace>>,
}

static SYSTEM_INDEX: OnceLock<SystemFontIndex> = OnceLock::new();

/// Normalize a font family name for case/punctuation/leading-dot-insensitive matching.
pub fn normalize_family_key(name: &str) -> String {
    let trimmed = name.trim().trim_start_matches('.');
    let mut key = String::with_capacity(trimmed.len());
    for ch in trimmed.chars() {
        if ch.is_ascii_alphanumeric() {
            key.push(ch.to_ascii_lowercase());
        }
    }
    // Apple's system monospace font names itself `.SF NS Mono` in its `name` table,
    // while users ask for `SF Mono` or `SFNSMono`.
    if key == "sfnsmono" {
        return "sfmono".to_string();
    }
    key
}

/// Standard OS font search roots on macOS and Linux.
pub fn system_font_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if cfg!(target_os = "macos") {
        if let Some(ref h) = home {
            dirs.push(h.join("Library/Fonts"));
        }
        dirs.push(PathBuf::from("/Library/Fonts"));
        dirs.push(PathBuf::from("/System/Library/Fonts"));
        dirs.push(PathBuf::from("/System/Library/Fonts/Supplemental"));
    } else {
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
            dirs.push(PathBuf::from(xdg).join("fonts"));
        } else if let Some(ref h) = home {
            dirs.push(h.join(".local/share/fonts"));
        }
        if let Some(ref h) = home {
            dirs.push(h.join(".fonts"));
        }
        dirs.push(PathBuf::from("/usr/local/share/fonts"));
        dirs.push(PathBuf::from("/usr/share/fonts"));
    }
    dirs
}

fn is_font_extension(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "ttf" | "otf" | "ttc" | "otc"
    )
}

fn collect_font_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect_font_files(&path, out);
        } else if is_font_extension(&path) {
            out.push(path);
        }
    }
}

/// Decode a `name` record string from either Unicode (`to_string()`) or Macintosh Roman
/// (`PlatformId::Macintosh`, encoding `0`, ASCII/Latin subset).
fn decode_name_record(rec: &ttf_parser::name::Name<'_>) -> Option<String> {
    if rec.is_unicode() {
        return rec.to_string();
    }
    if rec.platform_id == PlatformId::Macintosh && rec.encoding_id == 0 {
        // ASCII bytes match MacRoman verbatim; ignore non-ASCII control/legacy bytes cleanly.
        if rec.name.iter().all(|&b| (0x20..=0x7E).contains(&b)) {
            return std::str::from_utf8(rec.name).ok().map(str::to_owned);
        }
    }
    None
}

fn scan_file_into(path: &Path, by_key: &mut HashMap<String, Vec<ScannedFace>>) {
    let Ok(bytes) = std::fs::read(path) else {
        return;
    };
    let count = ttf_parser::fonts_in_collection(&bytes).unwrap_or(1);
    for face_index in 0..count {
        let Ok(face) = TtfFace::parse(&bytes, face_index) else {
            continue;
        };
        // Skip bitmap-only / empty-outline fonts (e.g. Apple Color Emoji) that cannot draw Slug text.
        if face.units_per_em() == 0 {
            continue;
        }
        let mut typographic: Option<String> = None;
        let mut family: Option<String> = None;
        let mut subfamily: Option<String> = None;
        let mut keys: Vec<String> = Vec::new();

        for rec in face.names() {
            let Some(s) = decode_name_record(&rec) else {
                continue;
            };
            let trimmed = s.trim();
            if trimmed.is_empty() {
                continue;
            }
            match rec.name_id {
                name_id::TYPOGRAPHIC_FAMILY => {
                    if typographic.is_none() {
                        typographic = Some(trimmed.to_owned());
                    }
                    let k = normalize_family_key(trimmed);
                    if !k.is_empty() && !keys.contains(&k) {
                        keys.push(k);
                    }
                }
                name_id::FAMILY => {
                    if family.is_none() {
                        family = Some(trimmed.to_owned());
                    }
                    let k = normalize_family_key(trimmed);
                    if !k.is_empty() && !keys.contains(&k) {
                        keys.push(k);
                    }
                }
                name_id::SUBFAMILY | name_id::TYPOGRAPHIC_SUBFAMILY
                    if subfamily.is_none() =>
                {
                    subfamily = Some(trimmed.to_owned());
                }
                _ => {}
            }
        }
        if keys.is_empty() {
            continue;
        }
        let display_name = typographic.or(family).unwrap_or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("system-font")
                .to_owned()
        });

        let sub_lower = subfamily.as_deref().unwrap_or("").to_ascii_lowercase();
        let weight = face.weight().to_number();
        let italic = face.is_italic()
            || face.is_oblique()
            || sub_lower.contains("italic")
            || sub_lower.contains("oblique");

        let mut wght_range = None;
        let mut slnt_range = None;
        if face.is_variable() {
            for axis in face.variation_axes() {
                if axis.tag == Tag::from_bytes(b"wght") {
                    wght_range = Some((axis.min_value, axis.max_value));
                } else if axis.tag == Tag::from_bytes(b"slnt") {
                    slnt_range = Some((axis.min_value, axis.max_value));
                }
            }
        }

        let entry = ScannedFace {
            path: path.to_path_buf(),
            face_index,
            display_name,
            weight,
            italic,
            wght_range,
            slnt_range,
        };
        for k in keys {
            by_key.entry(k).or_default().push(entry.clone());
        }
    }
}

fn scan_dirs(dirs: &[PathBuf]) -> SystemFontIndex {
    let mut files = Vec::new();
    for dir in dirs {
        collect_font_files(dir, &mut files);
    }
    let mut by_key: HashMap<String, Vec<ScannedFace>> = HashMap::new();
    for path in files {
        scan_file_into(&path, &mut by_key);
    }
    SystemFontIndex { by_key }
}

fn system_index() -> &'static SystemFontIndex {
    SYSTEM_INDEX.get_or_init(|| scan_dirs(&system_font_dirs()))
}

#[cfg(not(target_os = "macos"))]
fn fc_list_candidates(name: &str) -> Vec<ScannedFace> {
    let Ok(output) = std::process::Command::new("fc-list")
        .args([":", "family", "file", "index"])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let want = normalize_family_key(name);
    let mut by_key: HashMap<String, Vec<ScannedFace>> = HashMap::new();
    let mut seen_paths: Vec<PathBuf> = Vec::new();
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        // `fc-list : family file index` formats lines as: `<file>: <families>:index=<n>`
        // or `<families>:file=<path>:index=<n>` depending on format string; use `-f` instead below.
        let _ = line;
    }
    // Use explicit format string for unambiguous parsing: `<path>\t<family>\n`
    let Ok(output) = std::process::Command::new("fc-list")
        .args(["-f", "%{file}\t%{family}\n"])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((file, families)) = line.split_once('\t') else {
            continue;
        };
        let matched = families
            .split(',')
            .any(|f| normalize_family_key(f) == want);
        if !matched {
            continue;
        }
        let path = PathBuf::from(file.trim());
        if is_font_extension(&path) && !seen_paths.contains(&path) {
            seen_paths.push(path.clone());
            scan_file_into(&path, &mut by_key);
        }
    }
    by_key.remove(&want).unwrap_or_default()
}

fn pick_best(faces: &[ScannedFace], want_italic: bool, target_weight: u16) -> Option<&ScannedFace> {
    faces
        .iter()
        .filter(|f| f.italic == want_italic)
        .min_by_key(|f| {
            // Prefer a static or variable face whose natural weight is closest to target_weight.
            let dist = (f.weight as i32 - target_weight as i32).unsigned_abs();
            // Tie-break: prefer variable wght covering target, else lower face_index.
            let covers = f
                .wght_range
                .is_some_and(|(lo, hi)| (target_weight as f32) >= lo && (target_weight as f32) <= hi);
            (if covers { 0u32 } else { dist }, f.face_index)
        })
}

fn clamp_var(range: (f32, f32), value: f32) -> f32 {
    value.clamp(range.0, range.1)
}

fn build_spec_from_faces(requested_id: &str, faces: &[ScannedFace]) -> Option<FontSpec> {
    let regular = pick_best(faces, false, 400).or_else(|| faces.first())?;
    let mut reg_vars = Vec::new();
    if let Some(wr) = regular.wght_range {
        reg_vars.push(Variation::new(*b"wght", clamp_var(wr, 400.0)));
    }
    if let Some(sr) = regular.slnt_range {
        reg_vars.push(Variation::new(*b"slnt", clamp_var(sr, 0.0)));
    }

    // Bold: either a distinct static face with weight >= 600, or a variable wght=700 instance.
    let bold_static = pick_best(faces, false, 700).filter(|f| {
        f.weight >= 600 && (f.path != regular.path || f.face_index != regular.face_index)
    });
    let bold = if let Some(b) = bold_static {
        Some(FaceSpec::file_face(&b.path, b.face_index))
    } else if let Some(wr) = regular.wght_range.filter(|&(_, hi)| hi >= 600.0) {
        let mut vars = vec![Variation::new(*b"wght", clamp_var(wr, 700.0))];
        if let Some(sr) = regular.slnt_range {
            vars.push(Variation::new(*b"slnt", clamp_var(sr, 0.0)));
        }
        Some(FaceSpec::instance(vars))
    } else {
        None
    };

    // Italic: either a dedicated italic face/file (with wght=400 if variable) or variable slnt=-11.
    let italic_face = pick_best(faces, true, 400);
    let italic = if let Some(it) = italic_face {
        let mut spec = FaceSpec::file_face(&it.path, it.face_index);
        if let Some(wr) = it.wght_range {
            spec = spec.with_variations(vec![Variation::new(*b"wght", clamp_var(wr, 400.0))]);
        }
        Some(spec)
    } else if let Some(sr) = regular.slnt_range.filter(|&(lo, _)| lo < -1.0) {
        let mut vars = Vec::new();
        if let Some(wr) = regular.wght_range {
            vars.push(Variation::new(*b"wght", clamp_var(wr, 400.0)));
        }
        vars.push(Variation::new(*b"slnt", clamp_var(sr, -11.0)));
        Some(FaceSpec::instance(vars))
    } else {
        None
    };

    // Bold Italic: distinct bold-italic face, or variable wght=700 on the italic file, or variable wght+slnt.
    let bold_italic_static = pick_best(faces, true, 700).filter(|f| {
        f.weight >= 600
            && italic_face.is_none_or(|it| f.path != it.path || f.face_index != it.face_index)
    });
    let bold_italic = if let Some(bi) = bold_italic_static {
        Some(FaceSpec::file_face(&bi.path, bi.face_index))
    } else if let Some(it) = italic_face {
        it.wght_range.filter(|&(_, hi)| hi >= 600.0).map(|wr| {
            FaceSpec::file_face(&it.path, it.face_index)
                .with_variations(vec![Variation::new(*b"wght", clamp_var(wr, 700.0))])
        })
    } else if let (Some(wr), Some(sr)) = (
        regular.wght_range.filter(|&(_, hi)| hi >= 600.0),
        regular.slnt_range.filter(|&(lo, _)| lo < -1.0),
    ) {
        Some(FaceSpec::instance(vec![
            Variation::new(*b"wght", clamp_var(wr, 700.0)),
            Variation::new(*b"slnt", clamp_var(sr, -11.0)),
        ]))
    } else {
        None
    };

    Some(
        FontSpec::new(regular.display_name.clone(), FontSource::path(&regular.path))
            .with_family(requested_id.trim().to_owned())
            .with_face_index(regular.face_index)
            .with_variations(reg_vars)
            .with_faces(bold, italic, bold_italic),
    )
}

/// Resolve `family_name` from bundled wall fonts in `fonts_dir` first, then from installed
/// system fonts (macOS Font directories / Linux XDG + system font directories + `fc-list`).
///
/// Returns [`FontError::FamilyNotFound`] when no bundled or system font matches.
pub fn resolve_family_spec(fonts_dir: &Path, family_name: &str) -> Result<FontSpec, FontError> {
    let trimmed = family_name.trim();
    if trimmed.is_empty() {
        return Err(FontError::FamilyNotFound(family_name.to_owned()));
    }
    if let Some(bundled) = bundled_family_spec(fonts_dir, trimmed) {
        return Ok(bundled);
    }
    let key = normalize_family_key(trimmed);
    if let Some(faces) = system_index().by_key.get(&key) {
        if let Some(spec) = build_spec_from_faces(trimmed, faces) {
            return Ok(spec);
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let fc_faces = fc_list_candidates(trimmed);
        if !fc_faces.is_empty() {
            if let Some(spec) = build_spec_from_faces(trimmed, &fc_faces) {
                return Ok(spec);
            }
        }
    }
    Err(FontError::FamilyNotFound(trimmed.to_owned()))
}

/// Build a complete single-family font chain (`[primary, MesloLGS NF Mono, DejaVu Sans]`)
/// for `family_name` with `line_height_scale` applied to the primary and fallback fonts.
///
/// Used when switching families or line height at run time so the previous family's atlas
/// and GPU textures can be dropped cleanly without accumulating stale font data.
pub fn resolve_family_chain(
    fonts_dir: &Path,
    family_name: &str,
    line_height_scale: f64,
) -> Result<Vec<FontSpec>, FontError> {
    let primary = resolve_family_spec(fonts_dir, family_name)?
        .with_line_height_scale(line_height_scale);
    let mut chain = vec![primary];
    for fb in fallback_font_chain(fonts_dir) {
        chain.push(fb.with_line_height_scale(line_height_scale).with_lazy(true));
    }
    Ok(chain)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Face, FontAtlas};

    fn assets_fonts_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/fonts")
    }

    #[test]
    fn bundled_names_resolve_with_aliases_and_missing_errors() {
        let dir = assets_fonts_dir();
        let neon = resolve_family_spec(&dir, "monaspace-neon").expect("monaspace-neon");
        assert_eq!(neon.family.as_deref(), Some("monaspace-neon"));
        let neon_alias = resolve_family_spec(&dir, "Monaspace Neon").expect("Monaspace Neon");
        assert_eq!(neon_alias.family.as_deref(), Some("monaspace-neon"));
        let cousine = resolve_family_spec(&dir, "cousine").expect("cousine");
        assert_eq!(cousine.family.as_deref(), Some("cousine"));

        let err = resolve_family_spec(&dir, "NoSuchMonospaceFamilyXYZ999").unwrap_err();
        assert!(matches!(err, FontError::FamilyNotFound(_)), "unexpected err: {err}");
    }

    #[test]
    fn line_height_scale_expands_extents_without_distorting_glyphs() {
        let dir = assets_fonts_dir();
        let c1 = resolve_family_chain(&dir, "monaspace-neon", 1.0).unwrap();
        let c2 = resolve_family_chain(&dir, "monaspace-neon", 1.5).unwrap();
        let mut a1 = FontAtlas::load(&c1).unwrap();
        let mut a2 = FontAtlas::load(&c2).unwrap();
        let m1 = a1.family_metrics(a1.default_family());
        let m2 = a2.family_metrics(a2.default_family());
        assert!(
            ((m2.extents.em_height() / m1.extents.em_height()) - 1.5).abs() < 1e-6,
            "em_height should scale by 1.5x: {} vs {}",
            m1.extents.em_height(),
            m2.extents.em_height()
        );
        // Same advance_fu; aspect drops by 1/1.5 because the cell is 1.5x taller.
        assert_eq!(m1.advance_fu, m2.advance_fu);
        assert!(((m1.aspect / m2.aspect) - 1.5).abs() < 1e-4);
        assert_ne!(a1.glyph(a1.default_family(), Face::Regular, 'A'), 0);
        assert_ne!(a2.glyph(a2.default_family(), Face::Regular, 'A'), 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_system_monospace_fonts_resolve_and_encode_faces() {
        let dir = assets_fonts_dir();
        // Menlo.ttc is a TrueType Collection with Regular/Bold/Italic/BoldItalic face indices.
        let menlo_chain = resolve_family_chain(&dir, "Menlo", 1.0).expect("Menlo installed on macOS");
        assert!(menlo_chain[0].bold.is_some(), "Menlo should resolve Bold from Menlo.ttc");
        assert!(menlo_chain[0].italic.is_some(), "Menlo should resolve Italic from Menlo.ttc");
        let mut atlas = FontAtlas::load(&menlo_chain).expect("load Menlo FontAtlas");
        let fam = atlas.default_family();
        let reg_a = atlas.glyph(fam, Face::Regular, 'A');
        let bold_a = atlas.glyph(fam, Face::Bold, 'A');
        let ital_a = atlas.glyph(fam, Face::Italic, 'A');
        assert_ne!(reg_a, 0);
        assert_ne!(bold_a, 0);
        assert_ne!(ital_a, 0);
        assert_ne!(reg_a, bold_a, "Menlo Regular and Bold must allocate distinct slots");
        assert_ne!(reg_a, ital_a, "Menlo Regular and Italic must allocate distinct slots");

        // SF Mono (.SF NS Mono variable TTF + separate italic file) and Monaco also resolve.
        let sf = resolve_family_chain(&dir, "SF Mono", 1.0).expect("SF Mono installed on macOS");
        let mut sf_atlas = FontAtlas::load(&sf).expect("load SF Mono FontAtlas");
        assert_ne!(sf_atlas.glyph(sf_atlas.default_family(), Face::Regular, 'M'), 0);
        assert_ne!(sf_atlas.glyph(sf_atlas.default_family(), Face::Bold, 'M'), 0);

        let monaco = resolve_family_chain(&dir, "Monaco", 1.0).expect("Monaco installed on macOS");
        let mut monaco_atlas = FontAtlas::load(&monaco).expect("load Monaco FontAtlas");
        assert_ne!(monaco_atlas.glyph(monaco_atlas.default_family(), Face::Regular, 'M'), 0);
    }

    #[test]
    fn lazy_default_chain_defers_fallbacks_and_sibling_families_matching_eager_curves() {
        let dir = assets_fonts_dir();
        let eager_specs = crate::default_font_chain(&dir);
        let lazy_specs = crate::default_font_chain_lazy(&dir, crate::MONASPACE_NEON_FAMILY);

        let mut eager = FontAtlas::load(&eager_specs).expect("eager load");
        let mut lazy = FontAtlas::load(&lazy_specs).expect("lazy load");

        // All 14 font entries are registered, but only Monaspace Neon's 4 faces are loaded at startup.
        assert_eq!(lazy.font_count(), eager.font_count());
        assert_eq!(lazy.loaded_font_count(), 4);
        assert!(eager.loaded_font_count() > lazy.loaded_font_count());

        let neon_e = eager.family_id(crate::MONASPACE_NEON_FAMILY).unwrap();
        let neon_l = lazy.family_id(crate::MONASPACE_NEON_FAMILY).unwrap();

        // ASCII + box sprites stay in Monaspace Neon and load zero fallback/sibling fonts.
        for ch in ['A', 'z', '0', '╭', '█'] {
            let se = eager.glyph(neon_e, Face::Regular, ch);
            let sl = lazy.glyph(neon_l, Face::Regular, ch);
            assert_eq!(eager.slot_curves(se), lazy.slot_curves(sl));
        }
        assert_eq!(lazy.loaded_font_count(), 4);

        // A Nerd Font PUA glyph misses Monaspace Neon and lazily loads MesloLGS NF Mono.
        let nerd_e = eager.glyph(neon_e, Face::Regular, '\u{F021}');
        let nerd_l = lazy.glyph(neon_l, Face::Regular, '\u{F021}');
        assert_ne!(nerd_l, 0);
        assert_eq!(eager.slot_curves(nerd_e), lazy.slot_curves(nerd_l));
        assert_eq!(lazy.loaded_font_count(), 5);

        // Switching to Cousine lazily loads Cousine on demand and matches eager curves bit-for-bit.
        let cous_e = eager.family_id(crate::DEFAULT_FONT_FAMILY).unwrap();
        let cous_l = lazy.family_id(crate::DEFAULT_FONT_FAMILY).unwrap();
        let ce = eager.glyph(cous_e, Face::Bold, 'g');
        let cl = lazy.glyph(cous_l, Face::Bold, 'g');
        assert_eq!(eager.slot_curves(ce), lazy.slot_curves(cl));
        assert!(lazy.loaded_font_count() >= 7);
    }
}
