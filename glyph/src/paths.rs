//! Paths, processes, and the host facts asked of the renderer (its golden-set
//! key and hardware record).

use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

pub(crate) fn native() -> PathBuf {
    root().join("native")
}
pub(crate) fn stamps() -> PathBuf {
    root().join("target/.glyph-stamps")
}
/// Throwaway rebuilds and renders, per checkout. Not the system temp dir:
/// its paths were shared by every worktree, and a gate that `rm -rf`s its
/// scratch first ate another worktree's mid-run (2026-10-26, when gates
/// started running in parallel across helpers).
/// `GLYPH_SCRATCH` overrides it: prove's relax route gives each mutant its
/// own, so mutants running one gate at once never share its scratch.
pub(crate) fn scratch() -> PathBuf {
    std::env::var_os("GLYPH_SCRATCH").map(PathBuf::from).unwrap_or_else(|| root().join("target/glyph-scratch"))
}
/// Where a failure's evidence is kept: `target/glyph-scratch/kept/`, named
/// by when, which process, and what, so nothing overwrites it.
fn kept_path(what: &str, ext: &str) -> PathBuf {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let what: String = what.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    scratch().join("kept").join(format!("{ms}-{}-{what}.{ext}", std::process::id()))
}

/// The path to print for a kept file, or why it was not kept.
fn shown(path: &Path, result: std::io::Result<()>) -> String {
    match result {
        Ok(()) => path.strip_prefix(root()).unwrap_or(path).display().to_string(),
        Err(e) => format!("(not kept: {e})"),
    }
}

/// A failure's whole output, kept so the reason a gate went red outlives the
/// run that printed a summary of it; returns the path to print. Until
/// 2026-10-26 a gate printed a tail or a filtered few lines and dropped the
/// rest, and a red seen once in nine parallel runs could not be looked at
/// again.
pub(crate) fn keep(what: &str, text: &str) -> String {
    let path = kept_path(what, "log");
    let r = std::fs::create_dir_all(scratch().join("kept")).and_then(|()| std::fs::write(&path, text));
    shown(&path, r)
}

/// A failure's file (a diverging frame), copied beside `keep`'s logs: the
/// sweep dir it was rendered into is overwritten by the next run.
pub(crate) fn keep_file(what: &str, file: &Path) -> String {
    let ext = file.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
    let path = kept_path(what, &ext);
    let r = std::fs::create_dir_all(scratch().join("kept")).and_then(|()| std::fs::copy(file, &path).map(|_| ()));
    shown(&path, r)
}

/// Where the golden renders land: `GLYPH_SWEEP` when set (prove's relax route gives
/// each mutant its own, so mutants rendering at once never share a frame),
/// otherwise the battery's untracked sweep directory.
pub(crate) fn sweep() -> PathBuf {
    std::env::var_os("GLYPH_SWEEP").map(PathBuf::from).unwrap_or_else(|| root().join("out/tooling-ab/sweep"))
}

/// The renderer the gates run: `GLYPH_RENDERER` when it is set (prove's relax route
/// sets it to the renderer under relax), otherwise the built product. Every
/// gate reaches the renderer through this one path; `sh` exports it, so a
/// gate's command and scripts read `$GLYPH_RENDERER`.
pub(crate) fn renderer() -> PathBuf {
    std::env::var_os("GLYPH_RENDERER")
        .map(PathBuf::from)
        .unwrap_or_else(|| root().join("target/release/glyph3d-native"))
}

/// The env var `glyph test --relax` sets for its gates: how many relax
/// processes cargo-test's suite runs at once. Its presence is the mode.
pub(crate) const UNDER_RELAX: &str = "GLYPH_UNDER_RELAX";

/// Whether this run is `glyph test --relax`, and its suite's job count: the
/// gates read the tree from source under relax, and no product is built.
pub(crate) fn under_relax() -> Option<usize> {
    std::env::var(UNDER_RELAX).ok().and_then(|v| v.parse().ok()).filter(|&j| j > 0)
}

/// Every external command goes through here so failures look the same.
pub(crate) fn sh(cmd: &str, cwd: &Path) -> (bool, String) {
    let out = Command::new("bash").arg("-c").arg(cmd).current_dir(cwd).env("GLYPH_RENDERER", renderer()).output();
    match out {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.success(), s)
        }
        Err(e) => (false, format!("could not spawn: {e}")),
    }
}

pub(crate) fn expand(pattern: &str) -> Vec<PathBuf> {
    let p = root().join(pattern);
    let mut v: Vec<PathBuf> = glob::glob(&p.to_string_lossy())
        .map(|g| g.filter_map(Result::ok).filter(|p| p.is_file()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

pub(crate) fn step(msg: &str) {
    println!("\n── {msg}");
}

/// This host's golden-set key (`<backend>-<vendor>`), asked of the renderer
/// the gates run (`renderer()`) ONCE. `None` when the renderer is not built or refused to answer — a
/// caller that needs it says which. Not resolved at manifest load: the
/// answer needs the product that `build` is about to make.
pub(crate) fn gpu_key() -> Option<&'static str> {
    static KEY: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    KEY.get_or_init(|| {
        let bin = renderer();
        if !bin.exists() {
            return None;
        }
        let out = Command::new(&bin).arg("--gpu-key").current_dir(native()).output().ok()?;
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let safe = !s.is_empty()
            && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        (out.status.success() && safe).then_some(s)
    })
    .as_deref()
}

/// The live hardware record (`--gpu-profile`), for the provenance NOTE.
pub(crate) fn gpu_profile() -> Option<&'static str> {
    static TEXT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    TEXT.get_or_init(|| {
        let bin = renderer();
        let out = Command::new(&bin).arg("--gpu-profile").current_dir(native()).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    })
    .as_deref()
}

/// A manifest path with `{gpu}` filled in for this host, or None if the key
/// is unknown. Paths without the token pass through unchanged.
pub(crate) fn resolve_gpu(path: &str) -> Option<String> {
    if path.contains("{gpu}") {
        Some(path.replace("{gpu}", gpu_key()?))
    } else {
        Some(path.to_string())
    }
}
