//! Integration tests for `termd-client` against the real `g3d-termd` binary.
//!
//! Safety invariants (`out/WALL-PLAN-2026-11-26.md` & `A-termd-client.md`):
//! - Never connects to namespaces `wall`, `agents`, or `default`.
//! - Every test creates its own isolated `0700` scratch directory `/tmp/g3d-a-<pid>-<n>` (well
//!   under the 100-byte Unix socket limit) and its own namespace `ns-<n>`.
//! - `ScratchDaemon::drop` stops the daemon and removes the scratch directory even on panic.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use termd_client::{
    code, find_termd, ConnectOptions, Create, Error, Event, Key, Mouse, MouseAction, MouseButton,
    Role, Subscribe, Target, Termd, View, Mode,
};

static NEXT_ID: AtomicUsize = AtomicUsize::new(1);

const FORBIDDEN_NAMESPACES: [&str; 3] = ["wall", "agents", "default"];

fn require_termd_bin() -> Option<PathBuf> {
    let default_bin = PathBuf::from("/Users/lugo/.local/bin/g3d-termd");
    if default_bin.is_file() {
        return Some(default_bin);
    }
    match find_termd() {
        Ok(path) => Some(path),
        Err(why) => {
            eprintln!("SKIP termd_live: {why}");
            None
        }
    }
}

struct ScratchDaemon {
    dir: PathBuf,
    ns: String,
    bin: PathBuf,
}

