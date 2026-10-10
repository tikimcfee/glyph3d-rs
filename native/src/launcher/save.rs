//! Saving the launcher's changes into the user's own `launch_config.toml`
//! (gitignored, personal), so a choice made once stays made.
//!
//! Only what the launcher CHANGED is written: the keys whose value differs
//! from the config as it was loaded, set or removed in place by
//! `toml_edit`, so the file's comments, order, untouched keys (even ones
//! edited by hand since the launcher loaded it) and `[section]` tables
//! survive. A key written under its new name drops its old alias
//! (`lod_min_px`, `greek_onset_px`), which would otherwise be a duplicate.

use std::path::Path;

use crate::launch_config::LaunchConfig;

/// Keys renamed by C26, with the old name the file may still use.
const ALIASES: &[(&str, &str)] = &[("show_glyphs_px", "lod_min_px"), ("text_detail_px", "greek_onset_px")];

/// A new file's first lines.
const HEADER: &str = "# Glyph3D launch configuration, saved by the launcher (launch_config.example.toml\n\
                      # documents every key). Personal and gitignored; edit freely, comments are kept.\n";

/// The keys `changed` sets differently from `baseline`, and the keys it
/// dropped, as one table each.
fn delta(baseline: &LaunchConfig, changed: &LaunchConfig) -> (toml::Table, Vec<String>) {
    let old = toml::Table::try_from(baseline).expect("a launch config serializes");
    let new = toml::Table::try_from(changed).expect("a launch config serializes");
    let removed = old.keys().filter(|k| !new.contains_key(*k)).cloned().collect();
    let set = new.into_iter().filter(|(k, v)| old.get(k) != Some(v)).collect();
    (set, removed)
}

/// `text` (a launch config file, or empty) with `changed`'s differences
/// from `baseline` applied.
pub fn apply(text: &str, baseline: &LaunchConfig, changed: &LaunchConfig) -> Result<String, String> {
    let (set, removed) = delta(baseline, changed);
    if set.is_empty() && removed.is_empty() {
        return Ok(text.to_string());
    }
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| e.to_string())?;
    if text.trim().is_empty() {
        doc.decor_mut().set_prefix(HEADER);
    }
    let unalias = |doc: &mut toml_edit::DocumentMut, key: &str| {
        for (name, alias) in ALIASES {
            if *name == key {
                doc.remove(alias);
            }
        }
    };
    for key in &removed {
        doc.remove(key);
        unalias(&mut doc, key);
    }
    for (key, value) in set {
        unalias(&mut doc, &key);
        // The value as toml writes it, re-read as an editable item.
        let one = toml::to_string(&toml::Table::from_iter([(key.clone(), value)])).map_err(|e| e.to_string())?;
        let parsed: toml_edit::DocumentMut = one.parse().map_err(|e: toml_edit::TomlError| e.to_string())?;
        match doc.get_mut(&key).and_then(toml_edit::Item::as_value_mut) {
            // An existing key keeps its place and the comments around it.
            Some(v) => {
                let decor = v.decor().clone();
                *v = parsed[&key].as_value().expect("a top-level launch option is a value").clone();
                *v.decor_mut() = decor;
            }
            None => doc[&key] = parsed[&key].clone(),
        }
    }
    let out = doc.to_string();
    // What is written must load: never leave the user a file the renderer
    // refuses at startup.
    LaunchConfig::from_toml_str(&out).map_err(|e| format!("the saved config would not load: {e}"))?;
    Ok(out)
}

/// Apply the changes to the file at `path` (created if absent).
pub fn save(path: &Path, baseline: &LaunchConfig, changed: &LaunchConfig) -> Result<bool, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("could not read {}: {e}", path.display())),
    };
    let out = apply(&text, baseline, changed)?;
    if out == text {
        return Ok(false);
    }
    std::fs::write(path, out).map_err(|e| format!("could not write {}: {e}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fold::WrapMode;
    use glyph_field::GlyphFieldMode;

    const FILE: &str = "\
# My launch config
file_backgrounds = true

# Macro LOD threshold
lod_min_px = 1.0

# Wrap layout mode
wrap_mode = \"back\"  # the staircase
field_mode = \"derived\"
load_repo = \"native/fixtures/g-pick-repo\"

[glyph_scene]
clear_color = [1.0, 0.0, 0.0, 1.0]
";

    #[test]
    fn only_changes_are_written_and_comments_survive() {
        let base = LaunchConfig::from_toml_str(FILE).unwrap();
        let mut changed = base.clone();
        changed.wrap_mode = Some(WrapMode::Down);
        changed.field_mode = Some(GlyphFieldMode::Visible);
        changed.show_glyphs_px = Some(2.0);
        changed.cluster_mode = Some(crate::fold::ClusterMode::Leader);
        changed.load_repo = None;
        changed.demo = Some(true);
        let out = apply(FILE, &base, &changed).unwrap();

        assert!(out.contains("# My launch config\nfile_backgrounds = true\n"), "{out}");
        assert!(out.contains("# Wrap layout mode\nwrap_mode = \"down\"  # the staircase\n"), "in place, comments kept: {out}");
        assert!(out.contains("field_mode = \"visible\""), "{out}");
        assert!(!out.contains("lod_min_px"), "the alias goes when its key is written: {out}");
        assert!(out.contains("show_glyphs_px = 2.0"), "{out}");
        assert!(!out.contains("load_repo"), "a scene key the launch dropped is removed: {out}");
        assert!(out.contains("[glyph_scene]\nclear_color = [1.0, 0.0, 0.0, 1.0]"), "sections untouched: {out}");
        let back = LaunchConfig::from_toml_str(&out).unwrap();
        assert_eq!(back, changed);
    }

    /// A key the launcher did not change is not written, even when the file
    /// now says something else (a hand edit since the launcher loaded it).
    #[test]
    fn untouched_keys_keep_hand_edits() {
        let base = LaunchConfig::from_toml_str(FILE).unwrap();
        let mut changed = base.clone();
        changed.field_mode = Some(GlyphFieldMode::Visible);
        let edited = FILE.replace("wrap_mode = \"back\"", "wrap_mode = \"down\"");
        let out = apply(&edited, &base, &changed).unwrap();
        assert!(out.contains("wrap_mode = \"down\""), "{out}");
        assert!(out.contains("field_mode = \"visible\""), "{out}");
        assert_eq!(apply(FILE, &base, &base).unwrap(), FILE, "no change, no rewrite");
    }

    #[test]
    fn a_new_file_starts_with_a_header_and_loads() {
        let changed = LaunchConfig {
            field_mode: Some(GlyphFieldMode::Visible),
            repo_presets: Some(vec![".".into(), "~/src/linux".into()]),
            ..Default::default()
        };
        let out = apply("", &LaunchConfig::default(), &changed).unwrap();
        assert!(out.starts_with("# Glyph3D launch configuration"), "{out}");
        assert_eq!(LaunchConfig::from_toml_str(&out).unwrap(), changed);
    }
}
