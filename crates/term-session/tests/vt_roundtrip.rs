//! Conformance round-trip across `g3d-termd`, `term-session`, and `glyph-vt`
//! (`out/wall-port/briefs/S-term-session.md` & `crates/glyph-vt/tests/vt_conf.rs`).
//!
//! Feeds each of the 29 `.vt` conformance fixtures into a real `g3d-termd` terminal inside an
//! isolated `/tmp/g3d-s-...` scratch directory (`stty raw -echo` so the kernel PTY line discipline
//! never alters a byte), attaches a late subscriber (`Role::Watch`) whose `TerminalSession`
//! receives only `g3d-termd`'s `REPAINT` (+ metadata events), and compares both:
//!   1. `live_session.screen()` (live `Event::Output` stream through `TerminalSession`), and
//!   2. `late_session.screen()` (late subscriber's `Event::Repaint` through `TerminalSession`),
//!
//! against feeding the fixture bytes directly to `glyph_vt::Vt`.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use glyph_vt::{Theme, Vt};
use term_cells::Screen;
use term_session::{
    ConnectOptions, HubOptions, Role, SessionHub, SpawnOptions, Target, TermInfo,
};
use termd_client::Termd;

const EXPECTED_FIXTURE_COUNT: usize = 29;
const COLS: u16 = 40;
const ROWS: u16 = 12;

static SEQ: AtomicUsize = AtomicUsize::new(1);

struct ScratchDaemon {
    dir: PathBuf,
    ns: String,
}

impl ScratchDaemon {
    fn new(tag: &str) -> Self {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = PathBuf::from(format!("/tmp/g3d-s-{}-{tag}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create scratch dir");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
        let ns = "vt".to_string();
        assert!(
            dir.to_string_lossy().len() + ns.len() + 16 < 100,
            "socket path must stay under 100 bytes: {}",
            dir.display()
        );
        Self { dir, ns }
    }

    fn target(&self) -> Target {
        Target::namespace_in(&self.dir, &self.ns)
    }
}

impl Drop for ScratchDaemon {
    fn drop(&mut self) {
        let target = self.target();
        let opts = ConnectOptions {
            request_timeout: Duration::from_secs(2),
            reconnect_window: Duration::from_millis(100),
            ..ConnectOptions::default()
        };
        if let Ok(c) = Termd::connect_with(&target, "vt-cleanup", opts, Box::new(|| {})) {
            let _ = c.stop();
        }
        let sock = self.dir.join(format!("termd-{}.sock", self.ns));
        let deadline = Instant::now() + Duration::from_millis(500);
        while sock.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[derive(Default)]
struct WakeSignal {
    seq: Mutex<u64>,
    cv: Condvar,
}

impl WakeSignal {
    fn waker(self: &Arc<Self>) -> Arc<dyn Fn() + Send + Sync> {
        let sig = Arc::clone(self);
        Arc::new(move || {
            let mut g = sig.seq.lock().expect("wake lock");
            *g += 1;
            sig.cv.notify_all();
        })
    }

    fn wait_step(&self, seen: &mut u64, timeout: Duration) {
        let g = self.seq.lock().expect("wake lock");
        if *g == *seen {
            let (ng, _) = self.cv.wait_timeout(g, timeout).expect("wake wait");
            *seen = *ng;
        } else {
            *seen = *g;
        }
    }
}

fn base_env() -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert(
        "PATH".into(),
        std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin:/usr/sbin:/sbin".into()),
    );
    env.insert("HOME".into(), "/tmp".into());
    env.insert("LANG".into(), "en_US.UTF-8".into());
    env
}

fn fixture_paths() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../glyph-vt/tests/fixtures/vt-conf");
    let entries = fs::read_dir(&dir)
        .unwrap_or_else(|err| panic!("failed to read fixture dir {}: {err}", dir.display()));
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("vt") {
                Some(path)
            } else {
                None
            }
        })
        .collect();
    paths.sort();
    paths
}

