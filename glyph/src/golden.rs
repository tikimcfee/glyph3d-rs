//! Goldens are re-rendered and compared. There is no build path, by design:
//! re-baselining is a human decision, and the tool refuses to make it.
//!
//! Also `drift`: this host's fresh frames against every other rasterizer's set.

use std::path::{Path, PathBuf};

use crate::manifest::{Artifact, Class, Manifest};
use crate::paths::{gpu_key, gpu_profile, native, resolve_gpu, root, sh, step, sweep};

/// A golden equivalent's file-name tag: its arguments' alphanumerics, runs
/// of anything else collapsed to one `-` (`--field-mode derived` →
/// `field-mode-derived`).
pub(crate) fn equivalent_tag(args: &str) -> String {
    let mut tag = String::new();
    for c in args.chars() {
        if c.is_ascii_alphanumeric() {
            tag.push(c);
        } else if !tag.is_empty() && !tag.ends_with('-') {
            tag.push('-');
        }
    }
    tag.trim_end_matches('-').to_string()
}

/// Where a view's fresh frame lands: `<name>.png`, or `<name>@<tag>.png`
/// under a golden equivalent.
fn shot_path(name: &str, equivalent: Option<&str>) -> PathBuf {
    match equivalent {
        None => sweep().join(format!("{name}.png")),
        Some(args) => sweep().join(format!("{name}@{}.png", equivalent_tag(args))),
    }
}

/// Render every golden view into the sweep dir — and, with `equivalents`,
/// again under each golden equivalent. Shared by the gate and by `drift`
/// (which compares only the plain frames), so both read the same renders.
fn render_views(m: &Manifest, equivalents: bool) -> bool {
    let _ = std::fs::create_dir_all(sweep());
    let mut variants: Vec<Option<&str>> = vec![None];
    if equivalents {
        variants.extend(m.settings.golden_equivalents.iter().map(|e| Some(e.as_str())));
    }
    let mut ok = true;
    for v in &m.golden_view {
        for eq in &variants {
            let shot = shot_path(&v.name, *eq);
            let (good, out) = sh(
                &format!(
                    "../target/release/glyph3d-native {} {} --screenshot {}",
                    v.cmd,
                    eq.unwrap_or(""),
                    shot.to_string_lossy()
                ),
                &native(),
            );
            if !good {
                let under = eq.map(|e| format!(" under {e}")).unwrap_or_default();
                println!("FAIL  {}{under} render errored — {}", v.name, out.lines().last().unwrap_or(""));
                ok = false;
            }
        }
    }
    ok
}

/// The directory a golden set lives in for `key`, from the artifact's own
/// declared outputs rather than a second spelling of the path here.
fn golden_dir(a: &Artifact, key: &str) -> PathBuf {
    let first = a.outputs.first().expect("a golden artifact declares outputs");
    root().join(first.replace("{gpu}", key)).parent().unwrap().to_path_buf()
}

/// Provenance: the record beside a golden set versus the adapter that is
/// about to be compared against it. A NOTE, never a failure — a driver
/// update that moves nothing is fine, and one that moves a pixel is then
/// explained by the line that differs.
fn adapter_note(key: &str, dir: &Path) {
    let rec = dir.join("ADAPTER.txt");
    let Some(live) = gpu_profile() else { return };
    match std::fs::read_to_string(&rec) {
        Err(_) => {
            println!("NOTE  no ADAPTER.txt recorded for the {key} golden set. On the machine the");
            println!("      baselines came from:  target/release/glyph3d-native --gpu-profile > {}", rec.strip_prefix(root()).unwrap_or(&rec).display());
        }
        Ok(recorded) if recorded == live => {}
        Ok(recorded) => {
            println!("NOTE  the {key} baselines were recorded on different hardware or driver than this:");
            for (r, l) in recorded.lines().zip(live.lines()).filter(|(r, l)| r != l) {
                println!("      recorded  {r}");
                println!("      live      {l}");
            }
        }
    }
}

