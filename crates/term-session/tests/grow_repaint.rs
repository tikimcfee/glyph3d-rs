//! Who repaints after a primary-screen grow, by the daemon's protocol, against a mock daemon.
//!
//! A terminal that grows pulls history rows out of the daemon's scrollback, and a raw
//! subscriber needs a `REPAINT` to see them. A protocol 1.6 `g3d-termd` sends one itself right
//! after `size` whenever rows were pulled (PROTOCOL.md `resize`); a 1.5 one sends `size` alone.
//! So the session asks a 1.5 daemon once per drag, and a 1.6 daemon never.
//!
//! No daemon here speaks 1.5 any more, so both are a mock: a Unix socket that answers `HELLO`
//! with the protocol under test, answers `subscribe`, `resize` and `repaint` as the daemon
//! does, and counts the `repaint` requests it was sent. (The live counterpart, against the
//! installed daemon, is `session_live.rs`'s
//! `primary_grow_resize_pulls_scrollback_coalesced_and_burst_settles`.)

use std::fs;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use term_session::{HubOptions, SessionHub, Target, DEFAULT_GROW_REPAINT_SETTLE};
use termd_client::frame::{self, ty, Frame, FrameDecoder, RepaintHead};

static NEXT: AtomicUsize = AtomicUsize::new(1);

/// The terminal's handle on the mock's one connection.
const HANDLE: u32 = 4;

/// How the mock answers a `resize`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AfterSize {
    /// `size` alone: a 1.5 daemon always, a 1.6 one when the resize pulled no row.
    Nothing,
    /// `size`, then a live `REPAINT`: a 1.6 daemon whose resize pulled rows.
    Repaint,
}

#[derive(Default)]
struct Seen {
    /// `repaint` requests the client sent.
    repaint_requests: usize,
    /// Every request's `op`, in order.
    ops: Vec<String>,
}

struct Mock {
    dir: PathBuf,
    seen: Arc<Mutex<Seen>>,
}

impl Mock {
    fn socket(&self) -> PathBuf {
        self.dir.join("termd.sock")
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn read_frame(sock: &mut UnixStream, dec: &mut FrameDecoder) -> Option<Frame> {
    let mut buf = [0u8; 4096];
    loop {
        if let Some(f) = dec.next_frame().expect("a well-formed frame") {
            return Some(f);
        }
        match sock.read(&mut buf) {
            Ok(0) | Err(_) => return None,
            Ok(n) => dec.feed(&buf[..n]),
        }
    }
}

/// One line of screen per row, as a daemon's live `REPAINT` paints it.
fn repaint_frame(seq: u64, cols: u16, rows: u16) -> Vec<u8> {
    let mut vt = String::from("\x1b[H\x1b[2J");
    for r in 1..=rows {
        vt.push_str(&format!("row {r} of {rows}"));
        if r < rows {
            vt.push_str("\r\n");
        }
    }
    frame::encode_repaint(HANDLE, RepaintHead { seq, cols, rows, flags: 0 }, vt.as_bytes())
}

/// Starts a mock daemon speaking `proto` whose every `resize` is followed as `after` says.
fn mock_daemon(proto: [u32; 2], after: AfterSize) -> Mock {
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from(format!("/tmp/g3d-s-gr-{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch dir");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    let seen = Arc::new(Mutex::new(Seen::default()));
    let mock = Mock { dir, seen: Arc::clone(&seen) };
    let listener = UnixListener::bind(mock.socket()).expect("bind the mock's socket");

    thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else { return };
        let mut dec = FrameDecoder::default();
        let Some(hello) = read_frame(&mut stream, &mut dec) else { return };
        assert_eq!(hello.ty, ty::HELLO);
        let welcome = serde_json::json!({
            "proto": proto,
            "daemon": "g3d-termd/mock",
            "engine": "mock-vt",
            "ns": "mock",
            "features": ["raw", "view", "status", "key", "paste", "mouse"],
            "max_frame": 16 << 20,
            "instance": "inst-1",
            "generation": 1,
        });
        if stream.write_all(&frame::encode_json(ty::WELCOME, 0, &welcome)).is_err() {
            return;
        }
        let (mut cols, mut rows) = (80u16, 24u16);
        let mut seq = 0u64;
        while let Some(req) = read_frame(&mut stream, &mut dec) {
            if req.ty != ty::REQUEST {
                continue;
            }
            let body: serde_json::Value = serde_json::from_slice(&req.payload).expect("a JSON request");
            let id = body["req"].clone();
            let op = body["op"].as_str().unwrap_or_default().to_owned();
            {
                let mut seen = seen.lock().expect("seen lock");
                seen.ops.push(op.clone());
                if op == "repaint" {
                    seen.repaint_requests += 1;
                }
            }
            let mut out = Vec::new();
            match op.as_str() {
                "subscribe" => {
                    out.extend(frame::encode_json(
                        ty::REPLY,
                        0,
                        &serde_json::json!({
                            "req": id, "ok": true, "handle": HANDLE, "role": "own",
                            "seq": seq, "cols": cols, "rows": rows,
                        }),
                    ));
                    seq += 1;
                    out.extend(repaint_frame(seq, cols, rows));
                }
                "resize" => {
                    cols = body["cols"].as_u64().and_then(|c| u16::try_from(c).ok()).unwrap_or(cols);
                    rows = body["rows"].as_u64().and_then(|r| u16::try_from(r).ok()).unwrap_or(rows);
                    seq += 1;
                    // As the daemon orders them: `size` before the resize's reply.
                    out.extend(frame::encode_json(
                        ty::EVENT,
                        HANDLE,
                        &serde_json::json!({"ev": "size", "cols": cols, "rows": rows, "seq": seq}),
                    ));
                    if after == AfterSize::Repaint {
                        seq += 1;
                        out.extend(repaint_frame(seq, cols, rows));
                    }
                    out.extend(frame::encode_json(ty::REPLY, 0, &serde_json::json!({"req": id, "ok": true})));
                }
                "repaint" => {
                    out.extend(frame::encode_json(ty::REPLY, 0, &serde_json::json!({"req": id, "ok": true})));
                    seq += 1;
                    out.extend(repaint_frame(seq, cols, rows));
                }
                _ => {
                    out.extend(frame::encode_json(ty::REPLY, 0, &serde_json::json!({"req": id, "ok": true})));
                }
            }
            if stream.write_all(&out).is_err() {
                return;
            }
        }
    });
    mock
}

fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if check() {
            return;
        }
        thread::sleep(Duration::from_millis(2));
    }
    panic!("timed out waiting for {what}");
}