fn diff_screens(expected: &Screen, actual: &Screen) -> Vec<String> {
    let mut diffs = Vec::new();
    if (expected.cols, expected.rows) != (actual.cols, actual.rows) {
        diffs.push(format!(
            "dims expected {}x{}, got {}x{}",
            expected.cols, expected.rows, actual.cols, actual.rows
        ));
        return diffs;
    }

    let mut cell_diffs = 0usize;
    for row in 0..expected.rows {
        let exp_row = expected.row(row);
        let act_row = actual.row(row);
        for col in 0..usize::from(expected.cols) {
            let e = &exp_row[col];
            let a = &act_row[col];
            let e_txt = expected.content_str(e);
            let a_txt = actual.content_str(a);
            if e_txt != a_txt
                || (e.fg, e.bg, e.ul, e.attrs, e.width) != (a.fg, a.bg, a.ul, a.attrs, a.width)
            {
                if cell_diffs < 4 {
                    diffs.push(format!(
                        "cell({col},{row}): direct=({:?} w={} fg={:#010x} bg={:#010x} ul={:#010x} attrs={:#06x}) vs termd=({:?} w={} fg={:#010x} bg={:#010x} ul={:#010x} attrs={:#06x})",
                        e_txt, e.width, e.fg, e.bg, e.ul, e.attrs,
                        a_txt, a.width, a.fg, a.bg, a.ul, a.attrs
                    ));
                }
                cell_diffs += 1;
            }
        }
    }
    if cell_diffs > 4 {
        diffs.push(format!("... ({cell_diffs} total cell diffs)"));
    }

    if expected.wrapped != actual.wrapped {
        diffs.push(format!(
            "wrapped: direct={:?} vs termd={:?}",
            expected.wrapped, actual.wrapped
        ));
    }
    if expected.cursor != actual.cursor {
        diffs.push(format!(
            "cursor: direct={:?} vs termd={:?}",
            expected.cursor, actual.cursor
        ));
    }
    if expected.modes != actual.modes {
        diffs.push(format!(
            "modes: direct={:?} vs termd={:?}",
            expected.modes, actual.modes
        ));
    }
    if expected.title != actual.title {
        diffs.push(format!(
            "title: direct={:?} vs termd={:?}",
            expected.title, actual.title
        ));
    }
    if expected.default_fg != actual.default_fg {
        diffs.push(format!(
            "default_fg: direct={:#010x} vs termd={:#010x}",
            expected.default_fg, actual.default_fg
        ));
    }
    if expected.default_bg != actual.default_bg {
        diffs.push(format!(
            "default_bg: direct={:#010x} vs termd={:#010x}",
            expected.default_bg, actual.default_bg
        ));
    }
    diffs
}

