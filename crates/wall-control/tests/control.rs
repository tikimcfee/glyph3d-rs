use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use wall_control::{
    format_command_line, paths, send_one, Client, Error, Server, Target, WakeFn,
};

static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(1);

/// Short-path (`< 50` B) `0700` scratch directory in `/tmp` cleaned up on drop.
struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    fn new() -> Self {
        let seq = SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = PathBuf::from(format!("/tmp/g3d-wc-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&path)
            .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        ScratchDir { path }
    }

    fn target(&self, ns: &str) -> Target {
        Target::namespace_in(&self.path, ns)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Stand-in wall application running on its own thread: wakes via `wake`, drains
/// `server.try_recv()`, and answers verbs using a deterministic test router.
struct StandInApp {
    socket_path: PathBuf,
    wakes: Arc<AtomicU64>,
    app_thread: Option<thread::JoinHandle<()>>,
    stop_tx: std::sync::mpsc::Sender<()>,
    wake_tx: std::sync::mpsc::Sender<()>,
}

impl StandInApp {
    fn start(target: &Target) -> Result<Self, Error> {
        let (wake_tx, wake_rx) = std::sync::mpsc::channel::<()>();
        let wakes = Arc::new(AtomicU64::new(0));
        let wakes_cb = Arc::clone(&wakes);
        let wake_tx_cb = wake_tx.clone();
        let wake: WakeFn = Arc::new(move || {
            wakes_cb.fetch_add(1, Ordering::SeqCst);
            let _ = wake_tx_cb.send(());
        });

        let server = Server::bind(target, wake)?;
        let socket_path = server.socket_path().to_path_buf();

        let (srv_tx, srv_rx) = std::sync::mpsc::channel::<Server>();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();

        let app_thread = thread::spawn(move || {
            let srv = srv_rx.recv().unwrap();
            let mut counter: u64 = 0;
            loop {
                while let Some(req) = srv.try_recv() {
                    counter += 1;
                    let reply = dispatch_stand_in(req.line(), counter);
                    req.reply(reply);
                }
                if stop_rx.try_recv().is_ok() {
                    drop(srv);
                    break;
                }
                let _ = wake_rx.recv_timeout(Duration::from_millis(50));
            }
        });
        srv_tx.send(server).unwrap();

        Ok(StandInApp {
            socket_path,
            wakes,
            app_thread: Some(app_thread),
            stop_tx,
            wake_tx,
        })
    }
}

impl Drop for StandInApp {
    fn drop(&mut self) {
        let _ = self.stop_tx.send(());
        let _ = self.wake_tx.send(());
        if let Some(h) = self.app_thread.take() {
            let _ = h.join();
        }
    }
}

fn dispatch_stand_in(line: &str, seq: u64) -> String {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return "ERR: empty command".into();
    }
    if trimmed == "wall.split right" {
        return format!("OK: split right #{seq}");
    }
    if trimmed == "help" {
        return "╔══ help ══════════════════╗\n║ wall.split   split pane  ║\n║                          ║\n║ .leading-dot stays whole ║\n╚══════════════════════════╝".into();
    }
    if let Some(rest) = trimmed.strip_prefix("echo ") {
        return format!("OK: {rest}");
    }
    if trimmed.starts_with("count") {
        return format!("OK: #{seq} ({trimmed})");
    }
    format!("ERR: unknown command '{trimmed}'. Try 'help'.")
}

#[test]
fn single_request_wakes_app_sets_0600_mode_and_supports_raw_nc_line_protocol() {
    let dir = ScratchDir::new();
    let target = dir.target("one");
    let app = StandInApp::start(&target).unwrap();

    // Socket file has mode 0600 (`srw-------`), parent dir has mode 0700.
    let sock_meta = std::fs::symlink_metadata(&app.socket_path).unwrap();
    assert_eq!(sock_meta.permissions().mode() & 0o777, 0o600);
    let dir_meta = std::fs::symlink_metadata(&dir.path).unwrap();
    assert_eq!(dir_meta.permissions().mode() & 0o777, 0o700);

    // Client API: one request + refusal check.
    let reply = send_one(&target, "wall.split right").unwrap();
    assert!(reply.is_ok());
    assert_eq!(reply.text, "OK: split right #1");
    assert!(app.wakes.load(Ordering::SeqCst) >= 1);

    let bad = send_one(&target, "wall.bogus").unwrap();
    assert!(bad.is_err());
    assert_eq!(bad.text, "ERR: unknown command 'wall.bogus'. Try 'help'.");

    // Raw `nc -U` usage: write `<verb>\n`, read `<len>\n<reply>\n`.
    let mut raw = UnixStream::connect(&app.socket_path).unwrap();
    raw.write_all(b"wall.split right\n").unwrap();
    raw.flush().unwrap();
    let mut reader = BufReader::new(raw);
    let mut len_line = String::new();
    reader.read_line(&mut len_line).unwrap();
    let expected_body = "OK: split right #3";
    assert_eq!(len_line, format!("{}\n", expected_body.len()));
    let mut body_line = String::new();
    reader.read_line(&mut body_line).unwrap();
    assert_eq!(body_line, format!("{expected_body}\n"));
}

#[test]
fn many_requests_in_a_row_on_one_connection_and_multiline_replies_round_trip() {
    let dir = ScratchDir::new();
    let target = dir.target("many");
    let _app = StandInApp::start(&target).unwrap();

    let mut client = Client::connect(&target).unwrap();
    client.set_timeout(Some(Duration::from_secs(5))).unwrap();

    for i in 1..=25 {
        let rep = client.send(&format!("count {i}")).unwrap();
        assert!(rep.is_ok());
        assert_eq!(rep.text, format!("OK: #{i} (count {i})"));
    }

    // Multi-line reply containing Unicode box characters, a blank line, and a leading `.`
    // mid-connection, followed immediately by another single-line request on the same socket.
    let help = client.send("help").unwrap();
    assert!(help.is_ok());
    assert_eq!(help.text.lines().count(), 5);
    assert!(help.text.contains(".leading-dot stays whole"));

    let after = client.send("wall.split right").unwrap();
    assert_eq!(after.text, "OK: split right #27");
}

#[test]
fn two_clients_at_once_interleave_cleanly_without_head_of_line_blocking() {
    let dir = ScratchDir::new();
    let target = dir.target("concurrent");
    let _app = StandInApp::start(&target).unwrap();

    // Client A opens and holds an idle connection while Client B connects and runs verbs;
    // then both send bursts concurrently across a barrier.
    let mut idle_client = Client::connect(&target).unwrap();
    idle_client
        .set_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    let quick = send_one(&target, "echo while-idle-open").unwrap();
    assert_eq!(quick.text, "OK: while-idle-open");

    let barrier = Arc::new(Barrier::new(2));
    let t_a = target.clone();
    let b_a = Arc::clone(&barrier);
    let handle_a = thread::spawn(move || {
        let mut c = Client::connect(&t_a).unwrap();
        c.set_timeout(Some(Duration::from_secs(5))).unwrap();
        b_a.wait();
        for i in 0..20 {
            let r = c.send(&format!("echo a-{i}")).unwrap();
            assert_eq!(r.text, format!("OK: a-{i}"));
        }
    });

    let t_b = target.clone();
    let b_b = Arc::clone(&barrier);
    let handle_b = thread::spawn(move || {
        let mut c = Client::connect(&t_b).unwrap();
        c.set_timeout(Some(Duration::from_secs(5))).unwrap();
        b_b.wait();
        for i in 0..20 {
            let r = c.send(&format!("echo b-{i}")).unwrap();
            assert_eq!(r.text, format!("OK: b-{i}"));
        }
    });

    handle_a.join().unwrap();
    handle_b.join().unwrap();

    let final_idle = idle_client.send("echo still-alive").unwrap();
    assert_eq!(final_idle.text, "OK: still-alive");
}

#[test]
fn dead_socket_takeover_succeeds_and_live_second_server_is_refused() {
    let dir = ScratchDir::new();
    let target = dir.target("takeover");
    let sock_path = target.resolve_socket().unwrap();

    // Simulate a stale socket file left behind by a SIGKILLed process: bind a raw
    // UnixListener and drop its fd WITHOUT unlinking the socket path on disk.
    {
        let orphan = std::os::unix::net::UnixListener::bind(&sock_path).unwrap();
        drop(orphan);
    }
    assert!(std::fs::symlink_metadata(&sock_path).is_ok());

    // First server takes over the dead stale socket cleanly.
    let app1 = StandInApp::start(&target).unwrap();
    assert_eq!(
        send_one(&target, "echo taken-over").unwrap().text,
        "OK: taken-over"
    );

    // A second server in the same namespace is refused while `app1` is alive, and
    // `app1`'s socket continues serving traffic undisturbed.
    let err = match Server::bind_without_wake(&target) {
        Ok(_) => panic!("second server should have been refused"),
        Err(err) => err,
    };
    assert!(
        matches!(err, Error::AlreadyRunning { .. }),
        "expected AlreadyRunning, got {err:?}"
    );
    assert!(err.to_string().contains("already running"));
    assert_eq!(
        send_one(&target, "echo still-serving").unwrap().text,
        "OK: still-serving"
    );

    // Once `app1` drops cleanly, its socket is removed and a new server can bind.
    drop(app1);
    assert!(std::fs::symlink_metadata(&sock_path).is_err());
    let _app2 = StandInApp::start(&target).unwrap();
    assert_eq!(
        send_one(&target, "echo restarted").unwrap().text,
        "OK: restarted"
    );
}

#[test]
fn foreign_uid_peers_are_refused_in_unit_and_live_server_checks() {
    let my_uid = paths::current_uid();
    let foreign_uid = my_uid.wrapping_add(1);

    // 1. Direct unit test of `verify_peer_uid` on a connected UnixStream pair.
    let (sock_a, _sock_b) = UnixStream::pair().unwrap();
    assert!(paths::verify_peer_uid(&sock_a, my_uid).is_ok());
    let uid_err = paths::verify_peer_uid(&sock_a, foreign_uid).unwrap_err();
    assert_eq!(uid_err.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(uid_err.to_string().contains(&foreign_uid.to_string()));

    // 2. Live `Server` configured with `expected_uid = foreign_uid` immediately drops
    // incoming connections from `my_uid` without queuing any `Request` or waking the app.
    let dir = ScratchDir::new();
    let target = dir.target("foreign");
    let wakes = Arc::new(AtomicU64::new(0));
    let wakes_cb = Arc::clone(&wakes);
    let server = Server::builder(target.clone())
        .expected_uid(foreign_uid)
        .wake(Arc::new(move || {
            wakes_cb.fetch_add(1, Ordering::SeqCst);
        }))
        .bind()
        .unwrap();

    let mut client = Client::connect(&target).unwrap();
    client
        .set_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let send_res = client.send("wall.split right");
    assert!(send_res.is_err(), "foreign-uid connection must be dropped");
    assert!(server.try_recv().is_none());
    assert_eq!(wakes.load(Ordering::SeqCst), 0);

    // 3. Client-side foreign-UID check also refuses a server of unexpected UID.
    let dir2 = ScratchDir::new();
    let target2 = dir2.target("client-uid");
    let _app = StandInApp::start(&target2).unwrap();
    let client_err = match Client::connect_with_expected_uid(&target2, foreign_uid) {
        Ok(_) => panic!("client should refuse unexpected server uid"),
        Err(e) => e,
    };
    assert!(client_err.to_string().contains(&foreign_uid.to_string()));

    // 4. Unsafe parent directory permissions (`0755`) and symlinks are refused too.
    let unsafe_dir = dir.path.join("world-readable");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&unsafe_dir)
        .unwrap();
    std::fs::set_permissions(&unsafe_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let unsafe_target = Target::namespace_in(&unsafe_dir, "bad");
    assert!(Server::bind_without_wake(&unsafe_target).is_err());

    let sym_dir = dir.path.join("sym");
    std::os::unix::fs::symlink(&dir2.path, &sym_dir).unwrap();
    let sym_target = Target::namespace_in(&sym_dir, "bad");
    assert!(Server::bind_without_wake(&sym_target).is_err());

    // 5. Socket path rules (`socket_dir_for`, `validate_namespace`, 100-byte limit).
    assert_eq!(
        paths::socket_dir_for(Some(PathBuf::from("/run/user/1000")), 1000),
        PathBuf::from("/run/user/1000/g3d")
    );
    assert_eq!(
        paths::socket_dir_for(None, 501),
        PathBuf::from("/tmp/g3d-501")
    );
    assert!(paths::validate_namespace("rs-wall_1.0").is_ok());
    assert!(paths::validate_namespace("").is_err());
    assert!(paths::validate_namespace("bad/slash").is_err());
    let overlong = Target::socket(format!("/tmp/{}.sock", "x".repeat(100)));
    assert!(overlong.resolve_socket().is_err());
}

#[test]
fn format_command_line_quotes_spaces_and_escapes_special_chars() {
    assert_eq!(
        format_command_line(&["wall.split", "right"]),
        "wall.split right"
    );
    assert_eq!(
        format_command_line(&["wall.move", "g-1", "-10", "-20"]),
        "wall.move g-1 -10 -20"
    );
    assert_eq!(
        format_command_line(&["wall.rename", "t1", "my shell \"1\""]),
        "wall.rename t1 \"my shell \\\"1\\\"\""
    );
    assert_eq!(
        format_command_line(&["terminal.paste", "t1", "a\nb\tc\\d"]),
        "terminal.paste t1 \"a\\nb\\tc\\\\d\""
    );
}

#[test]
fn wallctl_binary_runs_single_verbs_refusals_and_stdin_batches() {
    let dir = ScratchDir::new();
    let target = dir.target("cli");
    let sock_path = target.resolve_socket().unwrap();
    let _app = StandInApp::start(&target).unwrap();
    let bin = env!("CARGO_BIN_EXE_wallctl");

    // 1. Single verb -> prints reply and exits 0 (using G3D_TERMD_DIR + --ns).
    let ok_out = Command::new(bin)
        .env("G3D_TERMD_DIR", &dir.path)
        .args(["--ns", "cli", "wall.split", "right"])
        .output()
        .unwrap();
    assert!(ok_out.status.success());
    assert_eq!(String::from_utf8_lossy(&ok_out.stdout), "OK: split right #1\n");

    // 2. Refused verb -> prints `ERR: ...` refusal and exits 1.
    let err_out = Command::new(bin)
        .env("G3D_TERMD_DIR", &dir.path)
        .args(["--ns", "cli", "wall.nope"])
        .output()
        .unwrap();
    assert_eq!(err_out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&err_out.stdout),
        "ERR: unknown command 'wall.nope'. Try 'help'.\n"
    );

    // 3. Stdin mode (no verb) -> sends multiple commands on one connection, including
    // multi-line `help`, exits 0 when all succeed, and exits 1 when any command refuses.
    let mut child_ok = Command::new(bin)
        .args(["--socket", sock_path.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child_ok
        .stdin
        .take()
        .unwrap()
        .write_all(b"echo first\n\nhelp\necho third\n")
        .unwrap();
    let batch_ok = child_ok.wait_with_output().unwrap();
    assert!(batch_ok.status.success());
    let batch_stdout = String::from_utf8_lossy(&batch_ok.stdout);
    assert!(batch_stdout.contains("OK: first\n"));
    assert!(batch_stdout.contains(".leading-dot stays whole"));
    assert!(batch_stdout.contains("OK: third\n"));

    let mut child_err = Command::new(bin)
        .args(["--socket", sock_path.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child_err
        .stdin
        .take()
        .unwrap()
        .write_all(b"echo ok-before\nwall.bad\necho ok-after\n")
        .unwrap();
    let batch_err = child_err.wait_with_output().unwrap();
    assert_eq!(batch_err.status.code(), Some(1));
    let err_stdout = String::from_utf8_lossy(&batch_err.stdout);
    assert!(err_stdout.contains("OK: ok-before\n"));
    assert!(err_stdout.contains("ERR: unknown command 'wall.bad'. Try 'help'.\n"));
    assert!(err_stdout.contains("OK: ok-after\n"));
}
