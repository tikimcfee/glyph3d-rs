//! `termd_record`: run a command inside an isolated scratch `g3d-termd` daemon at a
//! chosen `COLSxROWS` size, record the exact byte stream a subscriber receives
//! (`REPAINT` + `OUTPUT` frames in wire order), write it to a file, and stop + remove
//! the scratch daemon afterward.
//!
//! Safety rules (`out/WALL-PLAN-2026-11-26.md`):
//! - Never connects to live namespaces `wall`, `agents`, or `default`.
//! - Always creates its own `0700` scratch directory `/tmp/g3d-rec-<pid>` (keeping the
//!   Unix domain socket path well under 100 bytes), sets `G3D_TERMD_DIR` to it, uses a
//!   scratch namespace (`rec`), and stops the daemon + removes the directory on exit.
//!
//! Usage:
//! ```sh
//! cargo run -p termd-client --example termd_record -- \
//!   --out FILE.bin [--size 80x24] [--cwd DIR] [--settle-ms 400] [--max-ms 5000] \
//!   -- <command> [args...]
//! ```

use std::collections::BTreeMap;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use termd_client::{
    find_termd, ConnectOptions, Create, Event, Flow, Mode, Role, Subscribe, Target, Termd,
    MAX_SOCKET_PATH_BYTES,
};

const FORBIDDEN_NAMESPACES: [&str; 3] = ["wall", "agents", "default"];

struct Args {
    out: PathBuf,
    cols: u16,
    rows: u16,
    cwd: Option<PathBuf>,
    namespace: String,
    settle_ms: u64,
    max_ms: u64,
    command: Vec<String>,
}

fn usage_and_exit(msg: Option<&str>) -> ! {
    if let Some(err) = msg {
        eprintln!("termd_record: {err}");
    }
    eprintln!(
        "usage: termd_record --out <PATH> [--size COLSxROWS] [--cwd DIR] \
         [--ns SCRATCH_NS] [--settle-ms MS] [--max-ms MS] -- <cmd> [args...]"
    );
    std::process::exit(2);
}

fn parse_size(spec: &str) -> Result<(u16, u16), String> {
    let Some((c, r)) = spec.split_once(['x', 'X']) else {
        return Err(format!("expected COLSxROWS (e.g. 80x24), got {spec:?}"));
    };
    let cols: u16 = c
        .trim()
        .parse()
        .map_err(|_| format!("invalid cols in {spec:?}"))?;
    let rows: u16 = r
        .trim()
        .parse()
        .map_err(|_| format!("invalid rows in {spec:?}"))?;
    if cols == 0 || rows == 0 {
        return Err(format!("cols and rows must be >= 1, got {cols}x{rows}"));
    }
    Ok((cols, rows))
}

fn parse_args() -> Args {
    let mut raw = std::env::args().skip(1);
    let mut out: Option<PathBuf> = None;
    let mut cols: u16 = 80;
    let mut rows: u16 = 24;
    let mut cwd: Option<PathBuf> = None;
    let mut namespace = "rec".to_owned();
    let mut settle_ms: u64 = 400;
    let mut max_ms: u64 = 6000;
    let mut command: Vec<String> = Vec::new();

    while let Some(arg) = raw.next() {
        match arg.as_str() {
            "--out" => {
                let Some(val) = raw.next() else {
                    usage_and_exit(Some("--out requires a path"));
                };
                out = Some(PathBuf::from(val));
            }
            "--size" => {
                let Some(val) = raw.next() else {
                    usage_and_exit(Some("--size requires COLSxROWS"));
                };
                match parse_size(&val) {
                    Ok((c, r)) => {
                        cols = c;
                        rows = r;
                    }
                    Err(e) => usage_and_exit(Some(&e)),
                }
            }
            "--cwd" => {
                let Some(val) = raw.next() else {
                    usage_and_exit(Some("--cwd requires a directory"));
                };
                cwd = Some(PathBuf::from(val));
            }
            "--ns" => {
                let Some(val) = raw.next() else {
                    usage_and_exit(Some("--ns requires a scratch namespace"));
                };
                namespace = val;
            }
            "--settle-ms" => {
                let Some(val) = raw.next() else {
                    usage_and_exit(Some("--settle-ms requires milliseconds"));
                };
                settle_ms = val
                    .parse()
                    .unwrap_or_else(|_| usage_and_exit(Some("invalid --settle-ms")));
            }
            "--max-ms" => {
                let Some(val) = raw.next() else {
                    usage_and_exit(Some("--max-ms requires milliseconds"));
                };
                max_ms = val
                    .parse()
                    .unwrap_or_else(|_| usage_and_exit(Some("invalid --max-ms")));
            }
            "--help" | "-h" => usage_and_exit(None),
            "--" => {
                command.extend(raw);
                break;
            }
            other if other.starts_with('-') => {
                usage_and_exit(Some(&format!("unknown flag {other:?}")));
            }
            first_cmd => {
                command.push(first_cmd.to_owned());
                command.extend(raw);
                break;
            }
        }
    }

    if FORBIDDEN_NAMESPACES.contains(&namespace.as_str()) {
        usage_and_exit(Some(&format!(
            "refusing live namespace {namespace:?} (wall, agents, and default are off-limits)"
        )));
    }
    let Some(out) = out else {
        usage_and_exit(Some("missing required --out <PATH>"));
    };
    if command.is_empty() {
        usage_and_exit(Some("missing command to record after `--`"));
    }

    Args {
        out,
        cols,
        rows,
        cwd,
        namespace,
        settle_ms,
        max_ms,
        command,
    }
}