impl ScratchDaemon {
    fn new(tag: &str, bin: PathBuf) -> Self {
        let idx = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let ns = format!("a-{tag}-{idx}");
        assert!(
            !FORBIDDEN_NAMESPACES.contains(&ns.as_str()),
            "refusing forbidden namespace {ns}"
        );
        let dir = PathBuf::from(format!("/tmp/g3d-a-{}-{idx}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .expect("create 0700 scratch dir under /tmp");

        let sock = dir.join(format!("termd-{ns}.sock"));
        assert!(
            sock.as_os_str().len() <= termd_client::MAX_SOCKET_PATH_BYTES,
            "scratch socket {} exceeds 100 bytes",
            sock.display()
        );
        Self { dir, ns, bin }
    }

    fn target(&self) -> Target {
        Target::namespace_in(&self.dir, &self.ns)
    }

    fn connect_options(&self) -> ConnectOptions {
        ConnectOptions {
            termd_bin: Some(self.bin.clone()),
            startup_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(10),
            reconnect_window: Duration::from_secs(10),
        }
    }

    fn start_client(&self, wake: Box<dyn Fn() + Send>) -> Termd {
        Termd::connect_or_start_with(&self.target(), "termd-client-it", self.connect_options(), wake)
            .expect("connect_or_start against scratch g3d-termd")
    }
}

impl Drop for ScratchDaemon {
    fn drop(&mut self) {
        let target = self.target();
        let opts = ConnectOptions {
            termd_bin: Some(self.bin.clone()),
            startup_timeout: Duration::from_secs(1),
            request_timeout: Duration::from_secs(3),
            reconnect_window: Duration::from_millis(100),
        };
        if let Ok(cleaner) = Termd::connect_with(&target, "cleanup", opts, Box::new(|| {})) {
            let _ = cleaner.stop();
        }
        // Wait briefly for the socket to disappear; only if the socket is still present (meaning
        // `stop` could not complete) send SIGKILL to the holder pid recorded in `.lock`.
        let sock = self.dir.join(format!("termd-{}.sock", self.ns));
        let lock = PathBuf::from(format!("{}.lock", sock.display()));
        let deadline = Instant::now() + Duration::from_millis(1500);
        while sock.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
        if sock.exists() {
            if let Ok(pid_text) = std::fs::read_to_string(&lock) {
                if let Ok(pid) = pid_text.trim().parse::<u32>() {
                    if pid > 1 {
                        let _ = Command::new("/bin/kill")
                            .args(["-KILL", &pid.to_string()])
                            .stderr(std::process::Stdio::null())
                            .status();
                    }
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn fast_create(id: &str, cols: u16, rows: u16, argv: Vec<&str>) -> Create {
    // Pass an empty `env_base` so tests run hermetically and never wait on `launchctl getenv` per create.
    let mut env_base = BTreeMap::new();
    env_base.insert("PATH".into(), "/usr/bin:/bin:/usr/sbin:/sbin".into());
    Create {
        id: id.into(),
        argv: Some(argv.into_iter().map(String::from).collect()),
        cols,
        rows,
        cwd: Some("/tmp".into()),
        env_base: Some(env_base),
        ..Create::default()
    }
}

fn wait_for_event<F>(client: &Termd, timeout: Duration, mut pred: F) -> Event
where
    F: FnMut(&Event) -> bool,
{
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match client.events().recv_timeout(remaining) {
            Ok(ev) => {
                if pred(&ev) {
                    return ev;
                }
                seen.push(ev);
            }
            Err(err) => {
                panic!("timed out waiting for matching event ({err}); earlier events: {seen:?}");
            }
        }
    }
    panic!("timed out waiting for matching event; earlier events: {seen:?}");
}

fn collect_output_until(client: &Termd, handle: u32, needle: &[u8], timeout: Duration) -> Vec<u8> {
    let deadline = Instant::now() + timeout;
    let mut collected = Vec::new();
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match client.events().recv_timeout(remaining) {
            Ok(Event::Output { handle: h, bytes, .. }) if h == handle => {
                collected.extend_from_slice(&bytes);
                if collected.windows(needle.len()).any(|win| win == needle) {
                    return collected;
                }
            }
            Ok(Event::Repaint { handle: h, bytes, .. }) if h == handle => {
                collected.extend_from_slice(&bytes);
                if collected.windows(needle.len()).any(|win| win == needle) {
                    return collected;
                }
            }
            Ok(_) => {}
            Err(err) => {
                panic!(
                    "timed out waiting for {:?} ({err}); collected so far: {:?}",
                    String::from_utf8_lossy(needle),
                    String::from_utf8_lossy(&collected)
                );
            }
        }
    }
    panic!(
        "timed out waiting for {:?}; collected: {:?}",
        String::from_utf8_lossy(needle),
        String::from_utf8_lossy(&collected)
    );
}

#[test]
fn handshake_and_list_round_trip() {
    let Some(bin) = require_termd_bin() else { return };
    let scratch = ScratchDaemon::new("ls", bin);
    let client = scratch.start_client(Box::new(|| {}));

    let welcome = client.welcome();
    assert_eq!(welcome.proto[0], 1);
    assert!(welcome.proto[1] >= 5, "expected protocol >= 1.5, got {:?}", welcome.proto);
    assert_eq!(welcome.ns.as_deref(), Some(scratch.ns.as_str()));
    for feature in ["raw", "view", "key", "paste", "mouse", "paste_stream", "status"] {
        assert!(welcome.has_feature(feature), "missing feature {feature} in {welcome:?}");
    }

    assert!(client.list().unwrap().is_empty());

    // Also cover a default-env `create` (no `env_base`) so macOS session env setup is exercised.
    let created = client
        .create(&Create {
            id: "t-list".into(),
            argv: Some(vec!["/bin/sleep".into(), "30".into()]),
            cols: 90,
            rows: 28,
            cwd: Some("/tmp".into()),
            ..Create::default()
        })
        .unwrap();
    assert_eq!(created.id, "t-list");
    assert!(created.created);
    assert_eq!((created.cols, created.rows), (90, 28));

    let listed = client.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "t-list");
    assert_eq!(listed[0].handle, created.handle);
    assert_eq!((listed[0].cols, listed[0].rows), (90, 28));
}

#[test]
fn create_subscribe_sees_repaint_then_live_output() {
    let Some(bin) = require_termd_bin() else { return };
    let scratch = ScratchDaemon::new("out", bin);
    let wakes = Arc::new(AtomicUsize::new(0));
    let wakes_cb = Arc::clone(&wakes);
    let client = scratch.start_client(Box::new(move || {
        wakes_cb.fetch_add(1, Ordering::SeqCst);
    }));

    // Gate printing on a file in the scratch dir so `subscribe` deterministically lands BEFORE
    // the program prints, proving `Repaint` comes first and the program's print arrives as `Output`.
    let gate = scratch.dir.join("go-print");
    let gate_str = gate.display().to_string();
    let created = client
        .create(&fast_create(
            "t-print",
            80,
            24,
            vec![
                "/bin/sh",
                "-c",
                "while [ ! -f \"$1\" ]; do sleep 0.01; done; printf 'STREAM-MARKER-99\\n'; sleep 30",
                "sh",
                &gate_str,
            ],
        ))
        .unwrap();

    let sub = client.subscribe("t-print", Role::Own, Some((80, 24)), 0).unwrap();
    assert_eq!(sub.handle, created.handle);
    assert_eq!(sub.role, Role::Own);

    // First frame for the subscription must be its initial `Repaint`.
    let first = wait_for_event(&client, Duration::from_secs(5), |ev| {
        matches!(ev, Event::Repaint { handle, .. } | Event::Output { handle, .. } if *handle == sub.handle)
    });
    assert!(
        matches!(first, Event::Repaint { handle, view: false, cols: 80, rows: 24, .. } if handle == sub.handle),
        "expected initial live Repaint before Output, got {first:?}"
    );

    // Release the program to print now that we are subscribed.
    std::fs::write(&gate, b"go").unwrap();

    let mut output_bytes = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Event::Output { handle, bytes, .. } =
            client.events().recv_timeout(Duration::from_secs(3)).unwrap()
        {
            if handle == sub.handle {
                output_bytes.extend_from_slice(&bytes);
                if output_bytes.windows(b"STREAM-MARKER-99".len()).any(|w| w == b"STREAM-MARKER-99") {
                    break;
                }
            }
        }
    }
    assert!(
        output_bytes.windows(b"STREAM-MARKER-99".len()).any(|w| w == b"STREAM-MARKER-99"),
        "did not observe STREAM-MARKER-99 in Output frames: {:?}",
        String::from_utf8_lossy(&output_bytes)
    );
    assert!(wakes.load(Ordering::SeqCst) >= 2);
}

#[test]
fn key_events_and_raw_input_reach_echoing_program() {
    let Some(bin) = require_termd_bin() else { return };
    let scratch = ScratchDaemon::new("key", bin);
    let client = scratch.start_client(Box::new(|| {}));

    client
        .create(&fast_create(
            "t-echo",
            80,
            24,
            vec![
                "/bin/sh",
                "-c",
                "printf 'READY\\n'; IFS= read -r line; printf 'ECHO:%s:\\n' \"$line\"; IFS= read -r raw; printf 'RAW:%s:\\n' \"$raw\"; sleep 10",
            ],
        ))
        .unwrap();
    let sub = client.subscribe("t-echo", Role::Own, Some((80, 24)), 0).unwrap();
    collect_output_until(&client, sub.handle, b"READY", Duration::from_secs(5));

    for ch in ['h', 'i'] {
        let s = ch.to_string();
        client
            .key(
                sub.handle,
                &Key {
                    code: format!("Key{}", ch.to_ascii_uppercase()),
                    key: s.clone(),
                    text: s,
                    ..Key::default()
                },
            )
            .unwrap();
    }
    client
        .key(sub.handle, &Key { code: "Enter".into(), key: "Enter".into(), ..Key::default() })
        .unwrap();

    collect_output_until(&client, sub.handle, b"ECHO:hi:", Duration::from_secs(5));

    client.input(sub.handle, b"rawbytes\r").unwrap();
    collect_output_until(&client, sub.handle, b"RAW:rawbytes:", Duration::from_secs(5));
}

#[test]
fn resize_and_take_produce_size_events() {
    let Some(bin) = require_termd_bin() else { return };
    let scratch = ScratchDaemon::new("sz", bin);
    let owner = scratch.start_client(Box::new(|| {}));
    let attach_client = Termd::connect_with(
        &scratch.target(),
        "second-client",
        scratch.connect_options(),
        Box::new(|| {}),
    )
    .unwrap();

    owner.create(&fast_create("t-size", 80, 24, vec!["/bin/sleep", "30"])).unwrap();
    let own_sub = owner.subscribe("t-size", Role::Own, Some((80, 24)), 0).unwrap();
    let att_sub = attach_client.subscribe("t-size", Role::Attach, None, 0).unwrap();

    // Non-owner resize must fail with `not_owner`.
    let resize_err = attach_client.resize(att_sub.handle, 100, 30).unwrap_err();
    assert!(resize_err.is_code(code::NOT_OWNER), "got {resize_err:?}");

    owner.resize(own_sub.handle, 110, 35).unwrap();
    let size_ev = wait_for_event(&owner, Duration::from_secs(5), |ev| {
        matches!(ev, Event::Size { handle, cols: 110, rows: 35, .. } if *handle == own_sub.handle)
    });
    assert!(matches!(size_ev, Event::Size { cols: 110, rows: 35, .. }));

    // `take` from the attach client demotes the first owner and resizes in one step.
    let new_role = attach_client.take(att_sub.handle, Some((120, 42))).unwrap();
    assert_eq!(new_role, Role::Own);
    wait_for_event(&owner, Duration::from_secs(5), |ev| {
        matches!(ev, Event::Role { handle, role: Role::Attach } if *handle == own_sub.handle)
    });
    wait_for_event(&attach_client, Duration::from_secs(5), |ev| {
        matches!(ev, Event::Size { handle, cols: 120, rows: 42, .. } if *handle == att_sub.handle)
    });
}

#[test]
fn paste_covers_safe_unsafe_and_multi_megabyte_chunked_streams() {
    let Some(bin) = require_termd_bin() else { return };
    let scratch = ScratchDaemon::new("pst", bin);
    let client = scratch.start_client(Box::new(|| {}));

    let sink_file = scratch.dir.join("pasted.bin");
    let sink_str = sink_file.display().to_string();
    // Raw tty (`stty raw -echo`) avoids macOS's 1023-byte `MAX_CANON` line limit and reads 2.3 MiB whole.
    client
        .create(&fast_create(
            "t-paste",
            80,
            24,
            vec![
                "/bin/sh",
                "-c",
                "stty raw -echo; printf 'PASTE-READY\\n'; head -c 2400000 > \"$1\"; printf 'PASTE-DONE\\n'; sleep 10",
                "sh",
                &sink_str,
            ],
        ))
        .unwrap();
    let sub = client.subscribe("t-paste", Role::Own, Some((80, 24)), 0).unwrap();
    collect_output_until(&client, sub.handle, b"PASTE-READY", Duration::from_secs(5));

    // Unbracketed newline with `allow_unsafe = false` must be refused with `unsafe_paste`.
    let unsafe_err = client.paste_checked(sub.handle, "line1\nline2", false).unwrap_err();
    assert!(unsafe_err.is_code(code::UNSAFE_PASTE), "got {unsafe_err:?}");

    // 2,400,000 bytes (> 2 x 1 MiB chunk cap) exercises protocol 1.5 `more: true` chunk streaming!
    let mut payload = String::with_capacity(2_400_000);
    for idx in 0..40_000usize {
        use std::fmt::Write as _;
        let _ = write!(&mut payload, "chunk-row-{idx:05}-abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGH");
    }
    assert_eq!(payload.len(), 2_400_000);

    client.paste(sub.handle, &payload).unwrap();
    collect_output_until(&client, sub.handle, b"PASTE-DONE", Duration::from_secs(15));

    let on_disk = std::fs::read_to_string(&sink_file).unwrap();
    assert_eq!(on_disk.len(), payload.len());
    assert_eq!(on_disk, payload);
}

#[test]
fn mouse_view_scrollback_unsubscribe_and_status_follow_work() {
    let Some(bin) = require_termd_bin() else { return };
    let scratch = ScratchDaemon::new("misc", bin);
    let client = scratch.start_client(Box::new(|| {}));

    // Enable SGR mouse reporting (`?1000h?1006h`), print 40 lines to populate scrollback on a
    // 10-row screen, then print back the first mouse sequence read.
    client
        .create(&fast_create(
            "t-misc",
            60,
            10,
            vec![
                "/bin/sh",
                "-c",
                "stty raw -echo; i=1; while [ $i -le 35 ]; do printf 'LINE-%02d\\r\\n' $i; i=$((i+1)); done; printf '\\033[?1000h\\033[?1006hMOUSE-READY\\r\\n'; dd bs=1 count=9 2>/dev/null | od -An -tx1; sleep 10",
            ],
        ))
        .unwrap();
    let sub = client.subscribe("t-misc", Role::Own, Some((60, 10)), 0).unwrap();
    collect_output_until(&client, sub.handle, b"MOUSE-READY", Duration::from_secs(5));

    // Scroll back 5 rows and observe a view repaint (`view: true`), then return `Live`.
    let view_back = client.view(sub.handle, View::Delta(5)).unwrap();
    assert!(view_back.offset > 0 && view_back.max >= view_back.offset, "got {view_back:?}");
    wait_for_event(&client, Duration::from_secs(5), |ev| {
        matches!(ev, Event::Repaint { handle, view: true, .. } if *handle == sub.handle)
    });

    let view_live = client.view(sub.handle, View::Live).unwrap();
    assert_eq!(view_live.offset, 0);
    wait_for_event(&client, Duration::from_secs(5), |ev| {
        matches!(ev, Event::Repaint { handle, view: false, .. } if *handle == sub.handle)
    });

    // Send a left mouse press at (2, 3) -> SGR `ESC [ < 0 ; 3 ; 4 M` (9 bytes, ending in `4d`).
    client
        .mouse(
            sub.handle,
            &Mouse {
                action: MouseAction::Press,
                button: MouseButton::Left,
                col: 2.0,
                row: 3.0,
                mods: Vec::new(),
            },
        )
        .unwrap();
    collect_output_until(&client, sub.handle, b"1b", Duration::from_secs(5));

    // `status_follow` immediately pushes one `Event::Status` document naming our terminal.
    let interval = client.status_follow(Some(250)).unwrap();
    assert!(interval >= 250);
    let status_ev = wait_for_event(&client, Duration::from_secs(5), |ev| {
        matches!(ev, Event::Status(_))
    });
    if let Event::Status(doc) = status_ev {
        let terms = doc["terminals"].as_array().expect("terminals array in status");
        assert!(terms.iter().any(|t| t["id"] == "t-misc"));
    }
    client.status_unfollow().unwrap();
    client.unsubscribe(sub.handle).unwrap();
}

#[test]
fn kill_ends_in_exit_and_closed() {
    let Some(bin) = require_termd_bin() else { return };
    let scratch = ScratchDaemon::new("kill", bin);
    let client = scratch.start_client(Box::new(|| {}));

    client.create(&fast_create("t-kill", 80, 24, vec!["/bin/sleep", "60"])).unwrap();
    let sub = client.subscribe("t-kill", Role::Own, Some((80, 24)), 0).unwrap();

    client.kill("t-kill").unwrap();

    let mut saw_exit = false;
    let mut saw_closed = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !saw_closed && Instant::now() < deadline {
        match client.events().recv_timeout(Duration::from_secs(3)).unwrap() {
            Event::Exit { handle, .. } if handle == sub.handle => saw_exit = true,
            Event::Closed { handle } if handle == sub.handle => {
                assert!(saw_exit, "Exit must precede Closed");
                saw_closed = true;
            }
            _ => {}
        }
    }
    assert!(saw_closed, "expected Closed event after kill");
    assert!(client.list().unwrap().is_empty());
}

#[test]
fn two_threads_make_requests_while_output_streams() {
    let Some(bin) = require_termd_bin() else { return };
    let scratch = ScratchDaemon::new("conc", bin);
    let client = scratch.start_client(Box::new(|| {}));

    client
        .create(&fast_create(
            "t-stream",
            80,
            24,
            vec![
                "/bin/sh",
                "-c",
                "i=0; while [ $i -lt 400 ]; do printf 'STREAM-LINE-%04d-xxxxxxxxxxxxxxxxxxxxxxxxxxxx\\n' $i; i=$((i+1)); done; printf 'STREAM-FINISHED\\n'; sleep 10",
            ],
        ))
        .unwrap();
    let sub = client.subscribe("t-stream", Role::Own, Some((80, 24)), 0).unwrap();

    let handle_a = client.handle();
    let handle_b = client.handle();

    let thread_a = std::thread::spawn(move || {
        for _ in 0..25 {
            let terms = handle_a.list().unwrap();
            assert_eq!(terms.len(), 1);
            assert_eq!(terms[0].id, "t-stream");
        }
    });

    let sub_handle = sub.handle;
    let thread_b = std::thread::spawn(move || {
        for step in 0..25u16 {
            handle_b.ping().unwrap();
            if step % 5 == 0 {
                handle_b.repaint(sub_handle).unwrap();
            }
        }
    });

    collect_output_until(&client, sub.handle, b"STREAM-FINISHED", Duration::from_secs(10));
    thread_a.join().unwrap();
    thread_b.join().unwrap();
}

#[test]
fn rides_daemon_upgrade_restart_resubscribing_and_continuing() {
    let Some(bin) = require_termd_bin() else { return };
    let scratch = ScratchDaemon::new("upg", bin.clone());
    let client = scratch.start_client(Box::new(|| {}));

    let gen_before = client.welcome().generation.unwrap_or(1);
    client
        .create(&fast_create(
            "t-survive",
            80,
            24,
            vec![
                "/bin/sh",
                "-c",
                "printf 'BOOTED\\n'; while IFS= read -r line; do printf 'SURVIVED:%s:\\n' \"$line\"; done",
            ],
        ))
        .unwrap();
    let sub = client.subscribe("t-survive", Role::Own, Some((80, 24)), 0).unwrap();
    collect_output_until(&client, sub.handle, b"BOOTED", Duration::from_secs(5));

    // Trigger an engine upgrade via a separate control connection so our main client experiences
    // `ERROR restart` exactly as the live wall does when `g3d upgrade` runs.
    let upgrader = Termd::connect_with(
        &scratch.target(),
        "upgrader",
        scratch.connect_options(),
        Box::new(|| {}),
    )
    .unwrap();
    let accepted_gen = upgrader.upgrade(Some(&bin), false).unwrap();
    assert_eq!(accepted_gen, gen_before);
    drop(upgrader);

    // Wait for `Event::Reconnected` and the fresh `Event::Repaint` that follows re-subscription.
    let reconn_ev = wait_for_event(&client, Duration::from_secs(10), |ev| {
        matches!(ev, Event::Reconnected { .. })
    });
    match reconn_ev {
        Event::Reconnected { restart, generation } => {
            assert!(restart, "expected planned restart=true on upgrade");
            assert!(
                generation.unwrap_or(0) > gen_before,
                "expected generation > {gen_before}, got {generation:?}"
            );
        }
        other => panic!("unexpected event {other:?}"),
    }

    wait_for_event(&client, Duration::from_secs(5), |ev| {
        matches!(ev, Event::Repaint { handle, .. } if *handle == sub.handle)
    });

    // Verify the terminal is still alive and taking input on the new brain generation!
    client.paste(sub.handle, "after-upgrade\n").unwrap();
    collect_output_until(&client, sub.handle, b"SURVIVED:after-upgrade:", Duration::from_secs(5));
}

#[test]
fn rides_unplanned_brain_crash_and_restores_subscription() {
    let Some(bin) = require_termd_bin() else { return };
    let scratch = ScratchDaemon::new("crash", bin);
    let client = scratch.start_client(Box::new(|| {}));

    let welcome = client.welcome();
    let brain_pid = welcome.pid.expect("WELCOME carries brain pid");
    let holder_pid = welcome.holder.as_ref().expect("WELCOME carries holder").pid;
    assert_ne!(brain_pid, holder_pid);

    client
        .create(&fast_create(
            "t-crash",
            80,
            24,
            vec![
                "/bin/sh",
                "-c",
                "printf 'CRASH-BOOT\\n'; while IFS= read -r line; do printf 'AFTER-CRASH:%s:\\n' \"$line\"; done",
            ],
        ))
        .unwrap();
    let sub = client.subscribe("t-crash", Role::Own, Some((80, 24)), 0).unwrap();
    collect_output_until(&client, sub.handle, b"CRASH-BOOT", Duration::from_secs(5));

    // Kill ONLY the brain with SIGKILL; the holder stays alive and spawns brain generation 2.
    let status = Command::new("/bin/kill")
        .args(["-KILL", &brain_pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success());

    let reconn_ev = wait_for_event(&client, Duration::from_secs(10), |ev| {
        matches!(ev, Event::Reconnected { .. })
    });
    assert_eq!(reconn_ev, Event::Reconnected { restart: false, generation: Some(2) });

    wait_for_event(&client, Duration::from_secs(5), |ev| {
        matches!(ev, Event::Repaint { handle, .. } if *handle == sub.handle)
    });

    client.paste(sub.handle, "still-here\n").unwrap();
    collect_output_until(&client, sub.handle, b"AFTER-CRASH:still-here:", Duration::from_secs(5));
}

#[test]
fn target_namespace_honours_g3d_termd_dir_env_in_subprocess() {
    // Child entrypoint when invoked with `TERMD_ENV_CHILD_DIR`.
    if let Ok(child_dir) = std::env::var("TERMD_ENV_CHILD_DIR") {
        let dir_path = Path::new(&child_dir);
        assert_eq!(termd_client::paths::socket_dir(), dir_path);
        let client = Termd::connect_or_start(
            &Target::namespace("env-sub"),
            "env-child",
            Box::new(|| {}),
        )
        .expect("connect_or_start with G3D_TERMD_DIR");
        assert!(dir_path.join("termd-env-sub.sock").exists());
        assert!(client.list().unwrap().is_empty());
        client.stop().unwrap();
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "ENV_CHILD_OK");
        return;
    }

    let Some(bin) = require_termd_bin() else { return };
    let scratch = ScratchDaemon::new("env", bin);
    let current_test_exe = std::env::current_exe().unwrap();
    let output = Command::new(current_test_exe)
        .args([
            "--exact",
            "target_namespace_honours_g3d_termd_dir_env_in_subprocess",
            "--nocapture",
        ])
        .env("G3D_TERMD_DIR", &scratch.dir)
        .env("TERMD_ENV_CHILD_DIR", &scratch.dir)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("ENV_CHILD_OK"),
        "subprocess failed ({}): stdout={stdout}\nstderr={stderr}",
        output.status
    );
    let _ = Error::Start(String::new());
    let _ = Subscribe {
        id: "x".into(),
        mode: Mode::Raw,
        role: Role::Watch,
        history_rows: None,
        flow: None,
        cols: None,
        rows: None,
    };
}
