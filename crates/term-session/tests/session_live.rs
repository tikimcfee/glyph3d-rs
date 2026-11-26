//! Live integration and performance tests for `term-session` against a scratch `g3d-termd`
//! (`out/wall-port/briefs/S-term-session.md`).
//!
//! Every live assertion waits on [`wait_for`] with an explicit deadline rather than reading a
//! single snapshot after a fixed sleep, so all 8 tests run reliably concurrently even when CPU
//! cores are saturated by external load.

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
    Capture, CaptureFormat, ConnectOptions, DaemonEvent, DaemonState, HubOptions, Key, KeyAction,
    Role, SessionHub, SpawnOptions, Target, TerminalSession, View,
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

/// Polls `check` upon creation and after each wakeup (or 10 ms step) until `check` returns `true`
/// or `timeout` elapses.
fn wait_for(
    wake: &WakeSignal,
    seen: &mut u64,
    timeout: Duration,
    mut check: impl FnMut() -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if check() {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return check();
        }
        let step = (deadline - now).min(Duration::from_millis(10));
        wake.wait_since(seen, step);
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
    use std::fmt::Write as _;

    let scratch = ScratchDaemon::new("flood1");
    let seq_file = scratch.dir.join("seq_2m.txt");
    {
        let mut buf = String::with_capacity(15_000_000);
        for i in 1..=2_000_000u32 {
            let _ = writeln!(buf, "{i}");
        }
        fs::write(&seq_file, buf.as_bytes()).expect("write seq_2m.txt");
    }

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
                "exec cat \"$1\"".into(),
                "sh".into(),
                seq_file.to_string_lossy().into_owned(),
            ]),
            cols: 80,
            rows: 24,
            env_base: Some(base_env()),
            ..SpawnOptions::shell("seq1", 80, 24)
        })
        .expect("spawn seq1");

    let mut seen = wake.current();
    let mut pump_true_calls = 0u64;
    let settled = wait_for(&wake, &mut seen, Duration::from_secs(120), || {
        if session.pump() {
            pump_true_calls += 1;
        }
        session.exit_status().and_then(|e| e.code) == Some(0)
            && last_non_empty_line(&session) == "2000000"
            && session.stats().bytes_parsed >= 14_888_890
    });
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

    assert!(
        settled,
        "seq 1 2000000 did not settle cleanly: exit={:?} last_line={:?} bytes={}",
        session.exit_status(),
        last_non_empty_line(&session),
        stats.bytes_parsed
    );
    assert!(
        pub_per_s <= 120.0,
        "publish rate was not coalesced: {pub_per_s:.1} pub/s ({} publishes in {:.3}s)",
        stats.publishes,
        elapsed.as_secs_f64()
    );
    assert!(
        rss_delta_kb < 96 * 1024,
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

    let yes_start = Instant::now();
    let flood_ready = wait_for(&wake, &mut seen, Duration::from_secs(15), || {
        yes_sess.pump();
        yes_start.elapsed() >= Duration::from_millis(500)
            && yes_sess.stats().bytes_parsed >= 1024 * 1024
    });
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
    assert!(flood_ready, "yes flood did not reach 1 MiB before key injection");

    let t_key = Instant::now();
    yes_sess.send_key(Key {
        code: "KeyQ".into(),
        key: "q".into(),
        text: "q".into(),
        action: Some(KeyAction::Press),
        ..Key::default()
    });

    let mut key_delivered_at: Option<Duration> = None;
    let saw_marker = wait_for(&wake, &mut seen, Duration::from_secs(10), || {
        if key_delivered_at.is_none() && key_rx_marker.exists() {
            key_delivered_at = Some(t_key.elapsed());
        }
        yes_sess.pump();
        let found = screen_lines(&yes_sess)
            .iter()
            .any(|l| l.contains("KEY_HALTED_FLOOD_OK"));
        if found && key_delivered_at.is_none() && key_rx_marker.exists() {
            key_delivered_at = Some(t_key.elapsed());
        }
        found
    });
    let key_reaction = t_key.elapsed();
    let pty_rx_ms = key_delivered_at.unwrap_or(key_reaction).as_secs_f64() * 1000.0;
    println!(
        "[bench key-during-yes-flood] key -> PTY program received in {pty_rx_ms:.2} ms · buffered flood tail drained to screen in {:.2} ms (bytes_parsed={})",
        key_reaction.as_secs_f64() * 1000.0,
        yes_sess.stats().bytes_parsed
    );
    assert!(saw_marker, "key sent during yes flood never reached program");
    assert!(
        key_reaction < Duration::from_secs(5),
        "key during flood took too long: {key_reaction:?}"
    );
}

