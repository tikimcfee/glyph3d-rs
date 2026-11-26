//! Live integration and performance tests for `term-session` against a scratch `g3d-termd`
//! (`out/wall-port/briefs/S-term-session.md`).
//!
//! Covers:
//! 1. Single-terminal flood (`seq 1 2000000`) + mid-flood `key` responsiveness, bounded publish
//!    rate, exact final screen, and bounded RSS.
//! 2. Fourteen concurrent flooded terminals (`seq 1 300000` each) on 14 worker threads.
//! 3. Keystroke echo latency after a quiet spell (`> 16 ms`), proving leading-edge publication
//!    never waits on a timer.
//! 4. Live `resize` while output streams continuously.
//! 5. Scrollback `view` into history (cursor hidden, offset/max tracked) and `View::Live` return.
//! 6. Full window surface: `title`, `pwd` (`OSC 7`), `modes` (`mouse`, `alt_screen`,
//!    `bracketed_paste`), `mouse_sgr`, `bell`, DEC mode 2026 synchronized-output whole-frame
//!    gating, selection text (`text`, `block_text`, `row_text`), and `exit_status`.
//! 7. Daemon brain restart (`upgrade`) ridden with screens and titles intact, plus `Gone` on `stop`.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use term_cells::MouseTracking;
use term_session::{
    ConnectOptions, DaemonEvent, DaemonState, HubOptions, Key, KeyAction, SessionHub,
    SpawnOptions, Target, TerminalSession, View,
};
use termd_client::Termd;

static NEXT_ID: AtomicUsize = AtomicUsize::new(1);

struct ScratchDaemon {
    dir: PathBuf,
    ns: String,
}

