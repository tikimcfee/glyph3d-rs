//! Differential fidelity and flood-throughput harness comparing `glyph_vt::Vt`
//! against `g3d-termd`'s server-side `libghostty-vt` screen on real programs,
//! mid-stream resizes (`80x24 -> 132x40 -> 60x20`), and mid-session `REPAINT`
//! subscriptions.
//!
//! Safety rules (`out/WALL-PLAN-2026-11-26.md`):
//! - Refuses namespaces `wall`, `agents`, `default`, and `rs-wall`.
//! - Requires an isolated `/tmp/g3d-vt-*` `G3D_TERMD_DIR` (`0700`, socket < 100 B).
//! - Stops the scratch daemon and cleans up any scratch `tmux` server on drop.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use glyph_vt::{Theme, Vt};
use term_cells::{attrs, Cell, MouseFormat, MouseTracking, Screen};
use termd_client::{
    find_termd, Capture, CaptureFormat, Colors, ConnectOptions, Create, Event, Key, Mode, Role,
    Subscribe, Target, Termd, MAX_SOCKET_PATH_BYTES,
};

const FORBIDDEN_NAMESPACES: [&str; 4] = ["wall", "agents", "default", "rs-wall"];

struct Config {
    termd_dir: PathBuf,
    termd_bin: Option<PathBuf>,
    ns: String,
    tmux_label: String,
    verbose: bool,
    only: Option<String>,
    skip_flood: bool,
}

fn parse_args() -> Config {
    let mut args = std::env::args().skip(1);
    let pid = std::process::id();
    let mut termd_dir = std::env::var_os("G3D_TERMD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/g3d-vt-{pid}")));
    let mut termd_bin = std::env::var_os("G3D_TERMD_BIN").map(PathBuf::from);
    let mut ns = "probe".to_owned();
    let mut tmux_label = format!("vt-diff-{pid}");
    let mut verbose = false;
    let mut only: Option<String> = None;
    let mut skip_flood = false;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--termd-dir" => {
                termd_dir = PathBuf::from(args.next().expect("--termd-dir requires path"));
            }
            "--termd-bin" => {
                termd_bin = Some(PathBuf::from(args.next().expect("--termd-bin requires path")));
            }
            "--ns" => {
                ns = args.next().expect("--ns requires namespace");
            }
            "--tmux-label" => {
                tmux_label = args.next().expect("--tmux-label requires label");
            }
            "--only" => {
                only = Some(args.next().expect("--only requires filter"));
            }
            "--skip-flood" => skip_flood = true,
            "-v" | "--verbose" => verbose = true,
            other => panic!("unknown argument: {other}"),
        }
    }

    assert!(
        !FORBIDDEN_NAMESPACES.contains(&ns.as_str()),
        "refusing forbidden namespace {ns:?}"
    );
    let dir_str = termd_dir.to_string_lossy();
    assert!(
        dir_str.starts_with("/tmp/g3d-vt-"),
        "refusing non-scratch termd_dir {dir_str:?} (must start with /tmp/g3d-vt-)"
    );

    Config {
        termd_dir,
        termd_bin,
        ns,
        tmux_label,
        verbose,
        only,
        skip_flood,
    }
}

fn locate_termd_bin(override_bin: Option<&Path>) -> PathBuf {
    if let Some(p) = override_bin {
        assert!(p.is_file(), "--termd-bin {} does not exist", p.display());
        return p.to_path_buf();
    }
    let local = PathBuf::from("/Users/lugo/.local/bin/g3d-termd");
    if local.is_file() {
        return local;
    }
    find_termd().expect("locate g3d-termd binary")
}

struct ScratchHarness {
    dir: PathBuf,
    ns: String,
    tmux_label: String,
    bin: PathBuf,
    owns_dir: bool,
}

impl ScratchHarness {
    fn new(cfg: &Config) -> Self {
        let bin = locate_termd_bin(cfg.termd_bin.as_deref());
        let owns_dir = !cfg.termd_dir.exists();
        if owns_dir {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&cfg.termd_dir)
                .expect("create scratch G3D_TERMD_DIR");
        }
        let sock = cfg.termd_dir.join(format!("termd-{}.sock", cfg.ns));
        assert!(
            sock.as_os_str().len() <= MAX_SOCKET_PATH_BYTES,
            "socket path too long: {}",
            sock.display()
        );
        std::env::set_var("G3D_TERMD_DIR", &cfg.termd_dir);
        std::env::set_var("G3D_TERMD_ENV_SOURCE", "daemon");
        Self {
            dir: cfg.termd_dir.clone(),
            ns: cfg.ns.clone(),
            tmux_label: cfg.tmux_label.clone(),
            bin,
            owns_dir,
        }
    }

    fn target(&self) -> Target {
        Target::namespace_in(&self.dir, &self.ns)
    }

    fn connect_or_start(&self, name: &str) -> Termd {
        let opts = ConnectOptions {
            termd_bin: Some(self.bin.clone()),
            startup_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(15),
            reconnect_window: Duration::from_secs(2),
        };
        Termd::connect_or_start_with(&self.target(), name, opts, Box::new(|| {}))
            .expect("connect_or_start to scratch termd")
    }

    fn connect_late(&self, name: &str) -> Termd {
        let opts = ConnectOptions {
            termd_bin: Some(self.bin.clone()),
            startup_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(10),
            reconnect_window: Duration::from_secs(1),
        };
        Termd::connect_with(&self.target(), name, opts, Box::new(|| {}))
            .expect("connect late subscriber to scratch termd")
    }
}