#[test]
fn flood_fourteen_terminals_concurrently() {
    use std::fmt::Write as _;

    const N: usize = 14;
    let scratch = ScratchDaemon::new("flood14");
    let seq_file = scratch.dir.join("seq_300k.txt");
    {
        let mut buf = String::with_capacity(2_200_000);
        for i in 1..=300_000u32 {
            let _ = writeln!(buf, "{i}");
        }
        fs::write(&seq_file, buf.as_bytes()).expect("write seq_300k.txt");
    }

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
                argv: Some(vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "exec cat \"$1\"".into(),
                    "sh".into(),
                    seq_file.to_string_lossy().into_owned(),
                ]),
                cols: 80,
                rows: 24,
                env_base: Some(base_env()),
                ..SpawnOptions::shell(&id, 80, 24)
            })
            .expect("spawn flooded terminal");
        sessions.push(sess);
    }

    let mut seen = wake.current();
    let all_done = wait_for(&wake, &mut seen, Duration::from_secs(120), || {
        for s in &mut sessions {
            s.pump();
        }
        sessions.iter().all(|s| {
            s.exit_status().and_then(|e| e.code) == Some(0) && last_non_empty_line(s) == "300000"
        })
    });
    assert!(all_done, "not all 14 flooded terminals settled cleanly to 300000");

    let elapsed = t_start.elapsed();
    let rss_after = current_rss_kb();
    let rss_delta_kb = rss_after.saturating_sub(rss_before);

    let mut total_bytes = 0u64;
    let mut total_publishes = 0u64;
    for (i, s) in sessions.iter().enumerate() {
        let st = s.stats();
        total_bytes += st.bytes_parsed;
        total_publishes += st.publishes;
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
    assert!(
        wait_for(&wake, &mut seen, Duration::from_secs(10), || {
            session.pump();
            ready_file.exists() && session.row_text(0).starts_with("READY:")
        }),
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

        let echoed = wait_for(&wake, &mut seen, Duration::from_secs(5), || {
            session.pump();
            let row0 = String::from_utf8(
                session.screen().row(0)[..expected_prefix.len()]
                    .iter()
                    .map(|c| if c.content == 0 { b' ' } else { c.content as u8 })
                    .collect(),
            )
            .unwrap_or_default();
            row0 == expected_prefix
        });
        assert!(echoed, "timed out waiting for echoed char '{ch}'");
        samples_us.push(t0.elapsed().as_micros() as u64);
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

    // Unloaded p50 is ~0.38 ms; allow up to 30 ms when 8 parallel tests + 6 `yes` processes contend.
    assert!(
        p50_ms < 30.0,
        "median echo latency {p50_ms:.3} ms exceeded threshold"
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
    assert!(
        wait_for(&wake, &mut seen, Duration::from_secs(10), || {
            session.pump();
            session.stats().bytes_parsed >= 128
        }),
        "streaming output did not begin"
    );

    session.resize(100, 32).expect("resize to 100x32");
    let _ = wait_for(&wake, &mut seen, Duration::from_millis(60), || {
        session.pump();
        false
    });

    session.resize(64, 18).expect("resize to 64x18");

    let settled = wait_for(&wake, &mut seen, Duration::from_secs(20), || {
        session.pump();
        session.exit_status().is_some()
            && (session.screen().cols, session.screen().rows) == (64, 18)
            && screen_lines(&session)
                .iter()
                .any(|l| l.contains("FINAL_AFTER_RESIZE_OK"))
    });
    assert!(
        settled,
        "streaming resize did not settle to 64x18 with FINAL_AFTER_RESIZE_OK: size={:?} lines={:?}",
        (session.screen().cols, session.screen().rows),
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
    assert!(
        wait_for(&wake, &mut seen, Duration::from_secs(10), || {
            session.pump();
            done_file.exists()
                && screen_lines(&session).iter().any(|l| l.contains("LIVE-PROMPT>"))
                && session.screen().cursor.visible
                && session.scrollback().offset == 0
        }),
        "scrollback setup did not reach live prompt"
    );

    // Scroll 20 rows back into history.
    seen = wake.current();
    let reply = session.view(View::Delta(20)).expect("view delta 20");
    assert_eq!(reply.offset, 20);
    assert!(reply.max >= 40, "expected >= 40 scrollback rows, got {}", reply.max);

    let scrolled_back = wait_for(&wake, &mut seen, Duration::from_secs(10), || {
        session.pump();
        let txt = session.text(0, 0, 60, 9);
        session.scrollback().viewing
            && session.scrollback().offset == 20
            && !session.screen().cursor.visible
            && txt.contains("HIST-ROW-25")
            && !txt.contains("LIVE-PROMPT>")
    });
    assert!(
        scrolled_back,
        "did not settle into scrollback offset=20: scrollback={:?} cursor={:?} text={:?}",
        session.scrollback(),
        session.screen().cursor,
        session.text(0, 0, 60, 9)
    );

    // Return to live bottom.
    seen = wake.current();
    let live_reply = session.view(View::Live).expect("view live");
    assert_eq!(live_reply.offset, 0);

    let back_live = wait_for(&wake, &mut seen, Duration::from_secs(10), || {
        session.pump();
        !session.scrollback().viewing
            && session.scrollback().offset == 0
            && session.screen().cursor.visible
            && screen_lines(&session).iter().any(|l| l.contains("LIVE-PROMPT>"))
    });
    assert!(back_live, "did not return cleanly to live screen");
}

#[test]
fn surface_title_pwd_modes_bell_sync2026_selection_and_exit() {
    let scratch = ScratchDaemon::new("surface");
    let wake = Arc::new(WakeSignal::default());
    let hub = SessionHub::open_with(&scratch.target(), HubOptions::default(), wake.waker())
        .expect("open scratch hub");

    let sub_gate = scratch.dir.join("sub-ready");
    let setup_done = scratch.dir.join("setup-done");
    let start_2026 = scratch.dir.join("start-2026");
    let mid_2026 = scratch.dir.join("mid-2026");
    let phase1_done = scratch.dir.join("p1");
    let go_exit = scratch.dir.join("go-exit");

    // Wait for `sub_gate` first so transient events (`BEL`) cannot fire in the window between
    // `create` and `subscribe` when 8 tests + CPU load share the host. Inside `?2026h`..`?2026l`,
    // synchronize via builtin `IFS= read -r _` on stdin rather than forking `/bin/sleep` so heavy
    // scheduler contention cannot stall `/bin/sh` past `DEFAULT_SYNC_TIMEOUT`.
    let script = "\
        stty -echo; \
        while [ ! -f \"$1\" ]; do sleep 0.005; done; \
        printf '\\033]2;wall-craft-title\\007'; \
        printf '\\033]7;file://localhost/tmp/craft-cwd\\007'; \
        printf '\\033[?1049h\\033[H\\033[2J'; \
        printf '\\033[?1000h\\033[?1006h\\033[?2004h'; \
        printf '\\007\\007'; \
        printf 'ALPHA_0123\\r\\nBRAVO_4567\\r\\nCHARLIE_89\\r\\n'; \
        printf s > \"$2\"; \
        while [ ! -f \"$3\" ]; do sleep 0.01; done; \
        printf '\\033[?2026h\\033[5;1HTORN_MID_FRAME    '; \
        printf m > \"$4\"; \
        IFS= read -r _; \
        printf '\\033[5;1HATOMIC_2026_FRAME!\\033[?2026l'; \
        printf p > \"$5\"; \
        while [ ! -f \"$6\" ]; do sleep 0.01; done; \
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
                sub_gate.to_string_lossy().into_owned(),
                setup_done.to_string_lossy().into_owned(),
                start_2026.to_string_lossy().into_owned(),
                mid_2026.to_string_lossy().into_owned(),
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
    assert!(
        wait_for(&wake, &mut seen, Duration::from_secs(10), || {
            session.pump();
            session.stats().publishes >= 1
        }),
        "initial subscribe repaint never arrived"
    );
    fs::write(&sub_gate, b"1").expect("open sub_gate");

    let mut total_bells = 0u64;
    let phase0_ok = wait_for(&wake, &mut seen, Duration::from_secs(10), || {
        session.pump();
        total_bells += session.take_bell();
        setup_done.exists()
            && session.title() == "wall-craft-title"
            && session.osc_title() == "wall-craft-title"
            && session.pwd().is_some_and(|p| p.contains("/tmp/craft-cwd"))
            && session.modes().alt_screen
            && session.modes().mouse == MouseTracking::Normal
            && session.modes().bracketed_paste
            && session.mouse_sgr()
            && total_bells >= 1
            && session.row_text(0) == "ALPHA_0123"
            && session.text(0, 0, 10, 1) == "ALPHA_0123\nBRAVO_4567"
            && session.block_text(6, 0, 10, 2) == "0123\n4567\nE_89"
    });
    assert!(
        phase0_ok,
        "surface metadata/selection did not converge: title={:?} pwd={:?} modes={:?} sgr={} bells={total_bells} row0={:?}",
        session.title(),
        session.pwd(),
        session.modes(),
        session.mouse_sgr(),
        session.row_text(0),
    );
    assert_eq!(session.take_bell(), 0, "second take_bell with no new BEL must be 0");

    // Rest > 16ms so the upcoming ?2026h frame arrives after a genuine quiet spell.
    std::thread::sleep(Duration::from_millis(35));
    session.pump();
    let bytes_before_2026 = session.stats().bytes_parsed;
    fs::write(&start_2026, b"1").expect("trigger ?2026h frame");

    let mut ever_saw_torn = false;
    let mid_fed = wait_for(&wake, &mut seen, Duration::from_secs(10), || {
        if session.pump()
            && screen_lines(&session)
                .iter()
                .any(|l| l.contains("TORN_MID_FRAME"))
        {
            ever_saw_torn = true;
        }
        mid_2026.exists() && session.stats().bytes_parsed >= bytes_before_2026 + 24
    });
    assert!(mid_fed, "?2026h mid-frame bytes were never fed into Vt");

    // Hold > 16ms coalescing interval while ?2026h remains open and verify no publish leaked.
    std::thread::sleep(Duration::from_millis(25));
    if session.pump()
        && screen_lines(&session)
            .iter()
            .any(|l| l.contains("TORN_MID_FRAME"))
    {
        ever_saw_torn = true;
    }
    assert!(
        !ever_saw_torn,
        "DEC ?2026h mid-frame text TORN_MID_FRAME leaked before ?2026l!"
    );

    // Unblock the shell builtin `read` so it emits `ATOMIC_2026_FRAME!\033[?2026l` immediately.
    session.send_input(b"\n".to_vec());
    let sync_ok = wait_for(&wake, &mut seen, Duration::from_secs(10), || {
        if session.pump()
            && screen_lines(&session)
                .iter()
                .any(|l| l.contains("TORN_MID_FRAME"))
        {
            ever_saw_torn = true;
        }
        phase1_done.exists()
            && screen_lines(&session)
                .iter()
                .any(|l| l.contains("ATOMIC_2026_FRAME!"))
    });
    assert!(sync_ok, "ATOMIC_2026_FRAME! never published");
    assert!(
        !ever_saw_torn,
        "DEC ?2026h mid-frame text TORN_MID_FRAME leaked before ?2026l!"
    );

    // Let the process exit with code 42.
    fs::write(&go_exit, b"1").expect("write go_exit");
    let exited_ok = wait_for(&wake, &mut seen, Duration::from_secs(10), || {
        session.pump();
        session.exit_status().and_then(|e| e.code) == Some(42) && session.is_ended()
    });
    assert!(
        exited_ok,
        "process did not report exit code 42: {:?}",
        session.exit_status()
    );
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
                "stty raw -echo; printf '\\033]2;SurvivorOne\\007ALIVE_BANNER_ONE\\r\\n' && printf 1 > \"$1\"; exec cat".into(),
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
    assert!(
        wait_for(&wake, &mut seen, Duration::from_secs(10), || {
            s1.pump();
            s2.pump();
            r1.exists()
                && r2.exists()
                && screen_lines(&s1).iter().any(|l| l.contains("ALIVE_BANNER_ONE"))
                && screen_lines(&s2).iter().any(|l| l.contains("ALIVE_BANNER_TWO"))
                && s1.title() == "SurvivorOne"
                && s2.title() == "SurvivorTwo"
        }),
        "initial screens/titles did not converge before upgrade"
    );

    let upgrade_reply = hub
        .client()
        .upgrade(None, false)
        .expect("request daemon upgrade/restart");
    println!("[daemon restart] upgrade reply: {upgrade_reply}");

    let mut saw_reconnected = false;
    let survived = wait_for(&wake, &mut seen, Duration::from_secs(15), || {
        for ev in hub.poll_daemon_events() {
            if let DaemonEvent::Reconnected { restart, .. } = ev {
                assert!(restart, "expected planned restart=true on upgrade");
                saw_reconnected = true;
            }
        }
        s1.pump();
        s2.pump();
        saw_reconnected
            && matches!(
                hub.daemon_state(),
                DaemonState::Connected { restarted: true, .. }
            )
            && screen_lines(&s1).iter().any(|l| l.contains("ALIVE_BANNER_ONE"))
            && screen_lines(&s2).iter().any(|l| l.contains("ALIVE_BANNER_TWO"))
            && s1.title() == "SurvivorOne"
            && s2.title() == "SurvivorTwo"
    });
    assert!(
        survived,
        "screens/titles did not survive upgrade cleanly: reconnected={saw_reconnected} s1={:?} s2={:?}",
        screen_lines(&s1),
        screen_lines(&s2)
    );

    // Verify input/output works immediately on the re-subscribed handle after restart.
    s1.paste("AFTER_RESTART_ECHO\r\n").expect("paste post-restart");
    assert!(
        wait_for(&wake, &mut seen, Duration::from_secs(10), || {
            s1.pump();
            screen_lines(&s1).iter().any(|l| l.contains("AFTER_RESTART_ECHO"))
        }),
        "post-restart input did not echo onto s1 screen"
    );

    // Stop the daemon and confirm `DaemonEvent::Gone` + `DaemonState::Gone`.
    hub.stop().expect("stop scratch daemon");
    let mut saw_gone = false;
    assert!(
        wait_for(&wake, &mut seen, Duration::from_secs(10), || {
            for ev in hub.poll_daemon_events() {
                if matches!(ev, DaemonEvent::Gone { .. }) {
                    saw_gone = true;
                }
            }
            saw_gone && matches!(hub.daemon_state(), DaemonState::Gone { .. })
        }),
        "hub did not report DaemonEvent::Gone after stop"
    );
}

#[test]
fn sessions_wall_seam_end_to_end() {
    use term_session::{News, Sessions};
    use wall_core::input::Mods;
    use wall_core::keymap::{OptionAsAlt as WallOptAlt, TerminalKey, TerminalKeyAction};
    use wall_core::mouse::MouseAction as WallMouseAction;
    use wall_core::pointer::{ProgramMouseButton, TerminalMouseEvent};

    let scratch = ScratchDaemon::new("seam");
    let wake = Arc::new(WakeSignal::default());
    let mut sessions =
        Sessions::open_target(&scratch.target(), wake.waker()).expect("open Sessions");
    assert_eq!(sessions.list().expect("list empty"), Vec::<String>::new());

    let ready = scratch.dir.join("seam-ready");
    let script = "\
        stty raw -echo; \
        i=1; while [ $i -le 30 ]; do printf 'SEAM-HIST-%02d\\r\\n' \"$i\"; i=$((i + 1)); done; \
        printf '\\033]2;seam-shell\\007\\033[?2004hLIVE-BOTTOM>'; \
        printf r > \"$1\"; \
        exec cat";
    let (id, cols, rows) = sessions
        .spawn(
            Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                script.into(),
                "sh".into(),
                ready.to_string_lossy().into_owned(),
            ]),
            60,
            10,
        )
        .expect("spawn t1");
    assert_eq!(id, "t1");
    assert_eq!((cols, rows), (60, 10));
    assert_eq!(sessions.ids().collect::<Vec<_>>(), vec!["t1"]);

    let mut seen = wake.current();
    let mut saw_title = false;
    let mut saw_modes = false;
    assert!(
        wait_for(&wake, &mut seen, Duration::from_secs(10), || {
            let (_changed, news) = sessions.pump();
            for n in news {
                match n {
                    News::Title { id: tid, title } if tid == "t1" && title == "seam-shell" => {
                        saw_title = true;
                    }
                    News::Modes { id: tid, modes } if tid == "t1" && modes.bracketed_paste => {
                        saw_modes = true;
                    }
                    _ => {}
                }
            }
            ready.exists()
                && saw_title
                && saw_modes
                && sessions
                    .text("t1", 0, 0, 60, 9)
                    .unwrap_or_default()
                    .contains("LIVE-BOTTOM>")
        }),
        "Sessions::pump did not converge on initial title/modes/text"
    );

    // Verify `screen(id)` returns a stable `&Screen` reference held across a simulated frame.
    {
        let frame_screen = sessions.screen("t1").expect("screen for t1");
        assert_eq!((frame_screen.cols, frame_screen.rows), (60, 10));
        assert!(frame_screen.cursor.visible);
    }

    // Primary-screen `scroll` (-10 = 10 lines up into history) moves termd's view & hides cursor.
    seen = wake.current();
    sessions.scroll("t1", -10, 0.0, 0.0);
    assert!(
        wait_for(&wake, &mut seen, Duration::from_secs(10), || {
            let _ = sessions.pump();
            !sessions.screen("t1").expect("screen").cursor.visible
                && sessions
                    .text("t1", 0, 0, 60, 9)
                    .expect("text")
                    .contains("SEAM-HIST-15")
        }),
        "primary-screen scroll(-10) did not enter scrollback view"
    );

    // Scroll back down (+10) to live bottom.
    seen = wake.current();
    sessions.scroll("t1", 10, 0.0, 0.0);
    assert!(
        wait_for(&wake, &mut seen, Duration::from_secs(10), || {
            let _ = sessions.pump();
            sessions.screen("t1").expect("screen").cursor.visible
                && sessions
                    .text("t1", 0, 0, 60, 9)
                    .expect("text")
                    .contains("LIVE-BOTTOM>")
        }),
        "primary-screen scroll(+10) did not return to live bottom"
    );

    // Send `key` (&TerminalKey) and `paste` (bracketed because ?2004h is active).
    sessions.key(
        "t1",
        &TerminalKey {
            code: "KeyZ".into(),
            key: "z".into(),
            mods: Vec::new(),
            text: "z".into(),
            action: TerminalKeyAction::Press,
            option_as_alt: WallOptAlt::Auto,
        },
    );
    sessions.paste("t1", "hello");
    assert!(
        wait_for(&wake, &mut seen, Duration::from_secs(10), || {
            let _ = sessions.pump();
            let txt = sessions.text("t1", 0, 0, 60, 9).expect("text");
            txt.contains("LIVE-BOTTOM>z") && txt.contains("hello")
        }),
        "key + paste did not echo onto t1"
    );

    // Resize -> `News::Resized`.
    seen = wake.current();
    sessions.resize("t1", 72, 14);
    let mut saw_resized = false;
    assert!(
        wait_for(&wake, &mut seen, Duration::from_secs(10), || {
            let (_, news) = sessions.pump();
            if news.contains(&News::Resized {
                id: "t1".into(),
                cols: 72,
                rows: 14,
            }) {
                saw_resized = true;
            }
            saw_resized && sessions.screen("t1").map(|s| (s.cols, s.rows)) == Some((72, 14))
        }),
        "Sessions::pump did not emit News::Resized(72, 14)"
    );

    // Check what `Sessions` + `termd` do when a second `Sessions` adopts with `Role::Own`:
    // `termd` grants `Role::Own` to the second connection and demotes the first to `Role::Attach`.
    {
        let mut second =
            Sessions::open_target(&scratch.target(), wake.waker()).expect("open second Sessions");
        let second_dims = second.adopt("t1").expect("second adopt t1");
        assert_eq!(second_dims, (72, 14));
        assert!(
            wait_for(&wake, &mut seen, Duration::from_secs(5), || {
                let _ = sessions.pump();
                let _ = second.pump();
                sessions.session("t1").map(|s| s.role()) == Some(Role::Attach)
                    && second.session("t1").map(|s| s.role()) == Some(Role::Own)
            }),
            "second Role::Own adopt must demote first session to Role::Attach"
        );
    }

    // Exercise `mouse` and `repaint`, then `kill` -> `News::Ended`.
    sessions.mouse(
        "t1",
        &TerminalMouseEvent {
            action: WallMouseAction::Move,
            button: ProgramMouseButton::None,
            col: 2.0,
            row: 1.0,
            mods: Mods::default(),
        },
    );
    sessions.repaint("t1");
    seen = wake.current();
    sessions.kill("t1");
    let mut saw_ended = false;
    assert!(
        wait_for(&wake, &mut seen, Duration::from_secs(10), || {
            let (_, news) = sessions.pump();
            if news.contains(&News::Ended { id: "t1".into() }) {
                saw_ended = true;
            }
            saw_ended && sessions.screen("t1").is_none()
        }),
        "Sessions::pump did not emit News::Ended after kill"
    );
}

#[test]
fn primary_grow_resize_pulls_scrollback_coalesced_and_burst_settles() {
    use term_session::{News, Sessions};

    let scratch = ScratchDaemon::new("grow");
    let wake = Arc::new(WakeSignal::default());
    let hub = SessionHub::open_with(&scratch.target(), HubOptions::default(), wake.waker())
        .expect("open scratch hub");

    // 1. Finding T2 repro (`out/wall-port/vt-diff-2026-11-26.md`):
    //    30 numbered lines on 80x24 under `sleep 60` (pushing lines 1..=7 into scrollback),
    //    followed by a 14-step drag resize to 132x40.
    let done_grow = scratch.dir.join("grow-done");
    let script = "i=1; while [ $i -le 30 ]; do printf '%d\\r\\n' \"$i\"; i=$((i + 1)); done; \
                  printf d > \"$1\"; exec sleep 60";
    let mut session = hub
        .spawn(&SpawnOptions {
            id: "grow1".into(),
            argv: Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                script.into(),
                "sh".into(),
                done_grow.to_string_lossy().into_owned(),
            ]),
            cols: 80,
            rows: 24,
            env_base: Some(base_env()),
            ..SpawnOptions::shell("grow1", 80, 24)
        })
        .expect("spawn grow1");

    let mut seen = wake.current();
    assert!(
        wait_for(&wake, &mut seen, Duration::from_secs(10), || {
            session.pump();
            done_grow.exists() && session.row_text(22) == "30"
        }),
        "grow1 did not finish printing 1..30 at 80x24: {:?}",
        screen_lines(&session)
    );
    assert_eq!(session.row_text(0), "8", "lines 1..=7 should be in scrollback at 80x24");
    assert_eq!(session.stats().repaints_sent, 0);

    // Drag across 14 resize steps from 80x24 to 132x40 (4 cols / ~1 row per step, 2 ms apart).
    for step in 1..=14u16 {
        let cols = 80 + (52 * step) / 14;
        let rows = 24 + (16 * step) / 14;
        session.send_resize(cols, rows);
        if step < 14 {
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    assert!(
        wait_for(&wake, &mut seen, Duration::from_secs(10), || {
            session.pump();
            let lines = screen_lines(&session);
            (session.screen().cols, session.screen().rows) == (132, 40)
                && lines.first().map(String::as_str) == Some("1")
                && lines.get(29).map(String::as_str) == Some("30")
        }),
        "grow1 at 132x40 did not pull scrollback history lines 1..=7 via coalesced repaint: rows={:?} stats={:?}",
        screen_lines(&session),
        session.stats()
    );

    // Compare every row of `session` against `TermdHandle::capture(Plain, unwrap: false)`.
    let cap = hub
        .client()
        .capture(&Capture {
            id: "grow1".into(),
            format: Some(CaptureFormat::Plain),
            history_rows: None,
            unwrap: Some(false),
        })
        .expect("capture grow1 at 132x40");
    let cap_lines: Vec<&str> = cap.text.split('\n').collect();
    let vt_lines = screen_lines(&session);
    for (r, vt_row) in vt_lines.iter().enumerate() {
        let daemon_row = cap_lines.get(r).copied().unwrap_or("").trim_end();
        assert_eq!(
            vt_row, daemon_row,
            "row {r} mismatch vs TermdHandle::capture at 132x40"
        );
    }

    let grow_stats = session.stats();
    println!(
        "[bench T2 14-step primary grow 80x24->132x40] resizes_sent={} (of 14) · repaints_sent={} (expected 1)",
        grow_stats.resizes_sent, grow_stats.repaints_sent
    );
    assert_eq!(
        grow_stats.repaints_sent, 1,
        "14-step primary grow should coalesce to a single repaint after settling, got {}",
        grow_stats.repaints_sent
    );

    // 2. Shrinking on primary screen (132x40 -> 60x20) must NOT ask for a repaint and must still match capture.
    session.send_resize(60, 20);
    assert!(
        wait_for(&wake, &mut seen, Duration::from_secs(10), || {
            session.pump();
            (session.screen().cols, session.screen().rows) == (60, 20)
        }),
        "grow1 did not shrink to 60x20"
    );
    // Wait past `DEFAULT_GROW_REPAINT_SETTLE` (35ms) to prove no repaint fires on shrink.
    std::thread::sleep(Duration::from_millis(60));
    session.pump();
    assert_eq!(
        session.stats().repaints_sent,
        1,
        "primary-screen shrink must not request an extra repaint"
    );
    let cap_shrunk = hub
        .client()
        .capture(&Capture {
            id: "grow1".into(),
            format: Some(CaptureFormat::Plain),
            history_rows: None,
            unwrap: Some(false),
        })
        .expect("capture grow1 at 60x20");
    let shrunk_daemon: Vec<&str> = cap_shrunk.text.split('\n').collect();
    for (r, vt_row) in screen_lines(&session).iter().enumerate() {
        let daemon_row = shrunk_daemon.get(r).copied().unwrap_or("").trim_end();
        assert_eq!(vt_row, daemon_row, "row {r} mismatch after shrink to 60x20");
    }

    // 3. Alternate screen (`?1049h`) 14-step grow must NEVER ask termd for a repaint.
    let alt_ready = scratch.dir.join("alt-ready");
    let mut alt_sess = hub
        .spawn(&SpawnOptions {
            id: "alt1".into(),
            argv: Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf '\\033[?1049hALT_VIEW'; printf a > \"$1\"; exec sleep 60".into(),
                "sh".into(),
                alt_ready.to_string_lossy().into_owned(),
            ]),
            cols: 80,
            rows: 24,
            env_base: Some(base_env()),
            ..SpawnOptions::shell("alt1", 80, 24)
        })
        .expect("spawn alt1");
    assert!(
        wait_for(&wake, &mut seen, Duration::from_secs(10), || {
            alt_sess.pump();
            alt_ready.exists() && alt_sess.modes().alt_screen
        }),
        "alt1 did not enter ?1049h"
    );
    for step in 1..=14u16 {
        alt_sess.send_resize(80 + step * 3, 24 + step);
    }
    assert!(
        wait_for(&wake, &mut seen, Duration::from_secs(10), || {
            alt_sess.pump();
            (alt_sess.screen().cols, alt_sess.screen().rows) == (122, 38)
        }),
        "alt1 did not settle at 122x38"
    );
    std::thread::sleep(Duration::from_millis(60));
    alt_sess.pump();
    assert_eq!(
        alt_sess.stats().repaints_sent,
        0,
        "alternate-screen grow must not request a repaint from termd"
    );

    // 4. End-to-end `Sessions::resize` 14-step burst (`effect -> News::Resized`) settle timing &
    //    outbound coalescing check on an already-running terminal.
    let burst_ready = scratch.dir.join("burst-ready");
    let mut sessions =
        Sessions::open_target(&scratch.target(), wake.waker()).expect("open Sessions for burst");
    let (bid, _, _) = sessions
        .spawn(
            Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf b > \"$1\"; exec sleep 60".into(),
                "sh".into(),
                burst_ready.to_string_lossy().into_owned(),
            ]),
            80,
            24,
        )
        .expect("spawn burst terminal");
    assert!(
        wait_for(&wake, &mut seen, Duration::from_secs(10), || {
            let _ = sessions.pump();
            burst_ready.exists()
                && sessions.session(&bid).is_some_and(|s| s.stats().publishes >= 1)
        }),
        "burst terminal initial publish did not arrive"
    );
    std::thread::sleep(Duration::from_millis(20));
    let _ = sessions.pump();

    seen = wake.current();
    let t_burst = Instant::now();
    for step in 1..=14u16 {
        let cols = 80 + (52 * step) / 14;
        let rows = 24 + (16 * step) / 14;
        sessions.resize(&bid, cols, rows);
    }
    let mut resized_events = Vec::new();
    let burst_settled = wait_for(&wake, &mut seen, Duration::from_secs(10), || {
        let (_, news) = sessions.pump();
        for n in news {
            if let News::Resized { id, cols, rows } = n {
                if id == bid {
                    resized_events.push((cols, rows));
                }
            }
        }
        resized_events.last().copied() == Some((132, 40))
    });
    let settle_ms = t_burst.elapsed().as_secs_f64() * 1000.0;
    let burst_stats = sessions.session(&bid).expect("burst session").stats();
    println!(
        "[bench 14-step resize burst (effect -> News::Resized)] settle={settle_ms:.2} ms · \
         outbound resizes_sent={} (of 14) · News::Resized events={} ({resized_events:?})",
        burst_stats.resizes_sent,
        resized_events.len()
    );
    assert!(burst_settled, "14-step burst did not settle to (132, 40)");
    assert!(
        burst_stats.resizes_sent < 14,
        "outbound resize burst should coalesce queued intermediate sizes (sent {} of 14)",
        burst_stats.resizes_sent
    );
}