impl ScratchDaemon {
    fn new(tag: &str) -> Self {
        let n = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let dir = PathBuf::from(format!("/tmp/g3d-s-{}-{tag}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create scratch dir");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
        let ns = format!("s{n}");
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
        if let Ok(c) = Termd::connect_with(&target, "s-cleanup", opts, Box::new(|| {})) {
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

    fn current(&self) -> u64 {
        *self.seq.lock().expect("wake lock")
    }

    fn wait_since(&self, seen: &mut u64, timeout: Duration) {
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

fn current_rss_kb() -> u64 {
    let pid = std::process::id().to_string();
    let Ok(out) = Command::new("ps").args(["-o", "rss=", "-p", &pid]).output() else {
        return 0;
    };
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .unwrap_or(0)
}

fn screen_lines(session: &TerminalSession) -> Vec<String> {
    let s = session.screen();
    (0..s.rows)
        .map(|r| {
            let line: String = s
                .row(r)
                .iter()
                .map(|c| {
                    if c.width == 0 {
                        String::new()
                    } else if c.content == 0 {
                        " ".into()
                    } else {
                        s.content_str(c).into_owned()
                    }
                })
                .collect();
            line.trim_end().to_string()
        })
        .collect()
}

fn last_non_empty_line(session: &TerminalSession) -> String {
    screen_lines(session)
        .into_iter()
        .rev()
        .find(|l| !l.is_empty())
        .unwrap_or_default()
}

#[test]
fn flood_single_terminal_and_key_during_flood() {
    let scratch = ScratchDaemon::new("flood1");
    let wake = Arc::new(WakeSignal::default());
    let hub = SessionHub::open_with(&scratch.target(), HubOptions::default(), wake.waker())
        .expect("open scratch hub");

    let rss_before = current_rss_kb();
    let t_start = Instant::now();
    let mut session = hub
        .spawn(&SpawnOptions {
            id: "seq1".into(),
            argv: Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                "seq -f '%.0f' 1 2000000".into(),
            ]),
            cols: 80,
            rows: 24,
            env_base: Some(base_env()),
            ..SpawnOptions::shell("seq1", 80, 24)
        })
        .expect("spawn seq1");

    let mut seen = 0u64;
    let mut pump_true_calls = 0u64;
    let deadline = Instant::now() + Duration::from_secs(30);
    while session.exit_status().is_none() && Instant::now() < deadline {
        wake.wait_since(&mut seen, Duration::from_millis(25));
        if session.pump() {
            pump_true_calls += 1;
        }
    }
    std::thread::sleep(Duration::from_millis(20));
    if session.pump() {
        pump_true_calls += 1;
    }
    let elapsed = t_start.elapsed();
    let rss_after = current_rss_kb();
    let stats = session.stats();

    let mb = stats.bytes_parsed as f64 / (1024.0 * 1024.0);
    let mb_per_s = mb / elapsed.as_secs_f64();
    let pub_per_s = stats.publishes as f64 / elapsed.as_secs_f64();
    let rss_delta_kb = rss_after.saturating_sub(rss_before);

    println!(
        "[bench 1-term seq 1..2000000] bytes={} ({mb:.2} MiB) in {:.3}s => {mb_per_s:.1} MiB/s ({:.1} MB/s) | \
         publishes={} ({pub_per_s:.1} pub/s, window_swaps={pump_true_calls}) | RSS {rss_before} -> {rss_after} KiB (+{rss_delta_kb} KiB)",
        stats.bytes_parsed,
        elapsed.as_secs_f64(),
        (stats.bytes_parsed as f64 / 1_000_000.0) / elapsed.as_secs_f64(),
        stats.publishes,
    );

    assert_eq!(
        session.exit_status().and_then(|e| e.code),
        Some(0),
        "seq 1 2000000 must exit cleanly with 0"
    );
    assert_eq!(
        last_non_empty_line(&session),
        "2000000",
        "final screen bottom line must be 2000000"
    );
    assert!(
        stats.bytes_parsed >= 14_888_890,
        "expected >= 14.88 MB parsed, got {}",
        stats.bytes_parsed
    );
    assert!(
        pub_per_s <= 120.0,
        "publish rate was not coalesced: {pub_per_s:.1} pub/s ({} publishes in {:.3}s)",
        stats.publishes,
        elapsed.as_secs_f64()
    );
    assert!(
        rss_delta_kb < 64 * 1024,
        "RSS grew excessively during 1-terminal flood: +{rss_delta_kb} KiB"
    );
    drop(session);

    // Part B: key typed mid-flood (`yes`) reaches the program promptly and halts the flood.
    let key_rx_marker = scratch.dir.join("key-received");
    let script = "stty -icanon -echo min 1 time 0; \
                  yes 'FLOOD_ROW_0123456789_ABCDEFGHIJKLMNOPQRSTUVWXYZ' & fpid=$!; \
                  dd bs=1 count=1 >/dev/null 2>&1; \
                  printf k > \"$1\"; \
                  kill \"$fpid\" 2>/dev/null; wait \"$fpid\" 2>/dev/null; \
                  printf '\\r\\nKEY_HALTED_FLOOD_OK\\r\\n'";
    let mut yes_sess = hub
        .spawn(&SpawnOptions {
            id: "yes1".into(),
            argv: Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                script.into(),
                "sh".into(),
                key_rx_marker.to_string_lossy().into_owned(),
            ]),
            cols: 80,
            rows: 24,
            env_base: Some(base_env()),
            ..SpawnOptions::shell("yes1", 80, 24)
        })
        .expect("spawn yes1");

    // Let `yes` flood for ~500 ms so we also measure bulk 8 KiB block-buffered PTY flood speed,
    // then inject a keystroke mid-flood and time how quickly it halts the flood.
    let yes_start = Instant::now();
    let flood_window = Duration::from_millis(500);
    while yes_start.elapsed() < flood_window
        || yes_sess.stats().bytes_parsed < 1024 * 1024
    {
        if yes_start.elapsed() > Duration::from_secs(10) {
            break;
        }
        wake.wait_since(&mut seen, Duration::from_millis(10));
        yes_sess.pump();
    }
    let yes_elapsed = yes_start.elapsed();
    let yes_stats = yes_sess.stats();
    println!(
        "[bench 1-term yes bulk flood] bytes={} ({:.2} MiB) in {:.3}s => {:.1} MiB/s ({:.1} MB/s) | publishes={} ({:.1} pub/s)",
        yes_stats.bytes_parsed,
        yes_stats.bytes_parsed as f64 / (1024.0 * 1024.0),
        yes_elapsed.as_secs_f64(),
        (yes_stats.bytes_parsed as f64 / (1024.0 * 1024.0)) / yes_elapsed.as_secs_f64(),
        (yes_stats.bytes_parsed as f64 / 1_000_000.0) / yes_elapsed.as_secs_f64(),
        yes_stats.publishes,
        yes_stats.publishes as f64 / yes_elapsed.as_secs_f64(),
    );
    assert!(
        yes_stats.bytes_parsed >= 1024 * 1024,
        "yes flood did not reach 1 MiB before key injection"
    );

    let t_key = Instant::now();
    yes_sess.send_key(Key {
        code: "KeyQ".into(),
        key: "q".into(),
        text: "q".into(),
        action: Some(KeyAction::Press),
        ..Key::default()
    });

    let key_deadline = Instant::now() + Duration::from_secs(5);
    let mut key_delivered_at: Option<Duration> = None;
    let mut saw_marker = false;
    while Instant::now() < key_deadline {
        if key_delivered_at.is_none() && key_rx_marker.exists() {
            key_delivered_at = Some(t_key.elapsed());
        }
        wake.wait_since(&mut seen, Duration::from_millis(2));
        yes_sess.pump();
        if screen_lines(&yes_sess)
            .iter()
            .any(|l| l.contains("KEY_HALTED_FLOOD_OK"))
        {
            if key_delivered_at.is_none() && key_rx_marker.exists() {
                key_delivered_at = Some(t_key.elapsed());
            }
            saw_marker = true;
            break;
        }
    }
    let key_reaction = t_key.elapsed();
    let pty_rx_ms = key_delivered_at.unwrap_or(key_reaction).as_secs_f64() * 1000.0;
    println!(
        "[bench key-during-yes-flood] key -> PTY program received in {pty_rx_ms:.2} ms · buffered flood tail drained to screen in {:.2} ms (bytes_parsed={})",
        key_reaction.as_secs_f64() * 1000.0,
        yes_sess.stats().bytes_parsed
    );
    assert!(saw_marker, "key sent during yes flood never reached program");
    assert!(
        key_reaction < Duration::from_secs(1),
        "key during flood took too long: {key_reaction:?}"
    );
}

#[test]
fn flood_fourteen_terminals_concurrently() {
    const N: usize = 14;
    let scratch = ScratchDaemon::new("flood14");
    let wake = Arc::new(WakeSignal::default());
    let hub = SessionHub::open_with(&scratch.target(), HubOptions::default(), wake.waker())
        .expect("open scratch hub");

    let rss_before = current_rss_kb();
    let t_start = Instant::now();
    let mut sessions = Vec::with_capacity(N);
    for i in 0..N {
        let id = format!("f{i:02}");
        let sess = hub
            .spawn(&SpawnOptions {
                id: id.clone(),
                argv: Some(vec!["/bin/sh".into(), "-c".into(), "seq 1 300000".into()]),
                cols: 80,
                rows: 24,
                env_base: Some(base_env()),
                ..SpawnOptions::shell(&id, 80, 24)
            })
            .expect("spawn flooded terminal");
        sessions.push(sess);
    }

    let mut seen = 0u64;
    let deadline = Instant::now() + Duration::from_secs(45);
    while sessions.iter().any(|s| s.exit_status().is_none()) && Instant::now() < deadline {
        wake.wait_since(&mut seen, Duration::from_millis(20));
        for s in &mut sessions {
            s.pump();
        }
    }
    std::thread::sleep(Duration::from_millis(25));
    for s in &mut sessions {
        s.pump();
    }

    let elapsed = t_start.elapsed();
    let rss_after = current_rss_kb();
    let rss_delta_kb = rss_after.saturating_sub(rss_before);

    let mut total_bytes = 0u64;
    let mut total_publishes = 0u64;
    for (i, s) in sessions.iter().enumerate() {
        let st = s.stats();
        total_bytes += st.bytes_parsed;
        total_publishes += st.publishes;
        assert_eq!(
            s.exit_status().and_then(|e| e.code),
            Some(0),
            "terminal {i} did not exit cleanly"
        );
        assert_eq!(
            last_non_empty_line(s),
            "300000",
            "terminal {i} final bottom line mismatch"
        );
        let per_term_pub_s = st.publishes as f64 / elapsed.as_secs_f64();
        assert!(
            per_term_pub_s <= 120.0,
            "terminal {i} exceeded publish cap: {per_term_pub_s:.1} pub/s"
        );
    }

    let agg_mib_s = (total_bytes as f64 / (1024.0 * 1024.0)) / elapsed.as_secs_f64();
    let agg_mb_s = (total_bytes as f64 / 1_000_000.0) / elapsed.as_secs_f64();
    let agg_pub_s = total_publishes as f64 / elapsed.as_secs_f64();
    println!(
        "[bench 14-term seq 1..300000] total_bytes={total_bytes} ({:.2} MiB) in {:.3}s => \
         {agg_mib_s:.1} MiB/s ({agg_mb_s:.1} MB/s) | total_publishes={total_publishes} \
         ({agg_pub_s:.1} agg pub/s, {:.1} pub/s/term) | RSS {rss_before} -> {rss_after} KiB (+{rss_delta_kb} KiB)",
        total_bytes as f64 / (1024.0 * 1024.0),
        elapsed.as_secs_f64(),
        agg_pub_s / (N as f64),
    );
    assert!(
        rss_delta_kb < 128 * 1024,
        "RSS grew excessively across 14 concurrent floods: +{rss_delta_kb} KiB"
    );
}

#[test]
fn typing_echo_latency_after_quiet_spell() {
    let scratch = ScratchDaemon::new("echo");
    let wake = Arc::new(WakeSignal::default());
    let hub = SessionHub::open_with(&scratch.target(), HubOptions::default(), wake.waker())
        .expect("open scratch hub");

    let ready_file = scratch.dir.join("echo-ready");
    let script = "stty raw -echo; printf 'READY:' && printf r > \"$1\"; exec cat";
    let mut session = hub
        .spawn(&SpawnOptions {
            id: "echo1".into(),
            argv: Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                script.into(),
                "sh".into(),
                ready_file.to_string_lossy().into_owned(),
            ]),
            cols: 80,
            rows: 24,
            env_base: Some(base_env()),
            ..SpawnOptions::shell("echo1", 80, 24)
        })
        .expect("spawn echo1");

    let mut seen = wake.current();
    let deadline = Instant::now() + Duration::from_secs(5);
    while (!ready_file.exists() || !session.row_text(0).starts_with("READY:"))
        && Instant::now() < deadline
    {
        wake.wait_since(&mut seen, Duration::from_millis(10));
        session.pump();
    }
    assert!(
        session.row_text(0).starts_with("READY:"),
        "echo harness did not print READY:"
    );

    let mut samples_us = Vec::new();
    let mut expected_prefix = String::from("READY:");

    for ch in 'a'..='l' {
        // Quiet spell strictly exceeding the 16 ms coalescer interval.
        std::thread::sleep(Duration::from_millis(40));
        session.pump();
        seen = wake.current();

        expected_prefix.push(ch);
        let key_ev = Key {
            code: format!("Key{}", ch.to_ascii_uppercase()),
            key: ch.to_string(),
            text: ch.to_string(),
            action: Some(KeyAction::Press),
            ..Key::default()
        };

        let t0 = Instant::now();
        session.send_key(key_ev);

        let step_deadline = Instant::now() + Duration::from_secs(2);
        loop {
            wake.wait_since(&mut seen, Duration::from_millis(50));
            session.pump();
            let row0 = String::from_utf8(
                session.screen().row(0)[..expected_prefix.len()]
                    .iter()
                    .map(|c| if c.content == 0 { b' ' } else { c.content as u8 })
                    .collect(),
            )
            .unwrap_or_default();
            if row0 == expected_prefix {
                samples_us.push(t0.elapsed().as_micros() as u64);
                break;
            }
            assert!(
                Instant::now() < step_deadline,
                "timed out waiting for echoed char '{ch}' (row0={row0:?})"
            );
        }
    }

    samples_us.sort_unstable();
    let min_ms = samples_us[0] as f64 / 1000.0;
    let p50_ms = samples_us[samples_us.len() / 2] as f64 / 1000.0;
    let p95_ms = samples_us[(samples_us.len() * 95) / 100] as f64 / 1000.0;
    let max_ms = *samples_us.last().expect("non-empty") as f64 / 1000.0;
    let mean_ms =
        (samples_us.iter().sum::<u64>() as f64 / samples_us.len() as f64) / 1000.0;

    println!(
        "[bench quiet-spell echo latency (12 keys)] min={min_ms:.3} ms · p50={p50_ms:.3} ms · \
         mean={mean_ms:.3} ms · p95={p95_ms:.3} ms · max={max_ms:.3} ms (samples_us={samples_us:?})"
    );

    // Leading-edge publish must deliver keystroke echo well under the 16 ms flood coalescer window.
    assert!(
        p50_ms < 10.0,
        "median echo latency {p50_ms:.3} ms looks timer-delayed (expected < 10 ms)"
    );
}