fn locate_termd_bin() -> Result<PathBuf, String> {
    // `$G3D_TERMD_BIN`, beside this binary, `~/.local/bin`, `~/.cargo/bin`, then PATH.
    find_termd()
}

struct ScratchDaemonGuard {
    dir: PathBuf,
    ns: String,
    bin: PathBuf,
}

impl ScratchDaemonGuard {
    fn create(ns: &str, bin: PathBuf) -> Result<Self, String> {
        if FORBIDDEN_NAMESPACES.contains(&ns) {
            return Err(format!("refusing forbidden namespace {ns:?}"));
        }
        let dir = PathBuf::from(format!("/tmp/g3d-rec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .map_err(|e| format!("create {}: {e}", dir.display()))?;

        let sock = dir.join(format!("termd-{ns}.sock"));
        if sock.as_os_str().len() > MAX_SOCKET_PATH_BYTES {
            return Err(format!(
                "scratch socket path {} exceeds {MAX_SOCKET_PATH_BYTES} bytes",
                sock.display()
            ));
        }
        // Single-threaded startup before Termd spawns its reader thread; ensures any
        // child or helper inspects the scratch directory, never the live user socket dir.
        std::env::set_var("G3D_TERMD_DIR", &dir);
        Ok(Self {
            dir,
            ns: ns.to_owned(),
            bin,
        })
    }

    fn target(&self) -> Target {
        Target::namespace_in(&self.dir, &self.ns)
    }

    fn shutdown(&self) {
        let opts = ConnectOptions {
            termd_bin: Some(self.bin.clone()),
            startup_timeout: Duration::from_secs(1),
            request_timeout: Duration::from_secs(3),
            reconnect_window: Duration::from_millis(100),
        };
        if let Ok(cleaner) = Termd::connect_with(&self.target(), "termd-record-stop", opts, Box::new(|| {})) {
            let _ = cleaner.stop();
        }
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

impl Drop for ScratchDaemonGuard {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn write_output_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
    }
    std::fs::write(path, bytes).map_err(|e| format!("write {}: {e}", path.display()))
}

fn run(args: Args) -> Result<(), String> {
    let bin = locate_termd_bin()?;
    let guard = ScratchDaemonGuard::create(&args.namespace, bin.clone())?;

    let connect_opts = ConnectOptions {
        termd_bin: Some(bin),
        startup_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(10),
        reconnect_window: Duration::from_secs(2),
    };
    let client = Termd::connect_or_start_with(
        &guard.target(),
        "termd-record",
        connect_opts,
        Box::new(|| {}),
    )
    .map_err(|e| format!("connect_or_start: {e}"))?;

    // Gate the command behind a silent `read` so the subscriber attaches and receives the
    // initial REPAINT before `exec "$@"` emits its first byte, with zero echoed characters.
    let mut gated_argv = vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        "stty -echo; IFS= read -r _; stty echo; exec \"$@\"".to_owned(),
        "termd-record-gate".to_owned(),
    ];
    gated_argv.extend(args.command.iter().cloned());

    let cwd_str = args
        .cwd
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
        });