impl Drop for ScratchHarness {
    fn drop(&mut self) {
        if Path::new("/opt/homebrew/bin/tmux").is_file() {
            let _ = Command::new("/opt/homebrew/bin/tmux")
                .args(["-L", &self.tmux_label, "kill-server"])
                .stderr(std::process::Stdio::null())
                .status();
        }
        let opts = ConnectOptions {
            termd_bin: Some(self.bin.clone()),
            startup_timeout: Duration::from_secs(1),
            request_timeout: Duration::from_secs(3),
            reconnect_window: Duration::from_millis(100),
        };
        if let Ok(cleaner) = Termd::connect_with(&self.target(), "vt-diff-stop", opts, Box::new(|| {})) {
            let _ = cleaner.stop();
        }
        if self.owns_dir {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

#[derive(Clone, Debug)]
struct CheckFinding {
    scenario: String,
    checkpoint: String,
    category: &'static str,
    summary: String,
    sample_bytes_escaped: String,
}

#[derive(Clone, Debug)]
enum RecordedFrame {
    Repaint { cols: u16, rows: u16, bytes: Vec<u8> },
    Size { cols: u16, rows: u16 },
    Output(Vec<u8>),
}

struct LiveSession<'a> {
    harness: &'a ScratchHarness,
    client: &'a Termd,
    scenario: String,
    id: String,
    handle: u32,
    pid: u32,
    theme: Theme,
    vt: Vt,
    screen: Screen,
    last_seq: u64,
    server_modes: Option<(bool, bool, bool, bool, bool, bool)>,
    server_title: String,
    server_pwd: String,
    exited: bool,
    frames: Vec<RecordedFrame>,
    findings: Vec<CheckFinding>,
    checkpoints_run: usize,
    verbose: bool,
}

struct SpawnSpec<'s> {
    scenario: &'s str,
    id: &'s str,
    cols: u16,
    rows: u16,
    argv: Vec<String>,
    env: Option<BTreeMap<String, Option<String>>>,
    verbose: bool,
}

impl<'a> LiveSession<'a> {
    fn spawn(harness: &'a ScratchHarness, client: &'a Termd, spec: SpawnSpec<'_>) -> Self {
        let cwd = std::env::current_dir()
            .expect("cwd")
            .to_string_lossy()
            .into_owned();
        let create = Create {
            id: spec.id.to_owned(),
            argv: Some(spec.argv),
            cols: spec.cols,
            rows: spec.rows,
            cwd: Some(cwd),
            env: spec.env,
            colors: Some(Colors {
                fg: Some("#d8dee9".to_owned()),
                bg: Some("#0d1017".to_owned()),
                cursor: Some("#d8dee9".to_owned()),
            }),
            ..Create::default()
        };
        let created = client.create(&create).expect("termd create");
        let sub = client
            .subscribe_with(&Subscribe {
                id: spec.id.to_owned(),
                mode: Mode::Raw,
                role: Role::Own,
                history_rows: None,
                flow: None,
                cols: Some(spec.cols),
                rows: Some(spec.rows),
            })
            .expect("termd subscribe");

        let theme = Theme::default();
        let mut vt = Vt::new(spec.cols, spec.rows, &theme);
        let mut screen = Screen::new(spec.cols, spec.rows);
        vt.fill(&mut screen);

        Self {
            harness,
            client,
            scenario: spec.scenario.to_owned(),
            id: spec.id.to_owned(),
            handle: sub.handle,
            pid: created.pid,
            theme,
            vt,
            screen,
            last_seq: sub.seq,
            server_modes: None,
            server_title: String::new(),
            server_pwd: String::new(),
            exited: false,
            frames: Vec::new(),
            findings: Vec::new(),
            checkpoints_run: 0,
            verbose: spec.verbose,
        }
    }

    fn pump_available(&mut self) -> bool {
        let mut saw_any = false;
        while let Ok(ev) = self.client.events().try_recv() {
            match ev {
                Event::Repaint {
                    handle,
                    seq,
                    cols,
                    rows,
                    view: false,
                    bytes,
                } if handle == self.handle => {
                    saw_any = true;
                    self.last_seq = seq;
                    if (cols, rows) != (self.vt.cols(), self.vt.rows()) {
                        self.vt.resize(cols, rows);
                    }
                    self.vt.feed(&bytes);
                    self.frames.push(RecordedFrame::Repaint { cols, rows, bytes });
                }
                Event::Output { handle, seq, bytes } if handle == self.handle => {
                    saw_any = true;
                    self.last_seq = seq + bytes.len() as u64;
                    self.vt.feed(&bytes);
                    self.frames.push(RecordedFrame::Output(bytes));
                }
                Event::Size {
                    handle,
                    cols,
                    rows,
                    seq,
                } if handle == self.handle => {
                    saw_any = true;
                    self.last_seq = seq;
                    self.vt.resize(cols, rows);
                    self.frames.push(RecordedFrame::Size { cols, rows });
                }
                Event::Modes {
                    handle,
                    alt,
                    mouse,
                    mouse_sgr,
                    bracketed_paste,
                    app_cursor,
                    focus,
                } if handle == self.handle => {
                    saw_any = true;
                    self.server_modes =
                        Some((alt, mouse, mouse_sgr, bracketed_paste, app_cursor, focus));
                }
                Event::Title { handle, title } if handle == self.handle => {
                    saw_any = true;
                    self.server_title = title;
                }
                Event::Pwd { handle, pwd } if handle == self.handle => {
                    saw_any = true;
                    self.server_pwd = pwd;
                }
                Event::Exit { handle, seq, .. } if handle == self.handle => {
                    saw_any = true;
                    self.last_seq = self.last_seq.max(seq);
                    self.exited = true;
                }
                Event::Closed { handle } if handle == self.handle => {
                    saw_any = true;
                    self.exited = true;
                }
                _ => {}
            }
        }
        saw_any
    }