/// A five-step primary-screen grow (80x24 to 80x29) against a daemon speaking `proto` that
/// follows each resize as `after` says, then a shrink: `(repaint requests for the grow, for
/// the shrink)`, each counted by the mock, after the session's settle time has run out.
fn drag(proto: [u32; 2], after: AfterSize) -> (usize, usize) {
    let mock = mock_daemon(proto, after);
    let hub = SessionHub::connect_with(&Target::socket(mock.socket()), HubOptions::default(), Arc::new(|| {}))
        .expect("connect to the mock");
    assert_eq!(hub.welcome().proto, proto);
    let mut session = hub.attach_owner("t1", Some((80, 24))).expect("subscribe");
    wait_until("the first screen", || {
        session.pump();
        session.screen().rows == 24 && session.screen().generation > 0
    });

    for rows in 25..=29u16 {
        session.resize(80, rows).expect("resize");
        thread::sleep(Duration::from_millis(3));
    }
    wait_until("the grown screen", || {
        session.pump();
        session.screen().rows == 29
    });
    // The session asks only once a drag has been still for its settle time: let it run out,
    // several times over, before counting.
    thread::sleep(DEFAULT_GROW_REPAINT_SETTLE * 8);
    session.pump();
    let for_grow = mock.seen.lock().expect("seen lock").repaint_requests;
    assert_eq!(
        session.stats().repaints_sent as usize,
        for_grow,
        "the session's own count is what reached the daemon"
    );

    session.resize(80, 20).expect("shrink");
    wait_until("the shrunk screen", || {
        session.pump();
        session.screen().rows == 20
    });
    thread::sleep(DEFAULT_GROW_REPAINT_SETTLE * 8);
    let seen = mock.seen.lock().expect("seen lock");
    assert_eq!(
        seen.ops.iter().filter(|op| *op == "resize").count(),
        6,
        "every resize reached the daemon: {:?}",
        seen.ops
    );
    (for_grow, seen.repaint_requests - for_grow)
}

#[test]
fn a_1_5_daemon_is_asked_once_per_grow_and_a_1_6_daemon_never() {
    // 1.5: `size` comes alone, so the rows a grow pulled are the session's to ask for. Once
    // for the whole drag, after it settles; a shrink pulls nothing and asks nothing.
    assert_eq!(drag([1, 5], AfterSize::Nothing), (1, 0), "protocol 1.5");

    // 1.6, rows pulled: the daemon repaints after each `size`. Nothing to ask.
    assert_eq!(drag([1, 6], AfterSize::Repaint), (0, 0), "protocol 1.6, the daemon repaints");

    // 1.6, nothing pulled (no scrollback left): `size` comes alone, as from a 1.5 daemon,
    // and there is still nothing to ask: no row came out of history. Until 2026-11-27 the
    // session asked here, once per drag, for a copy of the screen it already had.
    assert_eq!(drag([1, 6], AfterSize::Nothing), (0, 0), "protocol 1.6, nothing pulled");

    // Any later minor repaints as 1.6 does. (Another major never gets this far: the client
    // refuses its `WELCOME`.)
    assert_eq!(drag([1, 9], AfterSize::Nothing), (0, 0), "protocol 1.9");

    // A minor below 1.5 is asked like 1.5.
    assert_eq!(drag([1, 4], AfterSize::Nothing), (1, 0), "protocol 1.4");
}
