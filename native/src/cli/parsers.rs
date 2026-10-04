use crate::fold;

/// `--wrap-mode` -> the layout parameter. clap's `value_parser` has already
/// refused anything that is not one of the two spellings, so an unknown value
/// here is a bug in this function rather than in the caller's command line —
/// which is why it panics instead of falling back to the default. A silent
/// fallback would render mode A while the operator believed they asked for B.
pub fn parse_wrap_mode(s: &str) -> fold::WrapMode {
    match s {
        "down" => fold::WrapMode::Down,
        "back" => fold::WrapMode::Back,
        other => panic!("--wrap-mode: unknown mode {other:?} (clap should have refused it)"),
    }
}

/// `--z-wrap-spacing` validation, run by clap at parse time. NaN must be
/// refused HERE, not left to `ItemParams::validate` at the layout seam:
/// clap's error names the flag and the value; the seam's panic would name
/// neither. Negative means the staircase steps FORWARD through the page
/// plane, which no baseline has ever rendered; refuse it rather than gate
/// nothing on a geometry nobody has looked at. 0 is legitimate — the
/// documented flat layout (`RepoParams::z_wrap_spacing`).
pub fn parse_z_wrap_spacing(s: &str) -> Result<f64, String> {
    let v: f64 = s
        .parse()
        .map_err(|_| format!("--z-wrap-spacing: {s:?} is not a number"))?;
    if !v.is_finite() || v < 0.0 {
        return Err(format!(
            "--z-wrap-spacing: {v} is out of domain (need a finite value >= 0)"
        ));
    }
    Ok(v)
}

/// Panics on an unknown strategy for the same reason `parse_wrap_mode` does:
/// clap has already refused anything else, so reaching here means the parser
/// and this match disagree, and silently loading with the wrong strategy would
/// make a verification run compare something other than what was asked for.
pub fn parse_strategy(s: &str) -> crate::repo::Strategy {
    use crate::repo::Strategy;
    match s {
        "naive" => Strategy::PerItem,
        "batch" => Strategy::Batched,
        "direct" => Strategy::Direct,
        "cubecl" => {
            #[cfg(feature = "cubecl")]
            {
                Strategy::Cubecl
            }
            #[cfg(not(feature = "cubecl"))]
            {
                eprintln!("error: --repo-engine cubecl was not compiled into this binary (rebuild with `cargo run --features cubecl`)");
                std::process::exit(1);
            }
        }
        "hyper" => Strategy::Hyper,
        other => panic!("--repo-engine: unknown mode {other:?} (clap should have refused it)"),
    }
}

/// `--cluster-mode` -> the layout parameter. Same shape as `parse_wrap_mode`:
/// clap has already refused anything that is not one of the two spellings, so
/// an unknown value here is a bug in this function rather than in the caller's
/// command line — a silent fallback would render mode A while the operator
/// believed they asked for B.
pub fn parse_cluster_mode(s: &str) -> fold::ClusterMode {
    match s {
        "leader" => fold::ClusterMode::Leader,
        "cluster" => fold::ClusterMode::Cluster,
        other => panic!("--cluster-mode: unknown mode {other:?} (clap should have refused it)"),
    }
}

/// `--layout-mode` -> the layout parameter. Same shape as `parse_wrap_mode`:
pub fn parse_layout_mode(s: &str) -> crate::repo::RepoLayoutMode {
    match s {
        "shelf" => crate::repo::RepoLayoutMode::Shelf,
        "carrel" => crate::repo::RepoLayoutMode::Carrel,
        other => panic!("--layout-mode: unknown mode {other:?} (clap should have refused it)"),
    }
}

/// `--color-mode` -> the repo syntax color mode.
pub fn parse_color_mode(s: &str) -> crate::repo::ColorMode {
    match s {
        "syntax" => crate::repo::ColorMode::Syntax,
        "flat" => crate::repo::ColorMode::Flat,
        other => panic!("--color-mode: unknown mode {other:?} (clap should have refused it)"),
    }
}

/// Parse comma-separated RGBA float string into `[f32; 4]`
pub fn parse_rgba(s: &str) -> Result<[f32; 4], String> {
    let clean = s.trim().trim_start_matches('[').trim_end_matches(']').trim_matches('"');
    let parts: Vec<&str> = clean.split(',').map(|p| p.trim()).collect();
    if parts.len() != 4 {
        return Err(format!("expected 4 comma-separated floats [r,g,b,a], got '{s}'"));
    }
    let mut out = [0.0f32; 4];
    for (i, p) in parts.iter().enumerate() {
        out[i] = p.parse::<f32>().map_err(|e| format!("invalid float '{p}': {e}"))?;
    }
    Ok(out)
}

/// clap has already refused anything outside the three spellings.
pub fn parse_present_mode(s: &str) -> wgpu::PresentMode {
    match s {
        "mailbox" => wgpu::PresentMode::Mailbox,
        "immediate" => wgpu::PresentMode::Immediate,
        _ => wgpu::PresentMode::Fifo,
    }
}