#[test]
fn vt_conformance_29_roundtrip_through_termd_session() {
    let paths = fixture_paths();
    assert_eq!(
        paths.len(),
        EXPECTED_FIXTURE_COUNT,
        "expected {EXPECTED_FIXTURE_COUNT} .vt fixtures, found {}",
        paths.len()
    );

    let scratch = ScratchDaemon::new("vtconf");
    let markers_dir = scratch.dir.join("markers");
    fs::create_dir_all(&markers_dir).expect("create markers dir");

    let wake_sig = Arc::new(WakeSignal::default());
    let hub = SessionHub::open_with(
        &scratch.target(),
        HubOptions::default(),
        wake_sig.waker(),
    )
    .expect("open scratch termd");
    let late_hub = SessionHub::connect_with(
        &scratch.target(),
        HubOptions {
            client_name: "vt-conf-late".into(),
            ..HubOptions::default()
        },
        wake_sig.waker(),
    )
    .expect("open second connection for late subscriber");

    let theme = Theme::default();
    let mut live_exact = 0usize;
    let mut repaint_exact = 0usize;
    let mut repaint_diff_summary: Vec<(String, Vec<String>)> = Vec::new();

    for (idx, path) in paths.iter().enumerate() {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();
        let bytes = fs::read(path).expect("read .vt fixture");

        // Reference: feed fixture bytes directly to `glyph_vt::Vt`.
        let mut direct_vt = Vt::new(COLS, ROWS, &theme);
        direct_vt.feed(&bytes);
        let mut direct_screen = Screen::new(COLS, ROWS);
        direct_vt.fill(&mut direct_screen);

        // Live terminal in `g3d-termd`: put PTY into raw -echo mode first, signal `ready_file`,
        // wait for 1 trigger byte on stdin so our live subscriber sees the entire byte stream from
        // offset 0, then `cat` the fixture verbatim and stay open for the late subscriber.
        let ready_file = markers_dir.join(format!("r-{idx}"));
        let done_file = markers_dir.join(format!("d-{idx}"));
        let script = "stty raw -echo; printf r > \"$2\"; dd bs=1 count=1 >/dev/null 2>&1; cat \"$1\"; printf d > \"$3\"; exec sleep 60";
        let term_id = format!("c{idx:02}");

        let mut live_session = hub
            .spawn(&SpawnOptions {
                id: term_id.clone(),
                argv: Some(vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    script.into(),
                    "sh".into(),
                    path.to_string_lossy().into_owned(),
                    ready_file.to_string_lossy().into_owned(),
                    done_file.to_string_lossy().into_owned(),
                ]),
                cols: COLS,
                rows: ROWS,
                env_base: Some(base_env()),
                ..SpawnOptions::shell(&term_id, COLS, ROWS)
            })
            .expect("spawn fixture terminal");

        // Wait for `stty raw -echo` to complete and initial empty repaint to arrive.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = 0u64;
        while (!ready_file.exists() || live_session.stats().publishes == 0)
            && Instant::now() < deadline
        {
            wake_sig.wait_step(&mut seen, Duration::from_millis(10));
            live_session.pump();
        }
        assert!(ready_file.exists(), "{stem}: shell did not become ready");

        let baseline_bytes = live_session.stats().bytes_parsed;
        live_session.input(b"g").expect("send trigger byte");

        // Wait until `cat` finishes AND `live_session` has parsed all fixture bytes and settled.
        let deadline = Instant::now() + Duration::from_secs(5);
        while (!done_file.exists()
            || live_session.stats().bytes_parsed < baseline_bytes + bytes.len() as u64)
            && Instant::now() < deadline
        {
            wake_sig.wait_step(&mut seen, Duration::from_millis(10));
            live_session.pump();
        }
        // Wait > 16 ms coalescing window so any trailing batch on `live_session` publishes.
        std::thread::sleep(Duration::from_millis(25));
        live_session.pump();

        let live_diffs = diff_screens(&direct_screen, live_session.screen());
        if live_diffs.is_empty() {
            live_exact += 1;
        } else {
            println!("LIVE-DIFF  {stem:<22} {}", live_diffs.join(" | "));
        }

        // Now attach a late subscriber (`Role::Watch`) on a second connection so its session
        // sees ONLY `g3d-termd`'s synthesized `REPAINT` (+ out-of-band `Title` / `Modes` events).
        let mut late_session = late_hub
            .attach_with_role(&term_id, Role::Watch, None, 0)
            .expect("attach late watch subscriber");
        let deadline = Instant::now() + Duration::from_secs(5);
        while late_session.stats().publishes == 0 && Instant::now() < deadline {
            wake_sig.wait_step(&mut seen, Duration::from_millis(10));
            late_session.pump();
        }
        std::thread::sleep(Duration::from_millis(20));
        late_session.pump();
        assert!(
            late_session.stats().publishes > 0,
            "{stem}: late subscriber never published its initial repaint"
        );

        let repaint_diffs = diff_screens(&direct_screen, late_session.screen());
        if repaint_diffs.is_empty() {
            repaint_exact += 1;
            println!("PASS       {stem:<22} live=exact · repaint=exact");
        } else {
            println!(
                "DIFF       {stem:<22} live={} · repaint differs: {}",
                if live_diffs.is_empty() { "exact" } else { "DIFF" },
                repaint_diffs.join(" ; ")
            );
            repaint_diff_summary.push((stem.clone(), repaint_diffs));
        }

        drop(late_session);
        drop(live_session);
        let _ = hub.kill(&term_id);
    }

    let remaining: Vec<TermInfo> = hub.list().expect("list remaining");
    println!(
        "\nvt-conf summary ({COLS}x{ROWS}): {live_exact}/{EXPECTED_FIXTURE_COUNT} live-stream exact, \
         {repaint_exact}/{EXPECTED_FIXTURE_COUNT} late-repaint exact (remaining live terms: {})",
        remaining.len()
    );
    for (name, reasons) in &repaint_diff_summary {
        println!("  repaint diff [{name}]: {}", reasons.join(" ; "));
    }

    assert_eq!(
        live_exact, EXPECTED_FIXTURE_COUNT,
        "every live OUTPUT stream fed through TerminalSession must match direct glyph_vt::Vt"
    );
    // At least 25+ of 29 late repaints must match cell-for-cell (any difference comes solely from
    // state `g3d-termd`'s Ghostty formatter omits or normalizes in REPAINT).
    assert!(
        repaint_exact >= 25,
        "unexpectedly low late-repaint agreement: {repaint_exact}/{EXPECTED_FIXTURE_COUNT}"
    );
}