    let mut env_base = BTreeMap::new();
    for key in ["HOME", "USER", "LOGNAME", "PATH", "LANG", "LC_ALL", "LC_CTYPE"] {
        if let Ok(val) = std::env::var(key) {
            env_base.insert(key.to_owned(), val);
        }
    }
    env_base
        .entry("LANG".to_owned())
        .or_insert_with(|| "en_US.UTF-8".to_owned());
    env_base.insert("TERM".to_owned(), "xterm-256color".to_owned());
    env_base.insert("COLORTERM".to_owned(), "truecolor".to_owned());
    env_base.insert("CLICOLOR".to_owned(), "1".to_owned());

    let term_id = "rec-1".to_owned();
    let created = client
        .create(&Create {
            id: term_id.clone(),
            argv: Some(gated_argv),
            cols: args.cols,
            rows: args.rows,
            cwd: cwd_str,
            env_base: Some(env_base),
            ..Create::default()
        })
        .map_err(|e| format!("create: {e}"))?;

    let sub = client
        .subscribe_with(&Subscribe {
            id: term_id.clone(),
            mode: Mode::Raw,
            role: Role::Own,
            history_rows: Some(0),
            flow: Some(Flow::Block),
            cols: Some(args.cols),
            rows: Some(args.rows),
        })
        .map_err(|e| format!("subscribe: {e}"))?;

    let mut recorded: Vec<u8> = Vec::new();
    let mut got_repaint = false;
    let repaint_deadline = Instant::now() + Duration::from_secs(3);
    while !got_repaint {
        let Some(rem) = repaint_deadline.checked_duration_since(Instant::now()) else {
            return Err("timed out waiting for initial REPAINT".to_owned());
        };
        match client.events().recv_timeout(rem) {
            Ok(Event::Repaint { handle, bytes, .. }) if handle == sub.handle => {
                recorded.extend_from_slice(&bytes);
                got_repaint = true;
            }
            Ok(Event::Output { handle, bytes, .. }) if handle == sub.handle => {
                recorded.extend_from_slice(&bytes);
            }
            Ok(_) => {}
            Err(e) => return Err(format!("waiting for REPAINT: {e}")),
        }
    }

    // Release the silent gate so `exec "$@"` begins on the live subscribed PTY.
    client
        .input(sub.handle, b"\n")
        .map_err(|e| format!("releasing gate: {e}"))?;

    let start = Instant::now();
    let max_dur = Duration::from_millis(args.max_ms);
    let settle_dur = Duration::from_millis(args.settle_ms);
    let mut saw_program_output = false;
    let mut exited = false;
    let mut last_output_at = Instant::now();

    loop {
        let elapsed = start.elapsed();
        if elapsed >= max_dur {
            break;
        }
        let until_max = max_dur - elapsed;
        let wait = if exited {
            // Drain any trailing buffered PTY bytes briefly after Exit before Closed.
            Duration::from_millis(80).min(until_max)
        } else if saw_program_output {
            match settle_dur.checked_sub(last_output_at.elapsed()) {
                Some(rem) => rem.min(until_max),
                None => break,
            }
        } else {
            until_max
        };

        match client.events().recv_timeout(wait) {
            Ok(Event::Output { handle, bytes, .. }) if handle == sub.handle => {
                recorded.extend_from_slice(&bytes);
                saw_program_output = true;
                last_output_at = Instant::now();
            }
            Ok(Event::Repaint { handle, bytes, .. }) if handle == sub.handle => {
                recorded.extend_from_slice(&bytes);
                last_output_at = Instant::now();
            }
            Ok(Event::Exit { handle, .. }) if handle == sub.handle => {
                exited = true;
            }
            Ok(Event::Closed { handle }) if handle == sub.handle => {
                break;
            }
            Ok(Event::Disconnected { message, .. }) => {
                eprintln!("termd_record: disconnected: {message}");
                break;
            }
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if exited || saw_program_output {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    let _ = client.kill(&created.id);
    let _ = client.stop();
    drop(client);
    drop(guard);

    write_output_file(&args.out, &recorded)?;
    eprintln!(
        "termd_record: wrote {} bytes ({}x{}) to {}",
        recorded.len(),
        args.cols,
        args.rows,
        args.out.display()
    );
    Ok(())
}

fn main() {
    let args = parse_args();
    if let Err(e) = run(args) {
        eprintln!("termd_record: {e}");
        std::process::exit(1);
    }
}