pub(crate) fn verify_golden(m: &Manifest) -> bool {
    let a = m.artifact.values().find(|a| a.class == Class::Golden).unwrap();
    let Some(key) = gpu_key() else {
        println!("FAIL  pixel-ab — the renderer did not answer --gpu-key, so this host's golden set is unknown");
        return false;
    };
    let dir = golden_dir(a, key);
    if !render_views(m, true) {
        return false;
    }
    let mut ok = true;
    let mut missing: Vec<&str> = Vec::new();
    for v in &m.golden_view {
        let shot = sweep().join(format!("{}.png", v.name));
        let baseline = a
            .outputs
            .iter()
            .find(|o| o.ends_with(&format!("/{}.png", v.name)))
            .and_then(|o| resolve_gpu(o))
            .map(|o| root().join(o));
        match baseline {
            Some(b) if !b.exists() => {
                println!("FAIL  {}.png — no {key} baseline", v.name);
                missing.push(&v.name);
                ok = false;
            }
            Some(b) => {
                let golden = std::fs::read(&b).ok();
                if golden == std::fs::read(&shot).ok() {
                    println!("PASS  {}.png BYTE-EQUAL ({key})", v.name)
                } else {
                    println!("FAIL  {}.png diverges from baseline — the renderer changed.", v.name);
                    println!("      If that was intended, re-baseline by hand and say so in the commit.");
                    ok = false;
                }
                // The same baseline, under each equivalent: a divergence here
                // with the plain frame equal is the variant breaking, not the
                // renderer moving — never a reason to re-baseline.
                for eq in &m.settings.golden_equivalents {
                    if golden == std::fs::read(shot_path(&v.name, Some(eq))).ok() {
                        println!("PASS  {}.png BYTE-EQUAL under {eq} ({key})", v.name)
                    } else {
                        println!("FAIL  {}.png diverges from baseline under {eq} — that variant no longer", v.name);
                        println!("      renders what the plain view does.");
                        ok = false;
                    }
                }
            }
            None => {
                println!("FAIL  {} has no declared baseline", v.name);
                ok = false;
            }
        }
    }
    if !missing.is_empty() {
        let rel = |p: &Path| p.strip_prefix(root()).unwrap_or(p).display().to_string();
        println!("      This host's key is {key} and no golden set exists for it. The frames it");
        println!("      would be compared against are in {}/. Adopting them is a human act —", rel(&sweep()));
        println!("      look at them, and run `glyph drift` to see how far they sit from the");
        println!("      other sets — then:");
        println!("        mkdir -p {}", rel(&dir));
        println!(
            "        cp {}/{{{}}}.png {}/",
            rel(&sweep()),
            m.golden_view.iter().map(|v| v.name.as_str()).collect::<Vec<_>>().join(","),
            rel(&dir)
        );
        println!("        target/release/glyph3d-native --gpu-profile > {}/ADAPTER.txt", rel(&dir));
        println!("      and say why in the commit.");
    } else {
        adapter_note(key, &dir);
    }
    ok
}

// ── drift: this host against every other rasterizer's golden set ─────────

/// Decode a PNG to RGBA8. Only what the offscreen path writes (8-bit RGB or
/// RGBA) is accepted; anything else is a wrong file, not a format to widen for.
fn png_rgba(path: &Path) -> Result<(u32, u32, Vec<u8>), String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut reader = png::Decoder::new(std::io::BufReader::new(file))
        .read_info()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let size = reader.output_buffer_size().ok_or_else(|| format!("{}: size overflow", path.display()))?;
    let mut buf = vec![0u8; size];
    let info = reader.next_frame(&mut buf).map_err(|e| format!("{}: {e}", path.display()))?;
    if info.bit_depth != png::BitDepth::Eight {
        return Err(format!("{}: {:?} bit depth, expected 8", path.display(), info.bit_depth));
    }
    let px = (info.width * info.height) as usize;
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf[..px * 4].to_vec(),
        png::ColorType::Rgb => {
            buf[..px * 3].as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255]).collect()
        }
        other => return Err(format!("{}: {other:?} colour type, expected RGB/RGBA", path.display())),
    };
    Ok((info.width, info.height, rgba))
}

/// How two frames of the same size differ, in the terms that separate
/// rasterizer edge noise from a layout change.
pub(crate) struct Drift {
    differing: usize,
    max_delta: u8,
    high: usize,
    /// High-delta pixels with no high-delta 8-neighbour: single edge flips.
    isolated: usize,
    /// High-delta pixels with three or more: the shape of something that moved.
    clustered: usize,
}

const DRIFT_HIGH: u8 = 16;

