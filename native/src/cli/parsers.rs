use crate::fold;

/// `--wrap-mode` -> the layout parameter. Delegates to WrapMode's FromStr implementation.
pub fn parse_wrap_mode(s: &str) -> fold::WrapMode {
    s.parse()
        .unwrap_or_else(|e| panic!("--wrap-mode: {e} (clap should have refused it)"))
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

/// Parse strategy string into `Strategy`.
pub fn parse_strategy(s: &str) -> crate::repo::Strategy {
    s.parse().unwrap_or_else(|e| panic!("--repo-engine: {e} (clap should have refused it)"))
}

/// `--cluster-mode` -> the layout parameter. Delegates to ClusterMode's FromStr.
pub fn parse_cluster_mode(s: &str) -> fold::ClusterMode {
    s.parse()
        .unwrap_or_else(|e| panic!("--cluster-mode: {e} (clap should have refused it)"))
}

/// `--layout-mode` -> the layout parameter. Delegates to RepoLayoutMode's FromStr.
pub fn parse_layout_mode(s: &str) -> crate::repo::RepoLayoutMode {
    s.parse()
        .unwrap_or_else(|e| panic!("--layout-mode: {e} (clap should have refused it)"))
}

/// `--color-mode` -> the repo syntax color mode. Delegates to ColorMode's FromStr.
pub fn parse_color_mode(s: &str) -> crate::repo::ColorMode {
    s.parse()
        .unwrap_or_else(|e| panic!("--color-mode: {e} (clap should have refused it)"))
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

/// Parse windowed present mode string.
pub fn parse_present_mode(s: &str) -> wgpu::PresentMode {
    let mode: super::args::PresentMode = s
        .parse()
        .unwrap_or_else(|e| panic!("--present-mode: {e} (clap should have refused it)"));
    mode.into()
}