    fn wait_quiet(&mut self, settle_ms: u64, max_ms: u64) {
        let start = Instant::now();
        let mut last_activity = Instant::now();
        let settle = Duration::from_millis(settle_ms);
        let max_dur = Duration::from_millis(max_ms);

        loop {
            if self.pump_available() {
                last_activity = Instant::now();
            }
            if last_activity.elapsed() >= settle {
                // Confirm the server's PTY byte counter matches our `last_seq` so no output
                // is mid-flight between termd's PTY reader and our socket.
                let server_caught_up = self
                    .client
                    .list()
                    .ok()
                    .and_then(|list| list.into_iter().find(|t| t.id == self.id))
                    .map(|info| info.seq == self.last_seq)
                    .unwrap_or(true);
                if server_caught_up && !self.pump_available() {
                    break;
                }
            }
            if start.elapsed() >= max_dur {
                self.pump_available();
                break;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    fn send_bytes(&mut self, bytes: &[u8]) {
        self.client
            .input(self.handle, bytes)
            .expect("termd input bytes");
    }

    fn send_key(&mut self, code: &str, key: &str, mods: &[&str], text: &str) {
        let k = Key {
            code: code.to_owned(),
            key: key.to_owned(),
            mods: mods.iter().map(|s| (*s).to_owned()).collect(),
            text: text.to_owned(),
            ..Key::default()
        };
        self.client.key(self.handle, &k).expect("termd send key");
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.client
            .resize(self.handle, cols, rows)
            .expect("termd resize");
    }

    fn signal_process(&self, sig: &str) {
        // Send to process group first (`-pid`), falling back to `pid`.
        let pgrp = format!("-{}", self.pid);
        let ok = Command::new("/bin/kill")
            .args([sig, &pgrp])
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            let _ = Command::new("/bin/kill")
                .args([sig, &self.pid.to_string()])
                .stderr(std::process::Stdio::null())
                .status();
        }
    }

    fn recent_bytes_preview(&self, limit: usize) -> String {
        let mut parts: Vec<String> = Vec::new();
        let mut collected = 0usize;
        for f in self.frames.iter().rev() {
            match f {
                RecordedFrame::Output(b) => {
                    let take = b.len().min(limit.saturating_sub(collected));
                    parts.push(escape_bytes(&b[b.len() - take..]));
                    collected += take;
                }
                RecordedFrame::Repaint { cols, rows, bytes } => {
                    let take = bytes.len().min(limit.saturating_sub(collected));
                    parts.push(format!(
                        "<REPAINT {cols}x{rows}>{}",
                        escape_bytes(&bytes[bytes.len() - take..])
                    ));
                    collected += take;
                }
                RecordedFrame::Size { cols, rows } => {
                    parts.push(format!("<SIZE {cols}x{rows}>"));
                }
            }
            if collected >= limit {
                break;
            }
        }
        parts.reverse();
        parts.join("")
    }

    fn checkpoint(&mut self, label: &str) {
        self.wait_quiet(180, 4_000);
        self.checkpoint_settled(label);
    }

    fn checkpoint_settled(&mut self, label: &str) {
        self.checkpoints_run += 1;
        self.vt.fill(&mut self.screen);

        let cols = self.screen.cols;
        let rows = self.screen.rows;

        // 1. Server plain-text capture (`unwrap: false`) vs `Vt` row text.
        let cap_plain = self
            .client
            .capture(&Capture {
                id: self.id.clone(),
                format: Some(CaptureFormat::Plain),
                history_rows: Some(0),
                unwrap: Some(false),
            })
            .expect("termd capture plain");

        let vt_plain = format_vt_plain(&self.screen);
        let norm_termd_plain = normalize_trailing_blank_lines(&cap_plain.text);
        let norm_vt_plain = normalize_trailing_blank_lines(&vt_plain);
        if norm_termd_plain != norm_vt_plain {
            let detail = diff_lines(norm_termd_plain, norm_vt_plain);
            self.record_finding(label, "capture-plain", detail);
        }

        // 2. Server soft-wrap-unwrapped capture (`unwrap: true`) vs `Vt::text(0, 0, cols, rows - 1)`.
        let cap_unwrap = self
            .client
            .capture(&Capture {
                id: self.id.clone(),
                format: Some(CaptureFormat::Plain),
                history_rows: Some(0),
                unwrap: Some(true),
            })
            .expect("termd capture unwrap");
        let vt_unwrap = self.vt.text(0, 0, cols, rows.saturating_sub(1));
        let norm_termd_unwrap = normalize_trailing_blank_lines(&cap_unwrap.text);
        let norm_vt_unwrap = normalize_trailing_blank_lines(&vt_unwrap);
        if norm_termd_unwrap != norm_vt_unwrap {
            let detail = diff_lines(norm_termd_unwrap, norm_vt_unwrap);
            self.record_finding(label, "capture-unwrap (soft-wrap)", detail);
        }

        // 3. Mid-session late subscriber (`REPAINT` only) into a fresh `Vt`:
        //    compares every cell (content/cluster, width, fg, bg, ul, attrs),
        //    soft-wrap flags, cursor (col, row, visible, shape, blink, cursor_color),
        //    modes, and title!
        let late_client = self.harness.connect_late("vt-diff-late");
        let late_sub = late_client
            .subscribe_with(&Subscribe {
                id: self.id.clone(),
                mode: Mode::Raw,
                role: Role::Watch,
                history_rows: None,
                flow: None,
                cols: None,
                rows: None,
            })
            .expect("late subscribe");

        let mut repaint_bytes: Option<(u16, u16, Vec<u8>)> = None;
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(Event::Repaint {
                handle,
                cols: rc,
                rows: rr,
                bytes,
                ..
            }) = late_client.events().recv_timeout(Duration::from_millis(200))
            {
                if handle == late_sub.handle {
                    repaint_bytes = Some((rc, rr, bytes));
                    break;
                }
            }
        }
        let _ = late_client.unsubscribe(late_sub.handle);
        drop(late_client);

        if let Some((rc, rr, rp_bytes)) = repaint_bytes {
            let mut vt_rp = Vt::new(rc, rr, &self.theme);
            let mut screen_rp = Screen::new(rc, rr);
            vt_rp.feed(&rp_bytes);
            vt_rp.fill(&mut screen_rp);

            // Also verify whether `vt_rp`'s plain text matches `termd`'s `capture(plain)`!
            let rp_plain = format_vt_plain(&screen_rp);
            let norm_rp_plain = normalize_trailing_blank_lines(&rp_plain);
            if norm_rp_plain != norm_termd_plain {
                let detail = format!(
                    "late REPAINT text != termd capture(plain): {}",
                    diff_lines(norm_termd_plain, norm_rp_plain)
                );
                self.record_finding(label, "repaint-vs-capture-plain", detail);
            }

            let screen_diffs = diff_screens(&self.screen, &screen_rp);
            for (cat, msg) in screen_diffs {
                let mut full_msg = msg;
                if self.verbose {
                    let _ = write!(
                        full_msg,
                        " [repaint_tail={}]",
                        escape_bytes(&rp_bytes[rp_bytes.len().saturating_sub(160)..])
                    );
                }
                self.record_finding(label, cat, full_msg);
            }
        } else {
            self.record_finding(
                label,
                "repaint-missing",
                "late subscriber received no REPAINT frame within 3s".to_owned(),
            );
        }

        // 4. Server `Event::Modes` & `list()` metadata vs `Vt` modes/title.
        if let Some((alt, mouse, mouse_sgr, paste, app_cursor, focus)) = self.server_modes {
            let m = self.screen.modes;
            let vt_mouse = m.mouse != MouseTracking::Off;
            let vt_sgr = matches!(m.mouse_format, MouseFormat::Sgr | MouseFormat::SgrPixels);
            if m.alt_screen != alt
                || vt_mouse != mouse
                || (vt_mouse && vt_sgr != mouse_sgr)
                || m.bracketed_paste != paste
                || m.app_cursor != app_cursor
                || m.focus_events != focus
            {
                self.record_finding(
                    label,
                    "modes-event",
                    format!(
                        "termd Event::Modes=(alt={alt}, mouse={mouse}, sgr={mouse_sgr}, paste={paste}, app_cursor={app_cursor}, focus={focus}) vs glyph-vt={m:?}"
                    ),
                );
            }
        }

        if self.verbose {
            eprintln!(
                "  [checkpoint] {}/{label} ({cols}x{rows}, seq={}): {} finding(s) so far",
                self.scenario,
                self.last_seq,
                self.findings.len()
            );
        }
    }

    fn record_finding(&mut self, checkpoint: &str, category: &'static str, summary: String) {
        let sample = self.recent_bytes_preview(200);
        self.findings.push(CheckFinding {
            scenario: self.scenario.clone(),
            checkpoint: checkpoint.to_owned(),
            category,
            summary,
            sample_bytes_escaped: sample,
        });
    }

    fn finish(self) -> (usize, Vec<CheckFinding>) {
        let _ = self.client.unsubscribe(self.handle);
        let _ = self.client.kill_with_signal(&self.id, "KILL");
        (self.checkpoints_run, self.findings)
    }
}

fn format_vt_plain(screen: &Screen) -> String {
    let mut lines = Vec::with_capacity(usize::from(screen.rows));
    for r in 0..screen.rows {
        let mut line = String::new();
        for cell in screen.row(r) {
            if cell.width == 0 {
                continue;
            }
            if cell.content == 0 {
                line.push(' ');
            } else {
                line.push_str(&screen.content_str(cell));
            }
        }
        let trimmed = line.trim_end_matches(' ');
        lines.push(trimmed.to_owned());
    }
    lines.join("\n")
}

fn normalize_trailing_blank_lines(s: &str) -> &str {
    s.trim_end_matches('\n')
}

fn diff_lines(termd_str: &str, vt_str: &str) -> String {
    let t_lines: Vec<&str> = termd_str.split('\n').collect();
    let v_lines: Vec<&str> = vt_str.split('\n').collect();
    let max_len = t_lines.len().max(v_lines.len());
    let mut parts = Vec::new();
    for idx in 0..max_len {
        let tl = t_lines.get(idx).copied().unwrap_or("<EOF>");
        let vl = v_lines.get(idx).copied().unwrap_or("<EOF>");
        if tl != vl {
            parts.push(format!("row {idx}: termd={tl:?} vs glyph-vt={vl:?}"));
            if parts.len() >= 3 {
                break;
            }
        }
    }
    parts.join("; ")
}

fn fmt_cell(screen: &Screen, cell: &Cell) -> String {
    let txt = if cell.width == 0 {
        "<tail>".to_owned()
    } else if cell.content == 0 {
        " ".to_owned()
    } else {
        screen.content_str(cell).into_owned()
    };
    let ul = attrs::underline(cell.attrs);
    format!(
        "txt={txt:?} w={} fg={:#010x} bg={:#010x} attrs={:#06x}(ul={ul}) ul_col={:#010x}",
        cell.width, cell.fg, cell.bg, cell.attrs, cell.ul
    )
}

fn diff_screens(live: &Screen, rp: &Screen) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    if (live.cols, live.rows) != (rp.cols, rp.rows) {
        out.push((
            "screen-size",
            format!(
                "live={}x{} vs repaint={}x{}",
                live.cols, live.rows, rp.cols, rp.rows
            ),
        ));
        return out;
    }