#[test]
fn part1_scroll_state_pipelined_bell_and_osc8_links() {
    use term_session::Sessions;

    let scratch = ScratchDaemon::new("part1");
    let wake = Arc::new(WakeSignal::default());
    let mut sessions =
        Sessions::open_target(&scratch.target(), wake.waker()).expect("open Sessions");

    // 1. Scroll state (`offset`, `max` / `history_len()`, `at_bottom`) + `scroll_to_live` +
    //    `scroll(id, 0, ..)` zero-delta jump (`wall.agent.jump live`).
    let sb_ready = scratch.dir.join("sb-ready");
    let sb_script = "\
        i=1; while [ $i -le 35 ]; do printf 'HIST-%02d\\r\\n' \"$i\"; i=$((i + 1)); done; \
        printf 'LIVE-PROMPT>'; \
        printf r > \"$1\"; \
        exec sleep 60";
    let (t1, _, _) = sessions
        .spawn(
            Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                sb_script.into(),
                "sh".into(),
                sb_ready.to_string_lossy().into_owned(),
            ]),
            60,
            10,
        )
        .expect("spawn t1");

    let mut seen = wake.current();
    assert!(
        wait_for(&wake, &mut seen, Duration::from_secs(10), || {
            let _ = sessions.pump();
            let Some(sb) = sessions.scrollback(&t1) else {
                return false;
            };
            // `status_follow(Some(500))` populates `max` (`scrollback_rows`) even before any scroll!
            sb_ready.exists()
                && sb.offset == 0
                && sb.at_bottom
                && sb.is_at_bottom()
                && !sb.viewing
                && sb.history_len() >= 25
                && sessions
                    .text(&t1, 0, 0, 60, 9)
                    .unwrap_or_default()
                    .contains("LIVE-PROMPT>")
        }),
        "initial scrollback state did not report at_bottom=true and history_len >= 25: {:?}",
        sessions.scrollback(&t1)
    );

    // Scroll up 12 lines into history.
    seen = wake.current();
    sessions.scroll(&t1, -12, 0.0, 0.0);
    assert!(
        wait_for(&wake, &mut seen, Duration::from_secs(10), || {
            let _ = sessions.pump();
            let Some(sb) = sessions.scrollback(&t1) else {
                return false;
            };
            sb.offset == 12
                && !sb.at_bottom
                && sb.viewing
                && sb.history_len() >= 25
                && !sessions.screen(&t1).expect("screen").cursor.visible
        }),
        "scroll(-12) did not update scrollback state to offset=12, at_bottom=false: {:?}",
        sessions.scrollback(&t1)
    );

    // Jump directly back to live via `Sessions::scroll_to_live`.
    seen = wake.current();
    sessions.scroll_to_live(&t1);
    assert!(
        wait_for(&wake, &mut seen, Duration::from_secs(10), || {
            let _ = sessions.pump();
            let Some(sb) = sessions.scrollback(&t1) else {
                return false;
            };
            sb.offset == 0
                && sb.at_bottom
                && !sb.viewing
                && sessions.screen(&t1).expect("screen").cursor.visible
                && sessions
                    .text(&t1, 0, 0, 60, 9)
                    .unwrap_or_default()
                    .contains("LIVE-PROMPT>")
        }),
        "scroll_to_live did not return t1 to offset=0, at_bottom=true: {:?}",
        sessions.scrollback(&t1)
    );

    // Scroll back 8 lines, then exercise `wall.agent.jump live`'s zero-delta `scroll(id, 0, ..)`.
    seen = wake.current();
    sessions.scroll(&t1, -8, 0.0, 0.0);
    assert!(
        wait_for(&wake, &mut seen, Duration::from_secs(10), || {
            let _ = sessions.pump();
            sessions.scrollback(&t1).is_some_and(|sb| sb.offset == 8 && !sb.at_bottom)
        }),
        "scroll(-8) did not move to offset=8"
    );
    seen = wake.current();
    sessions.scroll(&t1, 0, 0.0, 0.0);
    assert!(
        wait_for(&wake, &mut seen, Duration::from_secs(10), || {
            let _ = sessions.pump();
            sessions
                .scrollback(&t1)
                .is_some_and(|sb| sb.offset == 0 && sb.at_bottom && !sb.viewing)
        }),
        "zero-delta scroll(0) did not jump t1 back to live: {:?}",
        sessions.scrollback(&t1)
    );

    // 2. Counted, drainable bell (`bell_count`, `unread_bells`, `take_bell`, `take_bells`) +
    //    pipelined `create_and_subscribe` startup bell test.
    let bell_ready = scratch.dir.join("bell-ready");
    let bell_script = "\
        printf r > \"$1\"; \
        while IFS= read -r line; do printf '\\007RING:%s\\r\\n' \"$line\"; done";
    let (t2, _, _) = sessions
        .spawn(
            Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                bell_script.into(),
                "sh".into(),
                bell_ready.to_string_lossy().into_owned(),
            ]),
            60,
            10,
        )
        .expect("spawn t2");
    assert!(
        wait_for(&wake, &mut seen, Duration::from_secs(10), || {
            let _ = sessions.pump();
            bell_ready.exists()
        }),
        "bell loop terminal t2 did not start"
    );

    for step in 1..=3 {
        sessions.paste(&t2, &format!("b{step}\n"));
        assert!(
            wait_for(&wake, &mut seen, Duration::from_secs(10), || {
                let _ = sessions.pump();
                sessions.bell_count(&t2) >= step
            }),
            "bell #{step} did not arrive on t2"
        );
    }
    assert_eq!(sessions.bell_count(&t2), 3);
    assert_eq!(sessions.unread_bells(&t2), 3);
    assert_eq!(sessions.take_bells(), vec![(t2.clone(), 3)]);
    assert_eq!(sessions.unread_bells(&t2), 0);
    assert_eq!(sessions.take_bell(&t2), 0);
    assert_eq!(sessions.bell_count(&t2), 3, "cumulative bell_count stays monotonic after drain");

    // Immediate startup bell (no sleep gate before `printf '\007'`), exercising `create_and_subscribe`.
    let mut caught_startup_bells = 0usize;
    let mut startup_ids = Vec::new();
    for i in 0..8 {
        let marker = scratch.dir.join(format!("imm-{i}"));
        let (bid, _, _) = sessions
            .spawn(
                Some(vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "printf '\\007IMM_BELL'; printf x > \"$1\"; exec sleep 60".into(),
                    "sh".into(),
                    marker.to_string_lossy().into_owned(),
                ]),
                40,
                6,
            )
            .expect("spawn immediate bell terminal");
        assert!(
            wait_for(&wake, &mut seen, Duration::from_secs(10), || {
                let _ = sessions.pump();
                marker.exists()
                    && sessions
                        .text(&bid, 0, 0, 40, 5)
                        .unwrap_or_default()
                        .contains("IMM_BELL")
            }),
            "immediate bell terminal {bid} did not render IMM_BELL"
        );
        // Give any trailing Event::Bell frame a brief moment to land if emitted.
        let _ = wait_for(&wake, &mut seen, Duration::from_millis(50), || {
            let _ = sessions.pump();
            sessions.unread_bells(&bid) >= 1
        });
        if sessions.take_bell(&bid) >= 1 {
            caught_startup_bells += 1;
        }
        startup_ids.push(bid);
    }
    println!(
        "[pipelined create_and_subscribe startup bell] caught {caught_startup_bells}/{} un-gated t=0 bells",
        startup_ids.len()
    );
    for bid in startup_ids {
        sessions.kill(&bid);
    }

    // 3. `OSC 8` hyperlinks (`Sessions::link_at`, wide-character tail resolution, and `REPAINT`).
    let link_ready = scratch.dir.join("link-ready");
    let link_script = "\
        printf '\\033[H\\033[2J'; \
        printf '\\033]8;;https://glyph3d.dev/docs\\033\\\\DOCS\\033]8;;\\033\\\\ plain '; \
        printf '\\033]8;id=repo;https://github.com/lugotools/glyph3d\\007漢字\\033]8;;\\007!'; \
        printf k > \"$1\"; \
        exec sleep 60";
    let (t3, _, _) = sessions
        .spawn(
            Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                link_script.into(),
                "sh".into(),
                link_ready.to_string_lossy().into_owned(),
            ]),
            60,
            8,
        )
        .expect("spawn t3");
    assert!(
        wait_for(&wake, &mut seen, Duration::from_secs(10), || {
            let _ = sessions.pump();
            link_ready.exists()
                && sessions
                    .row_text(&t3, 0)
                    .unwrap_or_default()
                    .contains("DOCS plain 漢字!")
        }),
        "t3 did not render OSC 8 row: {:?}",
        sessions.row_text(&t3, 0)
    );

    // Columns 0..=3: "DOCS" -> https://glyph3d.dev/docs
    for c in 0..=3 {
        assert_eq!(
            sessions.link_at(&t3, c, 0).as_deref(),
            Some("https://glyph3d.dev/docs"),
            "col {c} should carry first OSC 8 URI"
        );
    }
    // Columns 4..=10: " plain " -> None
    for c in 4..=10 {
        assert_eq!(sessions.link_at(&t3, c, 0), None, "col {c} must be unlinked");
    }
    // Columns 11..=14: "漢字" (two wide CJK chars, 2 cells each; both head and tail cells resolve)
    for c in 11..=14 {
        assert_eq!(
            sessions.link_at(&t3, c, 0).as_deref(),
            Some("https://github.com/lugotools/glyph3d"),
            "wide CJK cell col {c} should resolve second OSC 8 URI"
        );
    }
    // Column 15 ("!") and out-of-bounds -> None
    assert_eq!(sessions.link_at(&t3, 15, 0), None);
    assert_eq!(sessions.link_at(&t3, 999, 999), None);
    assert_eq!(sessions.link_at("nonexistent", 0, 0), None);

    // Re-adopt `t3` on a fresh `Sessions` (fed solely by `g3d-termd`'s `REPAINT`, `Extras::ALL`)
    // and verify `OSC 8` links survive `REPAINT` as well!
    let mut reader2 =
        Sessions::open_target(&scratch.target(), wake.waker()).expect("open second Sessions");
    reader2.adopt(&t3).expect("re-adopt t3");
    assert!(
        wait_for(&wake, &mut seen, Duration::from_secs(10), || {
            let _ = reader2.pump();
            reader2.link_at(&t3, 0, 0).as_deref() == Some("https://glyph3d.dev/docs")
                && reader2.link_at(&t3, 12, 0).as_deref()
                    == Some("https://github.com/lugotools/glyph3d")
        }),
        "OSC 8 hyperlinks should survive g3d-termd REPAINT on re-adopt"
    );
}