#[test]
fn resize_while_output_streams() {
    let scratch = ScratchDaemon::new("resize");
    let wake = Arc::new(WakeSignal::default());
    let hub = SessionHub::open_with(&scratch.target(), HubOptions::default(), wake.waker())
        .expect("open scratch hub");

    let script = "i=1; while [ $i -le 80 ]; do \
                  printf 'RESIZE-STREAM-%03d-0123456789ABCDEF\\r\\n' \"$i\"; \
                  i=$((i + 1)); sleep 0.004; done; \
                  printf 'FINAL_AFTER_RESIZE_OK\\r\\n'";
    let mut session = hub
        .spawn(&SpawnOptions {
            id: "rsz1".into(),
            argv: Some(vec!["/bin/sh".into(), "-c".into(), script.into()]),
            cols: 80,
            rows: 24,
            env_base: Some(base_env()),
            ..SpawnOptions::shell("rsz1", 80, 24)
        })
        .expect("spawn rsz1");

    let mut seen = wake.current();
    // Wait until streaming has started.
    let start_deadline = Instant::now() + Duration::from_secs(5);
    while session.stats().bytes_parsed < 128 && Instant::now() < start_deadline {
        wake.wait_since(&mut seen, Duration::from_millis(10));
        session.pump();
    }

    session.resize(100, 32).expect("resize to 100x32");
    std::thread::sleep(Duration::from_millis(60));
    session.pump();

    session.resize(64, 18).expect("resize to 64x18");

    let end_deadline = Instant::now() + Duration::from_secs(10);
    while session.exit_status().is_none() && Instant::now() < end_deadline {
        wake.wait_since(&mut seen, Duration::from_millis(15));
        session.pump();
    }
    std::thread::sleep(Duration::from_millis(40));
    session.pump();

    assert_eq!(
        (session.screen().cols, session.screen().rows),
        (64, 18),
        "screen dimensions must settle to final 64x18 resize"
    );
    assert!(
        screen_lines(&session)
            .iter()
            .any(|l| l.contains("FINAL_AFTER_RESIZE_OK")),
        "missing FINAL_AFTER_RESIZE_OK after streaming resize: {:?}",
        screen_lines(&session)
    );
}