    let mut cell_mismatches = Vec::new();
    let mut total_cell_diffs = 0usize;
    for r in 0..live.rows {
        let lr = live.row(r);
        let rr = rp.row(r);
        for c in 0..usize::from(live.cols) {
            let lc = &lr[c];
            let rc = &rr[c];
            let l_txt = live.content_str(lc);
            let r_txt = rp.content_str(rc);
            let ul_diff = (lc.attrs & attrs::UNDERLINE_COLOR != 0) && lc.ul != rc.ul;
            if l_txt != r_txt
                || lc.width != rc.width
                || lc.fg != rc.fg
                || lc.bg != rc.bg
                || lc.attrs != rc.attrs
                || ul_diff
            {
                total_cell_diffs += 1;
                if cell_mismatches.len() < 4 {
                    cell_mismatches.push(format!(
                        "({c},{r}): live[{}] != termd_repaint[{}]",
                        fmt_cell(live, lc),
                        fmt_cell(rp, rc)
                    ));
                }
            }
        }
    }
    if total_cell_diffs > 0 {
        out.push((
            "cells",
            format!(
                "{total_cell_diffs} cell(s) differ: {}",
                cell_mismatches.join(" | ")
            ),
        ));
    }

    let mut wrap_diffs = Vec::new();
    for r in 0..usize::from(live.rows) {
        if live.wrapped[r] != rp.wrapped[r] {
            wrap_diffs.push(format!(
                "row {r}: live={} vs repaint={}",
                live.wrapped[r], rp.wrapped[r]
            ));
        }
    }
    if !wrap_diffs.is_empty() {
        out.push(("wrapped-rows", wrap_diffs.join(", ")));
    }

    if live.cursor != rp.cursor {
        out.push((
            "cursor",
            format!("live={:?} vs termd_repaint={:?}", live.cursor, rp.cursor),
        ));
    }

    if live.cursor_color != rp.cursor_color {
        out.push((
            "cursor-color",
            format!(
                "live={:#010x} vs termd_repaint={:#010x}",
                live.cursor_color, rp.cursor_color
            ),
        ));
    }

    if live.default_fg != rp.default_fg || live.default_bg != rp.default_bg {
        out.push((
            "default-colors",
            format!(
                "live=({:#010x},{:#010x}) vs termd_repaint=({:#010x},{:#010x})",
                live.default_fg, live.default_bg, rp.default_fg, rp.default_bg
            ),
        ));
    }

    if live.modes != rp.modes {
        out.push((
            "modes-repaint",
            format!("live={:?} vs termd_repaint={:?}", live.modes, rp.modes),
        ));
    }

    if live.title != rp.title {
        out.push((
            "title-repaint",
            format!("live={:?} vs termd_repaint={:?}", live.title, rp.title),
        ));
    }