fn drift_stats(w: u32, h: u32, a: &[u8], b: &[u8]) -> Drift {
    let (w, h) = (w as usize, h as usize);
    let delta: Vec<u8> = a
        .as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0)
        .map(|(p, q)| p.iter().zip(q).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0))
        .collect();
    let high_at = |x: isize, y: isize| {
        x >= 0 && y >= 0 && x < w as isize && y < h as isize && delta[y as usize * w + x as usize] >= DRIFT_HIGH
    };
    let mut d = Drift { differing: 0, max_delta: 0, high: 0, isolated: 0, clustered: 0 };
    for y in 0..h {
        for x in 0..w {
            let v = delta[y * w + x];
            if v == 0 {
                continue;
            }
            d.differing += 1;
            d.max_delta = d.max_delta.max(v);
            if v >= DRIFT_HIGH {
                d.high += 1;
                let (x, y) = (x as isize, y as isize);
                let n = [(-1, -1), (0, -1), (1, -1), (-1, 0), (1, 0), (-1, 1), (0, 1), (1, 1)]
                    .iter()
                    .filter(|(dx, dy)| high_at(x + dx, y + dy))
                    .count();
                if n == 0 {
                    d.isolated += 1;
                } else if n >= 3 {
                    d.clustered += 1;
                }
            }
        }
    }
    d
}

/// An instrument, not a gate: render this host's views and lay them against
/// every OTHER rasterizer's golden set. Prints where the two disagree and in
/// what shape; exits nonzero only when it could not compare. Matching across
/// vendors is not a goal — this exists so that the day a cross-vendor
/// difference stops looking like isolated edge flips, someone sees it.
pub(crate) fn cmd_drift(m: &Manifest) -> bool {
    let a = m.artifact.values().find(|a| a.class == Class::Golden).unwrap();
    let Some(key) = gpu_key() else {
        println!("FAIL  the renderer did not answer --gpu-key; run `glyph build` first");
        return false;
    };
    step(&format!("drift: this host ({key}) against the other golden sets"));
    if !render_views(m, false) {
        return false;
    }
    let base = golden_dir(a, key).parent().unwrap().to_path_buf();
    let mut others: Vec<String> = std::fs::read_dir(&base)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n != key)
                .collect()
        })
        .unwrap_or_default();
    others.sort();
    if others.is_empty() {
        println!("NOTE  no other golden set under {} to compare against", base.display());
        return true;
    }
    let mut ok = true;
    for other in &others {
        let dir = base.join(other);
        let device = std::fs::read_to_string(dir.join("ADAPTER.txt"))
            .ok()
            .and_then(|t| t.lines().find(|l| l.starts_with("device:")).map(|l| l.to_string()))
            .unwrap_or_else(|| "device: (no ADAPTER.txt recorded)".to_string());
        println!("\n{other}  —  {device}");
        println!("  {:<10} {:>9} {:>7} {:>5} {:>7} {:>9} {:>9}", "view", "differing", "%", "max", ">=16", "isolated", "clustered");
        let mut clustered_total = 0usize;
        for v in &m.golden_view {
            let mine = sweep().join(format!("{}.png", v.name));
            let theirs = dir.join(format!("{}.png", v.name));
            if !theirs.exists() {
                println!("  {:<10} (no {other} baseline)", v.name);
                continue;
            }
            let (pa, pb) = match (png_rgba(&mine), png_rgba(&theirs)) {
                (Ok(a), Ok(b)) => (a, b),
                (Err(e), _) | (_, Err(e)) => {
                    println!("  {:<10} FAIL {e}", v.name);
                    ok = false;
                    continue;
                }
            };
            if (pa.0, pa.1) != (pb.0, pb.1) {
                println!("  {:<10} FAIL size {}x{} vs {}x{}", v.name, pa.0, pa.1, pb.0, pb.1);
                ok = false;
                continue;
            }
            let d = drift_stats(pa.0, pa.1, &pa.2, &pb.2);
            let total = (pa.0 * pa.1) as f64;
            println!(
                "  {:<10} {:>9} {:>6.2}% {:>5} {:>7} {:>9} {:>9}",
                v.name, d.differing, 100.0 * d.differing as f64 / total, d.max_delta, d.high, d.isolated, d.clustered
            );
            clustered_total += d.clustered;
        }
        if clustered_total == 0 {
            println!("  no clustered high-delta pixels: the difference has the shape of rasterizer edge noise");
        } else {
            println!("  {clustered_total} clustered high-delta pixels: something has a shape — look at the frames");
        }
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A golden equivalent's frames get a stable, file-safe tag of their own,
    /// so they never overwrite the plain frame the baseline is adopted from.
    #[test]
    fn equivalent_frames_never_shadow_the_plain_frame() {
        assert_eq!(equivalent_tag("--field-mode derived"), "field-mode-derived");
        assert_eq!(equivalent_tag("  --zoom 1.25 "), "zoom-1-25");
        assert_eq!(equivalent_tag("--"), "");
        let plain = shot_path("text", None);
        let derived = shot_path("text", Some("--field-mode derived"));
        assert_ne!(plain, derived);
        assert!(derived.ends_with("text@field-mode-derived.png"), "{}", derived.display());
    }
}