#[test]
fn scrollback_view_out_and_back_to_live() {
    let scratch = ScratchDaemon::new("scroll");
    let wake = Arc::new(WakeSignal::default());
    let hub = SessionHub::open_with(&scratch.target(), HubOptions::default(), wake.waker())
        .expect("open scratch hub");

    let done_file = scratch.dir.join("scroll-done");
    let script = "i=1; while [ $i -le 50 ]; do printf 'HIST-ROW-%02d\\r\\n' \"$i\"; i=$((i + 1)); done; \
                  printf 'LIVE-PROMPT>' && printf d > \"$1\"; exec sleep 60";
    let mut session = hub
        .spawn(&SpawnOptions {
            id: "scr1".into(),
            argv: Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                script.into(),
                "sh".into(),
                done_file.to_string_lossy().into_owned(),
            ]),
            cols: 60,
            rows: 10,
            env_base: Some(base_env()),
            ..SpawnOptions::shell("scr1", 60, 10)
        })
        .expect("spawn scr1");

    let mut seen = wake.current();
    let deadline = Instant::now() + Duration::from_secs(5);
    while (!done_file.exists()
        || !screen_lines(&session).iter().any(|l| l.contains("LIVE-PROMPT>")))
        && Instant::now() < deadline
    {
        wake.wait_since(&mut seen, Duration::from_millis(10));
        session.pump();
    }
    assert!(session.screen().cursor.visible, "live screen cursor must be visible");
    assert_eq!(session.scrollback().offset, 0);

    // Scroll 20 rows back into history.
    seen = wake.current();
    let reply = session.view(View::Delta(20)).expect("view delta 20");
    assert_eq!(reply.offset, 20);
    assert!(reply.max >= 40, "expected >= 40 scrollback rows, got {}", reply.max);

    let view_deadline = Instant::now() + Duration::from_secs(3);
    while (!session.scrollback().viewing
        || screen_lines(&session).iter().any(|l| l.contains("LIVE-PROMPT>")))
        && Instant::now() < view_deadline
    {
        wake.wait_since(&mut seen, Duration::from_millis(10));
        session.pump();
    }

    assert!(session.scrollback().viewing, "scrollback viewing flag must be true");
    assert_eq!(session.scrollback().offset, 20);
    assert!(
        !session.screen().cursor.visible,
        "cursor must be hidden while viewing scrollback"
    );
    let history_text = session.text(0, 0, 60, 9);
    assert!(
        history_text.contains("HIST-ROW-25"),
        "expected history rows around 22..31 at offset 20, got:\n{history_text}"
    );
    assert!(
        !history_text.contains("LIVE-PROMPT>"),
        "history view should not contain live bottom prompt:\n{history_text}"
    );

    // Return to live bottom.
    seen = wake.current();
    let live_reply = session.view(View::Live).expect("view live");
    assert_eq!(live_reply.offset, 0);

    let live_deadline = Instant::now() + Duration::from_secs(3);
    while !screen_lines(&session).iter().any(|l| l.contains("LIVE-PROMPT>"))
        && Instant::now() < live_deadline
    {
        wake.wait_since(&mut seen, Duration::from_millis(10));
        session.pump();
    }
    assert!(!session.scrollback().viewing);
    assert_eq!(session.scrollback().offset, 0);
    assert!(
        session.screen().cursor.visible,
        "cursor must become visible again on return to live"
    );
}