    out
}

fn escape_bytes(bytes: &[u8]) -> String {
    let mut s = String::new();
    for &b in bytes {
        match b {
            b'\x1b' => s.push_str("\\x1b"),
            b'\r' => s.push_str("\\r"),
            b'\n' => s.push_str("\\n"),
            b'\t' => s.push_str("\\t"),
            b'\\' => s.push_str("\\\\"),
            0x20..=0x7e => s.push(b as char),
            other => {
                let _ = write!(s, "\\x{other:02x}");
            }
        }
    }
    s
}

fn run_resize_trio(sess: &mut LiveSession<'_>, prefix: &str) {
    sess.checkpoint(&format!("{prefix}@80x24"));
    sess.resize(132, 40);
    sess.checkpoint(&format!("{prefix}@132x40"));
    sess.resize(60, 20);
    sess.checkpoint(&format!("{prefix}@60x20"));
}

// ── Scenarios ────────────────────────────────────────────────────────────────

fn scenario_zsh_wrapped_prompt(
    harness: &ScratchHarness,
    client: &Termd,
    verbose: bool,
) -> (usize, Vec<CheckFinding>) {
    let mut env = BTreeMap::new();
    env.insert("ZDOTDIR".to_owned(), Some("/var/empty".to_owned()));
    let mut sess = LiveSession::spawn(
        harness,
        client,
        SpawnSpec {
            scenario: "zsh-wrapped-prompt",
            id: "s-zsh",
            cols: 80,
            rows: 24,
            argv: vec!["/bin/zsh".to_owned(), "-f".to_owned(), "-i".to_owned()],
            env: Some(env),
            verbose,
        },
    );
    sess.wait_quiet(250, 3_000);
    // Configure a deterministic coloured ZLE prompt and clear screen.
    sess.send_bytes(
        b"PROMPT='%F{green}wall%f:%F{cyan}~/glyph3d%f %B$%b '; clear\r",
    );
    sess.wait_quiet(250, 3_000);
    // Type a 175-char command line without pressing Enter so ZLE soft-wraps across multiple rows.
    sess.send_bytes(
        b"cargo test --release -p glyph-vt -p wall-core -- --nocapture && printf 'wrapped-line-segment-alpha-bravo-charlie-delta-echo-foxtrot-golf-hotel-india-juliet-kilo'",
    );
    run_resize_trio(&mut sess, "zle-wrapped");
    sess.send_key("KeyC", "c", &["Control"], "c");
    sess.checkpoint("after-ctrl-c@60x20");
    sess.finish()
}

fn scenario_vim(
    harness: &ScratchHarness,
    client: &Termd,
    verbose: bool,
) -> (usize, Vec<CheckFinding>) {
    let mut sess = LiveSession::spawn(
        harness,
        client,
        SpawnSpec {
            scenario: "vim-split-visual",
            id: "s-vim",
            cols: 80,
            rows: 24,
            argv: vec![
                "/usr/bin/vim".to_owned(),
                "-u".to_owned(),
                "NONE".to_owned(),
                "-N".to_owned(),
                "-n".to_owned(),
                "--cmd".to_owned(),
                "syntax on | set number ruler laststatus=2 noshowmode".to_owned(),
                "crates/glyph-vt/src/lib.rs".to_owned(),
            ],
            env: None,
            verbose,
        },
    );
    sess.checkpoint("open@80x24");
    // Page down twice (`Ctrl+F`), open vertical + horizontal splits, and visually select words.
    sess.send_bytes(b"\x06\x06");
    sess.checkpoint("paged@80x24");
    sess.send_bytes(b":vsplit\r:split\r");
    sess.checkpoint("splits@80x24");
    sess.send_bytes(b"gg4j0v3w");
    run_resize_trio(&mut sess, "visual-splits");
    // Quit all splits back to clean state.
    sess.send_bytes(b"\x1b:qa!\r");
    sess.wait_quiet(200, 2_000);
    sess.finish()
}

fn scenario_less_search(
    harness: &ScratchHarness,
    client: &Termd,
    verbose: bool,
) -> (usize, Vec<CheckFinding>) {
    let mut env = BTreeMap::new();
    env.insert("LESSHISTFILE".to_owned(), Some("-".to_owned()));
    let mut sess = LiveSession::spawn(
        harness,
        client,
        SpawnSpec {
            scenario: "less-search",
            id: "s-less",
            cols: 80,
            rows: 24,
            argv: vec![
                "/usr/bin/less".to_owned(),
                "-R".to_owned(),
                "-N".to_owned(),
                "crates/glyph-vt/src/term/terminal.rs".to_owned(),
            ],
            env: Some(env),
            verbose,
        },
    );
    sess.checkpoint("open@80x24");
    sess.send_bytes(b"/scroll_up\r");
    sess.checkpoint("search-match-1@80x24");
    sess.send_bytes(b"nn");
    run_resize_trio(&mut sess, "search-match-3");
    sess.send_bytes(b"q");
    sess.wait_quiet(150, 2_000);
    sess.finish()
}

fn scenario_top_3s(
    harness: &ScratchHarness,
    client: &Termd,
    verbose: bool,
) -> (usize, Vec<CheckFinding>) {
    let mut sess = LiveSession::spawn(
        harness,
        client,
        SpawnSpec {
            scenario: "top-3s",
            id: "s-top",
            cols: 80,
            rows: 24,
            argv: vec!["/usr/bin/top".to_owned(), "-s".to_owned(), "1".to_owned()],
            env: None,
            verbose,
        },
    );
    // Let `top` run live for 3 seconds across 3+ full screen refreshes.
    let end = Instant::now() + Duration::from_secs(3);
    while Instant::now() < end {
        sess.pump_available();
        std::thread::sleep(Duration::from_millis(50));
    }

    // Freeze `top` at each size checkpoint so server `capture` and `vt` compare at exact `seq`.
    sess.signal_process("-STOP");
    sess.wait_quiet(120, 2_000);
    sess.checkpoint_settled("after-3s@80x24");
    sess.signal_process("-CONT");

    sess.resize(132, 40);
    std::thread::sleep(Duration::from_millis(1_150));
    sess.signal_process("-STOP");
    sess.wait_quiet(120, 2_000);
    sess.checkpoint_settled("resized@132x40");
    sess.signal_process("-CONT");

    sess.resize(60, 20);
    std::thread::sleep(Duration::from_millis(1_150));
    sess.signal_process("-STOP");
    sess.wait_quiet(120, 2_000);
    sess.checkpoint_settled("resized@60x20");
    sess.signal_process("-CONT");

    sess.send_bytes(b"q");
    sess.wait_quiet(150, 1_500);
    sess.finish()
}

fn scenario_tmux_two_panes(
    harness: &ScratchHarness,
    client: &Termd,
    tmux_label: &str,
    verbose: bool,
) -> (usize, Vec<CheckFinding>) {
    if !Path::new("/opt/homebrew/bin/tmux").is_file() {
        return (0, Vec::new());
    }
    let mut env = BTreeMap::new();
    env.insert("SHELL".to_owned(), Some("/bin/sh".to_owned()));
    let mut sess = LiveSession::spawn(
        harness,
        client,
        SpawnSpec {
            scenario: "tmux-two-panes",
            id: "s-tmux",
            cols: 80,
            rows: 24,
            argv: vec![
                "/opt/homebrew/bin/tmux".to_owned(),
                "-L".to_owned(),
                tmux_label.to_owned(),
                "-f".to_owned(),
                "/dev/null".to_owned(),
                "new-session".to_owned(),
                "printf '\\033[1;32mLEFT-PANE\\033[0m\\n1234567890abcdef\\n'; cat".to_owned(),
                ";".to_owned(),
                "split-window".to_owned(),
                "-h".to_owned(),
                "printf '\\033[1;36mRIGHT-PANE\\033[0m\\n漢字 🚀 cafe\\314\\201\\n'; cat".to_owned(),
            ],
            env: Some(env),
            verbose,
        },
    );
    sess.wait_quiet(400, 4_000);
    run_resize_trio(&mut sess, "two-panes");
    let _ = Command::new("/opt/homebrew/bin/tmux")
        .args(["-L", tmux_label, "kill-server"])
        .stderr(std::process::Stdio::null())
        .status();
    sess.finish()
}

fn scenario_git_log_graph(
    harness: &ScratchHarness,
    client: &Termd,
    verbose: bool,
) -> (usize, Vec<CheckFinding>) {
    let git_bin = if Path::new("/Library/Developer/CommandLineTools/usr/bin/git").is_file() {
        "/Library/Developer/CommandLineTools/usr/bin/git"
    } else {
        "/usr/bin/git"
    };
    let script = format!(
        "stty -echo; {git_bin} --no-pager log --graph --color=always --decorate --oneline -n 35; exec sleep 60"
    );
    let mut sess = LiveSession::spawn(
        harness,
        client,
        SpawnSpec {
            scenario: "git-log-graph",
            id: "s-git",
            cols: 80,
            rows: 24,
            argv: vec!["/bin/sh".to_owned(), "-c".to_owned(), script],
            env: None,
            verbose,
        },
    );
    run_resize_trio(&mut sess, "graph-color");
    sess.finish()
}

fn build_unicode_specimen(mode_2027: Option<bool>) -> Vec<u8> {
    let mut out = Vec::new();
    match mode_2027 {
        Some(true) => out.extend_from_slice(b"\x1b[?2027h"),
        Some(false) => out.extend_from_slice(b"\x1b[?2027l"),
        None => {}
    }
    out.extend_from_slice(
        "CJK: 漢字仮名交じり文 | Emoji: 😀 🚀 🎉 | Skin: 👋🏽 | Flags: 🇺🇸 🇯🇵\r\n".as_bytes(),
    );
    out.extend_from_slice(
        "VS15/16: [⌚] [⌚\u{FE0E}] [❤] [❤\u{FE0F}] | Keycap: [1\u{FE0F}\u{20E3}]\r\n".as_bytes(),
    );
    out.extend_from_slice(
        "ZWJ: [👨‍👩‍👧‍👦] [👩‍🔬] [🏳️‍🌈]\r\n".as_bytes(),
    );
    out.extend_from_slice(
        "Marks: cafe\u{0301} Vie\u{0323}\u{0302}t q\u{0308}\u{0304} n\u{0303} 漢\u{0301}\r\n"
            .as_bytes(),
    );
    // Odd-column edge wrap of wide CJK at column 79 (0-based) on an 80-col terminal:
    out.extend_from_slice(
        format!("{}漢語wrap-after-spacer\r\n", "a".repeat(79)).as_bytes(),
    );
    // All 5 underline styles + SGR 58 underline colour + inverse + overline + strike + OSC 8 + OSC 12:
    out.extend_from_slice(
        b"\x1b[1mbold\x1b[0;2mdim\x1b[0;3mital\x1b[0;7minv\x1b[0;9mstrk\x1b[0;53movrl\x1b[0m \
          \x1b[4:1mu1\x1b[4:2mu2\x1b[4:3;58;2;255;80;40mu3c\x1b[4:4mu4\x1b[4:5mu5\x1b[59;24m \
          \x1b]8;;https://glyph3d.dev\x07link\x1b]8;;\x07\x1b]12;#44cc88\x07\r\n",
    );
    out
}

fn scenario_unicode(
    harness: &ScratchHarness,
    client: &Termd,
    mode_2027: Option<bool>,
    verbose: bool,
) -> (usize, Vec<CheckFinding>) {
    let (name, id) = match mode_2027 {
        None => ("unicode-initial-default", "s-uniinit"),
        Some(false) => ("unicode-mode2027-off", "s-unidef"),
        Some(true) => ("unicode-mode2027-on", "s-uni2027"),
    };
    let payload_path = harness.dir.join(format!("{id}.bin"));
    std::fs::write(&payload_path, build_unicode_specimen(mode_2027))
        .expect("write unicode specimen");

    let script = format!(
        "stty raw -echo; cat '{}'; exec sleep 60",
        payload_path.display()
    );
    let mut sess = LiveSession::spawn(
        harness,
        client,
        SpawnSpec {
            scenario: name,
            id,
            cols: 80,
            rows: 24,
            argv: vec!["/bin/sh".to_owned(), "-c".to_owned(), script],
            env: None,
            verbose,
        },
    );
    run_resize_trio(&mut sess, name);
    sess.finish()
}

fn scenario_tabs(
    harness: &ScratchHarness,
    client: &Termd,
    verbose: bool,
) -> (usize, Vec<CheckFinding>) {
    let mut payload = Vec::new();
    // Row 0: default 8-col stops.
    payload.extend_from_slice(b"a\tb\tc\t12345678\td\r\n");
    // Row 1: background SGR 44 across `\t` (verifies cursor-skip vs space-write BCE semantics!).
    payload.extend_from_slice(b"pre\x1b[44m\t\tpost\x1b[0m\r\n");
    // Row 2: CHT (`CSI 2 I`) and CBT (`CSI Z`) over pre-printed text (tabs must not erase skipped cells!).
    payload.extend_from_slice(b"0123456789abcdef0123456789\r\tX\x1b[2IY\x1b[ZZ\r\n");
    // Row 3: clear all stops (`CSI 3 g`), set custom stops (`ESC H`) at cols 5, 13, 29 (0-based 4, 12, 28).
    payload.extend_from_slice(b"\x1b[3g\x1b[4;5H\x1bH\x1b[4;13H\x1bH\x1b[4;29H\x1bH\x1b[4;1H*\tA\tB\tC\r\n");
    // Row 4: clear single stop at col 13 (`CSI 0 g`) and verify `\t` jumps from 5 -> 29.
    payload.extend_from_slice(b"\x1b[5;13H\x1b[0g\x1b[5;1H*\tA\tB\r\n");

    let payload_path = harness.dir.join("s-tabs.bin");
    std::fs::write(&payload_path, payload).expect("write tab specimen");
    let script = format!(
        "stty raw -echo; cat '{}'; exec sleep 60",
        payload_path.display()
    );
    let mut sess = LiveSession::spawn(
        harness,
        client,
        SpawnSpec {
            scenario: "tab-stops",
            id: "s-tabs",
            cols: 80,
            rows: 24,
            argv: vec!["/bin/sh".to_owned(), "-c".to_owned(), script],
            env: None,
            verbose,
        },
    );
    run_resize_trio(&mut sess, "tabs");
    sess.finish()
}

// ── Flood Throughput Benchmarks ──────────────────────────────────────────────

struct FloodResult {
    label: &'static str,
    bytes: usize,
    cpu_60hz_mbs: f64,
    cpu_every_chunk_mbs: f64,
    wall_60hz_mbs: f64,
    feed_pct: f64,
    fill_pct: f64,
    fills_60hz: usize,
    live_termd_mbs: f64,
    live_rx_bytes: usize,
}

fn build_20mb_source_corpus() -> Vec<u8> {
    let seed_files = [
        "crates/glyph-vt/src/term/terminal.rs",
        "crates/glyph-vt/src/lib.rs",
        "crates/wall-core/src/selection.rs",
        "crates/termd-client/src/client.rs",
    ];
    let mut unit = Vec::with_capacity(256 * 1024);
    for path in seed_files {
        if let Ok(text) = std::fs::read_to_string(path) {
            for line in text.lines() {
                unit.extend_from_slice(line.as_bytes());
                unit.extend_from_slice(b"\r\n");
            }
        }
    }
    let target = 20 * 1_000_000;
    let mut out = Vec::with_capacity(target + unit.len());
    while out.len() < target {
        let rem = target - out.len();
        out.extend_from_slice(&unit[..rem.min(unit.len())]);
    }
    out
}

fn build_20mb_color_corpus() -> Vec<u8> {
    let mut unit = String::with_capacity(256 * 1024);
    for i in 0..1_500u32 {
        let r = ((i * 37) & 0xff) as u8;
        let g = ((i * 73) & 0xff) as u8;
        let b = ((i * 19) & 0xff) as u8;
        let p = (16 + (i % 216)) as u8;
        let _ = write!(
            unit,
            "\x1b[38;5;242m{i:06}\x1b[0m \x1b[1;34mpub fn\x1b[0m \
             \x1b[38;2;{r};{g};{b}mrender_span_{i:04}\x1b[0m\
             (\x1b[3;33mscreen\x1b[0m: \x1b[32m&mut Screen\x1b[0m) -> \
             \x1b[4:3;58;5;{p};38;5;{p}mbool\x1b[0m \x1b[7;36m PASS \x1b[0m\r\n"
        );
    }
    let unit_bytes = unit.into_bytes();
    let target = 20 * 1_000_000;
    let mut out = Vec::with_capacity(target + unit_bytes.len());
    while out.len() < target {
        let rem = target - out.len();
        out.extend_from_slice(&unit_bytes[..rem.min(unit_bytes.len())]);
    }
    out
}

fn bench_flood_stream(
    harness: &ScratchHarness,
    client: &Termd,
    label: &'static str,
    id: &str,
    stream: &[u8],
    live_cmd: Vec<String>,
) -> FloodResult {
    let cols = 120u16;
    let rows = 40u16;
    let chunk_size = 64 * 1024;
    let theme = Theme::default();
    let chunks: Vec<&[u8]> = stream.chunks(chunk_size).collect();

    // 1. In-memory `Vt::feed` + 60 Hz frame-paced `Vt::fill`.
    //    Record both whole-pass wall-clock best AND per-chunk minimum across 5 passes
    //    so heavy sibling load preemption on shared hosts does not hide true CPU cost.
    let frame_interval = Duration::from_micros(16_667);
    let mut min_chunk_feed = vec![Duration::MAX; chunks.len()];
    let mut min_single_fill = Duration::MAX;
    let mut best_wall_60hz = Duration::MAX;

    for _ in 0..5 {
        let mut vt = Vt::new(cols, rows, &theme);
        let mut screen = Screen::new(cols, rows);
        vt.fill(&mut screen);

        let mut pass_wall = Duration::ZERO;
        let mut last_frame = Instant::now();

        for (idx, &chunk) in chunks.iter().enumerate() {
            let t0 = Instant::now();
            vt.feed(std::hint::black_box(chunk));
            let dt = t0.elapsed();
            pass_wall += dt;
            if dt < min_chunk_feed[idx] {
                min_chunk_feed[idx] = dt;
            }

            if last_frame.elapsed() >= frame_interval {
                let t1 = Instant::now();
                std::hint::black_box(vt.fill(std::hint::black_box(&mut screen)));
                let fdt = t1.elapsed();
                pass_wall += fdt;
                if fdt < min_single_fill {
                    min_single_fill = fdt;
                }
                last_frame = Instant::now();
            }
        }
        let t1 = Instant::now();
        std::hint::black_box(vt.fill(std::hint::black_box(&mut screen)));
        let fdt = t1.elapsed();
        pass_wall += fdt;
        if fdt < min_single_fill {
            min_single_fill = fdt;
        }
        best_wall_60hz = best_wall_60hz.min(pass_wall);
    }

    let cpu_feed_total: Duration = min_chunk_feed.iter().copied().sum();
    let fills_at_60hz = ((cpu_feed_total.as_secs_f64() * 60.0).ceil() as usize).max(1);
    let cpu_fill_60hz_total = min_single_fill.saturating_mul(fills_at_60hz as u32);
    let cpu_60hz_total = cpu_feed_total + cpu_fill_60hz_total;

    // 2. In-memory `Vt::feed` + `Vt::fill` after EVERY 64 KiB read chunk (per-chunk min across 4 passes).
    let mut min_chunk_both = vec![Duration::MAX; chunks.len()];
    for _ in 0..4 {
        let mut vt = Vt::new(cols, rows, &theme);
        let mut screen = Screen::new(cols, rows);
        vt.fill(&mut screen);
        for (idx, &chunk) in chunks.iter().enumerate() {
            let t0 = Instant::now();
            vt.feed(std::hint::black_box(chunk));
            std::hint::black_box(vt.fill(std::hint::black_box(&mut screen)));
            let dt = t0.elapsed();
            if dt < min_chunk_both[idx] {
                min_chunk_both[idx] = dt;
            }
        }
    }
    let cpu_every_chunk_total: Duration = min_chunk_both.iter().copied().sum();

    // 3. Live end-to-end PTY -> `g3d-termd` (`Role::Own`, `flow: "block"`) -> Unix socket -> `Vt::feed` + 60 Hz `fill`.
    let _ = harness;
    let created = client
        .create(&Create {
            id: id.to_owned(),
            argv: Some(live_cmd),
            cols,
            rows,
            ..Create::default()
        })
        .expect("create flood terminal");
    let sub = client
        .subscribe(id, Role::Own, Some((cols, rows)), 0)
        .expect("subscribe flood terminal");
    let _ = created;

    // Wait for shell readiness marker (`READY`) so fork/exec/`stty` latency is excluded.
    let mut ready_buf = Vec::new();
    let ready_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < ready_deadline {
        match client.events().recv_timeout(Duration::from_millis(500)) {
            Ok(Event::Output { handle, bytes, .. }) if handle == sub.handle => {
                ready_buf.extend_from_slice(&bytes);
                if ready_buf.windows(5).any(|w| w == b"READY") {
                    break;
                }
            }
            _ => {}
        }
    }

    let mut vt_live = Vt::new(cols, rows, &theme);
    let mut screen_live = Screen::new(cols, rows);
    let live_start = Instant::now();
    let mut last_fill = live_start;
    let mut live_bytes = 0usize;

    // Trigger gated `exec cat` start now that shell is parked in `read`.
    client.input(sub.handle, b"\n").expect("trigger flood gate");

    let deadline = Instant::now() + Duration::from_secs(45);
    while Instant::now() < deadline {
        match client.events().recv_timeout(Duration::from_secs(5)) {
            Ok(Event::Repaint { handle, bytes, .. }) if handle == sub.handle => {
                vt_live.feed(&bytes);
            }
            Ok(Event::Output { handle, bytes, .. }) if handle == sub.handle => {
                live_bytes += bytes.len();
                vt_live.feed(&bytes);
                if last_fill.elapsed() >= frame_interval {
                    vt_live.fill(&mut screen_live);
                    last_fill = Instant::now();
                }
            }
            Ok(Event::Exit { handle, .. }) | Ok(Event::Closed { handle })
                if handle == sub.handle =>
            {
                while let Ok(ev) = client.events().try_recv() {
                    match ev {
                        Event::Output { handle: h, bytes, .. } if h == sub.handle => {
                            live_bytes += bytes.len();
                            vt_live.feed(&bytes);
                        }
                        _ => {}
                    }
                }
                vt_live.fill(&mut screen_live);
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let live_elapsed = live_start.elapsed();
    let _ = client.unsubscribe(sub.handle);
    let _ = client.kill_with_signal(id, "KILL");

    let mb = stream.len() as f64 / 1_000_000.0;
    let cpu_60hz_mbs = mb / cpu_60hz_total.as_secs_f64();
    let cpu_every_chunk_mbs = mb / cpu_every_chunk_total.as_secs_f64();
    let wall_60hz_mbs = mb / best_wall_60hz.as_secs_f64();
    let total_ns = cpu_60hz_total.as_nanos().max(1) as f64;
    let feed_pct = (cpu_feed_total.as_nanos() as f64 / total_ns) * 100.0;
    let fill_pct = (cpu_fill_60hz_total.as_nanos() as f64 / total_ns) * 100.0;
    let live_termd_mbs = (live_bytes as f64 / 1_000_000.0) / live_elapsed.as_secs_f64();

    FloodResult {
        label,
        bytes: stream.len(),
        cpu_60hz_mbs,
        cpu_every_chunk_mbs,
        wall_60hz_mbs,
        feed_pct,
        fill_pct,
        fills_60hz: fills_at_60hz,
        live_termd_mbs,
        live_rx_bytes: live_bytes,
    }
}

fn run_flood_benchmarks(harness: &ScratchHarness, client: &Termd) -> Vec<FloodResult> {
    let mut results = Vec::new();

    // 1. `yes` flood (20 MB of `y\r\n`; stream via file so `SIGPIPE` inheritance cannot hang `yes | head`).
    let mut yes_buf = Vec::with_capacity(20_000_004);
    while yes_buf.len() < 20_000_000 {
        yes_buf.extend_from_slice(b"y\r\n");
    }
    yes_buf.truncate(20_000_001);
    let yes_file = harness.dir.join("yes-20mb.txt");
    let yes_lf: Vec<u8> = yes_buf.iter().copied().filter(|&b| b != b'\r').collect();
    std::fs::write(&yes_file, &yes_lf).expect("write 20MB yes file");
    let yes_cmd = vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        format!(
            "stty -echo; printf 'READY\\n'; IFS= read -r _; exec cat '{}'",
            yes_file.display()
        ),
    ];
    results.push(bench_flood_stream(
        harness, client, "yes (20 MB)", "f-yes", &yes_buf, yes_cmd,
    ));
    let _ = std::fs::remove_file(&yes_file);

    // 2. `cat` of a 20 MB source file.
    let src_corpus = build_20mb_source_corpus();
    let src_file = harness.dir.join("source-20mb.rs");
    // Write LF-separated file so PTY `ONLCR` expands `\n` -> `\r\n` like a real `cat`.
    let src_lf: Vec<u8> = src_corpus.iter().copied().filter(|&b| b != b'\r').collect();
    std::fs::write(&src_file, &src_lf).expect("write 20MB source file");
    let cat_src_cmd = vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        format!(
            "stty -echo; printf 'READY\\n'; IFS= read -r _; exec cat '{}'",
            src_file.display()
        ),
    ];
    results.push(bench_flood_stream(
        harness,
        client,
        "cat 20 MB source",
        "f-catsrc",
        &src_corpus,
        cat_src_cmd,
    ));

    // 3. Colour-heavy 20 MB stream.
    let color_corpus = build_20mb_color_corpus();
    let color_file = harness.dir.join("color-20mb.vt");
    let color_lf: Vec<u8> = color_corpus.iter().copied().filter(|&b| b != b'\r').collect();
    std::fs::write(&color_file, &color_lf).expect("write 20MB color file");
    let cat_color_cmd = vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        format!(
            "stty -echo; printf 'READY\\n'; IFS= read -r _; exec cat '{}'",
            color_file.display()
        ),
    ];
    results.push(bench_flood_stream(
        harness,
        client,
        "colour-heavy (20 MB)",
        "f-color",
        &color_corpus,
        cat_color_cmd,
    ));

    let _ = std::fs::remove_file(&src_file);
    let _ = std::fs::remove_file(&color_file);
    results
}

fn matches_filter(name: &str, only: Option<&str>) -> bool {
    match only {
        Some(f) => name.contains(f),
        None => true,
    }
}

fn main() {
    let cfg = parse_args();
    let harness = ScratchHarness::new(&cfg);
    let client = harness.connect_or_start("vt-diff");
    let welcome = client.welcome();

    println!(
        "vt-diff connected to scratch daemon {} (engine: {}, ns: {:?})",
        welcome.daemon, welcome.engine, welcome.ns
    );
    println!("claude/agy TUI: skipped (`claude` binary not installed; `agy` interactive TUI connects to live backend & mutates user conversations)");
    println!();

    let mut total_checkpoints = 0usize;
    let mut all_findings: Vec<CheckFinding> = Vec::new();

    type Runner<'h> = (&'static str, Box<dyn FnOnce() -> (usize, Vec<CheckFinding>) + 'h>);
    let scenarios: Vec<Runner<'_>> = vec![
        (
            "zsh-wrapped-prompt",
            Box::new(|| scenario_zsh_wrapped_prompt(&harness, &client, cfg.verbose)),
        ),
        (
            "vim-split-visual",
            Box::new(|| scenario_vim(&harness, &client, cfg.verbose)),
        ),
        (
            "less-search",
            Box::new(|| scenario_less_search(&harness, &client, cfg.verbose)),
        ),
        (
            "top-3s",
            Box::new(|| scenario_top_3s(&harness, &client, cfg.verbose)),
        ),
        (
            "tmux-two-panes",
            Box::new(|| {
                scenario_tmux_two_panes(&harness, &client, &cfg.tmux_label, cfg.verbose)
            }),
        ),
        (
            "git-log-graph",
            Box::new(|| scenario_git_log_graph(&harness, &client, cfg.verbose)),
        ),
        (
            "unicode-initial-default",
            Box::new(|| scenario_unicode(&harness, &client, None, cfg.verbose)),
        ),
        (
            "unicode-mode2027-off",
            Box::new(|| scenario_unicode(&harness, &client, Some(false), cfg.verbose)),
        ),
        (
            "unicode-mode2027-on",
            Box::new(|| scenario_unicode(&harness, &client, Some(true), cfg.verbose)),
        ),
        (
            "tab-stops",
            Box::new(|| scenario_tabs(&harness, &client, cfg.verbose)),
        ),
    ];