#[test]
fn surface_title_pwd_modes_bell_sync2026_selection_and_exit() {
    let scratch = ScratchDaemon::new("surface");
    let wake = Arc::new(WakeSignal::default());
    let hub = SessionHub::open_with(&scratch.target(), HubOptions::default(), wake.waker())
        .expect("open scratch hub");

    let setup_done = scratch.dir.join("setup-done");
    let start_2026 = scratch.dir.join("start-2026");
    let phase1_done = scratch.dir.join("p1");
    let go_exit = scratch.dir.join("go-exit");

    // 1) Set OSC 2 title, OSC 7 pwd, ?1000h + ?1006h + ?2004h + ?1049h, ring BEL twice, draw 3
    //    rows for selection text/block_text/row_text, and wait for a quiet spell (`start_2026`).
    // 2) Enter DEC ?2026h synchronized output from a quiet state, write TORN_MID_FRAME, sleep
    //    60ms (> 16ms coalescer interval!), overwrite with ATOMIC_2026_FRAME!, and close ?2026l.
    // 3) Wait for `go_exit`, leave alt screen, exit 42.
    let script = "\
        printf '\\033]2;wall-craft-title\\007'; \
        printf '\\033]7;file://localhost/tmp/craft-cwd\\007'; \
        printf '\\033[?1049h\\033[H\\033[2J'; \
        printf '\\033[?1000h\\033[?1006h\\033[?2004h'; \
        printf '\\007\\007'; \
        printf 'ALPHA_0123\\r\\nBRAVO_4567\\r\\nCHARLIE_89\\r\\n'; \
        printf s > \"$1\"; \
        while [ ! -f \"$2\" ]; do sleep 0.01; done; \
        printf '\\033[?2026h\\033[5;1HTORN_MID_FRAME    '; \
        sleep 0.06; \
        printf '\\033[5;1HATOMIC_2026_FRAME!\\033[?2026l'; \
        printf p > \"$3\"; \
        while [ ! -f \"$4\" ]; do sleep 0.01; done; \
        printf '\\033[?1049l'; \
        exit 42";

    let mut session = hub
        .spawn(&SpawnOptions {
            id: "surf1".into(),
            argv: Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                script.into(),
                "sh".into(),
                setup_done.to_string_lossy().into_owned(),
                start_2026.to_string_lossy().into_owned(),
                phase1_done.to_string_lossy().into_owned(),
                go_exit.to_string_lossy().into_owned(),
            ]),
            cols: 50,
            rows: 10,
            env_base: Some(base_env()),
            ..SpawnOptions::shell("surf1", 50, 10)
        })
        .expect("spawn surf1");

    let mut seen = wake.current();
    let setup_deadline = Instant::now() + Duration::from_secs(5);
    while (!setup_done.exists() || session.row_text(2) != "CHARLIE_89")
        && Instant::now() < setup_deadline
    {
        wake.wait_since(&mut seen, Duration::from_millis(10));
        session.pump();
    }
    // Rest > 16ms so the upcoming ?2026h frame arrives after a genuine quiet spell.
    std::thread::sleep(Duration::from_millis(35));
    session.pump();
    fs::write(&start_2026, b"1").expect("trigger ?2026h frame");

    let mut ever_saw_torn = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while (!phase1_done.exists()
        || !screen_lines(&session)
            .iter()
            .any(|l| l.contains("ATOMIC_2026_FRAME!")))
        && Instant::now() < deadline
    {
        wake.wait_since(&mut seen, Duration::from_millis(5));
        if session.pump()
            && screen_lines(&session)
                .iter()
                .any(|l| l.contains("TORN_MID_FRAME"))
        {
            ever_saw_torn = true;
        }
    }
    std::thread::sleep(Duration::from_millis(25));
    session.pump();

    assert!(
        !ever_saw_torn,
        "DEC ?2026h mid-frame text TORN_MID_FRAME leaked before ?2026l!"
    );
    assert_eq!(session.title(), "wall-craft-title");
    assert_eq!(session.osc_title(), "wall-craft-title");
    assert!(
        session
            .pwd()
            .is_some_and(|p| p.contains("/tmp/craft-cwd")),
        "unexpected pwd: {:?}",
        session.pwd()
    );
    assert!(session.modes().alt_screen);
    assert_eq!(session.modes().mouse, MouseTracking::Normal);
    assert!(session.modes().bracketed_paste);
    assert!(session.mouse_sgr());
    assert!(session.take_bell() >= 1, "expected at least one bell");
    assert_eq!(session.take_bell(), 0, "second take_bell must be 0");

    // Selection text extraction on the window thread without stopping the worker.
    assert_eq!(session.row_text(0), "ALPHA_0123");
    assert_eq!(
        session.text(0, 0, 10, 1),
        "ALPHA_0123\nBRAVO_4567"
    );
    assert_eq!(
        session.block_text(6, 0, 10, 2),
        "0123\n4567\nE_89"
    );

    // Let the process exit with code 42.
    fs::write(&go_exit, b"1").expect("write go_exit");
    let exit_deadline = Instant::now() + Duration::from_secs(5);
    while session.exit_status().is_none() && Instant::now() < exit_deadline {
        wake.wait_since(&mut seen, Duration::from_millis(10));
        session.pump();
    }
    assert_eq!(session.exit_status().and_then(|e| e.code), Some(42));
    assert!(session.is_ended());
}