    println!("Differential scenarios (80x24 -> 132x40 -> 60x20 + mid-session REPAINT):");
    for (name, run) in scenarios {
        if !matches_filter(name, cfg.only.as_deref()) {
            continue;
        }
        let (checks, findings) = run();
        total_checkpoints += checks;
        let status = if findings.is_empty() { "MATCH" } else { "DIFF " };
        println!(
            "  {status}  {name:<24} ({checks} checkpoints, {} disagreement(s))",
            findings.len()
        );
        for f in &findings {
            println!(
                "         - [{} / {} :: {}] {}",
                f.scenario, f.checkpoint, f.category, f.summary
            );
            if cfg.verbose {
                println!("           bytes: {}", f.sample_bytes_escaped);
            }
        }
        all_findings.extend(findings);
    }

    println!(
        "\nSummary: {total_checkpoints} quiet-point comparisons, {} total disagreement(s).",
        all_findings.len()
    );

    if !cfg.skip_flood {
        println!("\nFlood throughput (120x40 grid, 64 KiB read chunks):");
        let floods = run_flood_benchmarks(&harness, &client);
        for f in &floods {
            println!(
                "  {:<22} cpu@60Hz: {:>7.1} MB/s (feed {:>4.1}%, fill {:>4.1}%, {} fills) | fill-per-64K: {:>7.1} MB/s | wall@60Hz: {:>6.1} MB/s | live PTY+termd: {:>6.1} MB/s ({:.1}/{:.1} MB)",
                f.label,
                f.cpu_60hz_mbs,
                f.feed_pct,
                f.fill_pct,
                f.fills_60hz,
                f.cpu_every_chunk_mbs,
                f.wall_60hz_mbs,
                f.live_termd_mbs,
                f.live_rx_bytes as f64 / 1_000_000.0,
                f.bytes as f64 / 1_000_000.0,
            );
        }
    }
}