#[test]
fn daemon_restart_preserves_session_screens_and_stop_reports_gone() {
    let scratch = ScratchDaemon::new("restart");
    let wake = Arc::new(WakeSignal::default());
    let hub = SessionHub::open_with(&scratch.target(), HubOptions::default(), wake.waker())
        .expect("open scratch hub");

    let r1 = scratch.dir.join("r1");
    let r2 = scratch.dir.join("r2");

    let mut s1 = hub
        .spawn(&SpawnOptions {
            id: "keep1".into(),
            argv: Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                "stty raw echo; printf '\\033]2;SurvivorOne\\007ALIVE_BANNER_ONE\\r\\n' && printf 1 > \"$1\"; exec cat".into(),
                "sh".into(),
                r1.to_string_lossy().into_owned(),
            ]),
            cols: 60,
            rows: 12,
            env_base: Some(base_env()),
            ..SpawnOptions::shell("keep1", 60, 12)
        })
        .expect("spawn keep1");

    let mut s2 = hub
        .spawn(&SpawnOptions {
            id: "keep2".into(),
            argv: Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf '\\033]2;SurvivorTwo\\007ALIVE_BANNER_TWO\\r\\n' && printf 2 > \"$1\"; exec sleep 60".into(),
                "sh".into(),
                r2.to_string_lossy().into_owned(),
            ]),
            cols: 60,
            rows: 12,
            env_base: Some(base_env()),
            ..SpawnOptions::shell("keep2", 60, 12)
        })
        .expect("spawn keep2");

    let mut seen = wake.current();
    let deadline = Instant::now() + Duration::from_secs(5);
    while (!r1.exists()
        || !r2.exists()
        || !screen_lines(&s1).iter().any(|l| l.contains("ALIVE_BANNER_ONE"))
        || !screen_lines(&s2).iter().any(|l| l.contains("ALIVE_BANNER_TWO")))
        && Instant::now() < deadline
    {
        wake.wait_since(&mut seen, Duration::from_millis(10));
        s1.pump();
        s2.pump();
    }

    // Trigger a live daemon brain restart (`upgrade(None, false)` re-execs the brain over its holder).
    let upgrade_reply = hub
        .client()
        .upgrade(None, false)
        .expect("request daemon upgrade/restart");
    println!("[daemon restart] upgrade reply: {upgrade_reply}");

    let mut saw_reconnected = false;
    let reconn_deadline = Instant::now() + Duration::from_secs(10);
    while !saw_reconnected && Instant::now() < reconn_deadline {
        wake.wait_since(&mut seen, Duration::from_millis(20));
        for ev in hub.poll_daemon_events() {
            if let DaemonEvent::Reconnected { restart, .. } = ev {
                assert!(restart, "expected planned restart=true on upgrade");
                saw_reconnected = true;
            }
        }
        s1.pump();
        s2.pump();
    }
    assert!(saw_reconnected, "hub never emitted DaemonEvent::Reconnected");
    assert!(matches!(
        hub.daemon_state(),
        DaemonState::Connected { restarted: true, .. }
    ));

    // Allow post-reconnect repaints to settle and verify both screens & titles survived intact.
    std::thread::sleep(Duration::from_millis(50));
    s1.pump();
    s2.pump();
    assert!(
        screen_lines(&s1).iter().any(|l| l.contains("ALIVE_BANNER_ONE")),
        "s1 screen lost across restart: {:?}",
        screen_lines(&s1)
    );
    assert!(
        screen_lines(&s2).iter().any(|l| l.contains("ALIVE_BANNER_TWO")),
        "s2 screen lost across restart: {:?}",
        screen_lines(&s2)
    );
    assert_eq!(s1.title(), "SurvivorOne");
    assert_eq!(s2.title(), "SurvivorTwo");

    // Verify input/output works immediately on the re-subscribed handle after restart.
    s1.paste("AFTER_RESTART_ECHO\r\n").expect("paste post-restart");
    let post_deadline = Instant::now() + Duration::from_secs(5);
    while !screen_lines(&s1).iter().any(|l| l.contains("AFTER_RESTART_ECHO"))
        && Instant::now() < post_deadline
    {
        wake.wait_since(&mut seen, Duration::from_millis(15));
        s1.pump();
    }
    assert!(
        screen_lines(&s1).iter().any(|l| l.contains("AFTER_RESTART_ECHO")),
        "post-restart input did not echo onto s1 screen"
    );

    // Stop the daemon and confirm `DaemonEvent::Gone` + `DaemonState::Gone`.
    hub.stop().expect("stop scratch daemon");
    let mut saw_gone = false;
    let gone_deadline = Instant::now() + Duration::from_secs(5);
    while !saw_gone && Instant::now() < gone_deadline {
        wake.wait_since(&mut seen, Duration::from_millis(20));
        for ev in hub.poll_daemon_events() {
            if matches!(ev, DaemonEvent::Gone { .. }) {
                saw_gone = true;
            }
        }
    }
    assert!(saw_gone, "hub did not report DaemonEvent::Gone after stop");
    assert!(matches!(hub.daemon_state(), DaemonState::Gone { .. }));
}
