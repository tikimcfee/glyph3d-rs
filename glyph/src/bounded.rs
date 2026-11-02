//! Every process a gate runs, bounded (2026-11-01): past its limit
//! (build.toml `[settings] process_timeout_secs`) it is killed with every
//! process under it, and what it printed until then comes back with how it
//! ended.
//!
//! Until then a gate waited on its processes with no limit, and a GPU test
//! that stopped making progress held the whole battery: on 2026-11-01, on
//! monolith1, the renderer's unit-test binary hung four times in half an
//! hour, one run reaching 353 s before someone killed it by hand.
//!
//! On a timeout, before anything is killed, the runner records what it can
//! of where the processes were: on Linux, each one's threads (name, state,
//! CPU time, the kernel function a sleeping thread waits in), and then a
//! stack of every thread of each process at the bottom of the tree (the
//! test binary, the renderer), from a core dump: it is sent SIGABRT, and
//! systemd-coredump writes the stacks `coredumpctl info` prints. gdb cannot
//! attach there (`kernel.yama.ptrace_scope` = 1), and a core needs no
//! attach. Elsewhere the tree is only killed.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// How one bounded process ended.
pub(crate) struct Outcome {
    /// It exited, and with success.
    pub(crate) ok: bool,
    /// Its stdout then its stderr, as far as they got.
    pub(crate) out: String,
    /// Past its limit: killed, with what was recorded of it first
    /// (`stacks`).
    pub(crate) timed_out: Option<String>,
    pub(crate) secs: f64,
}

/// How long a process aborted for its stack has to write its core before
/// the tree is killed regardless.
const ABORT_GRACE: Duration = Duration::from_secs(10);
/// How long the dump's stacks have to reach `coredumpctl`.
const DUMP_WAIT: Duration = Duration::from_secs(20);
/// How long the output pipes have to close once the process has gone: a
/// process that left the tree may hold them, and is not waited on.
const PIPE_GRACE: Duration = Duration::from_secs(5);

/// `cmd`, bounded by `limit`. `stacks`: on a timeout, record the tree's
/// threads and abort its bottom processes for their stacks before the kill
/// (the gates); without it the tree is only killed (the unit tests, which
/// would otherwise leave a core dump per run).
pub(crate) fn run_within(cmd: &mut Command, limit: Duration, stacks: bool) -> Outcome {
    let start = Instant::now();
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Outcome {
                ok: false,
                out: format!("could not spawn {:?}: {e}", cmd.get_program()),
                timed_out: None,
                secs: 0.0,
            }
        }
    };
    let pid = child.id();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let pipes: Vec<Arc<Mutex<Vec<u8>>>> = [
        child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
        child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    .map(|mut pipe| {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let (into, done) = (Arc::clone(&buf), done_tx.clone());
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            while let Ok(n) = pipe.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                into.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(&chunk[..n]);
            }
            let _ = done.send(());
        });
        buf
    })
    .collect();
    drop(done_tx);
    let (status_tx, status_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = status_tx.send(child.wait());
    });
    let (status, timed_out) = match status_rx.recv_timeout(limit) {
        Ok(s) => (s.ok(), None),
        Err(_) => {
            let record = end_tree(pid, stacks);
            (status_rx.recv_timeout(ABORT_GRACE).ok().and_then(Result::ok), Some(record))
        }
    };
    let pipe_deadline = Instant::now() + PIPE_GRACE;
    for _ in 0..pipes.len() {
        if done_rx.recv_timeout(pipe_deadline.saturating_duration_since(Instant::now())).is_err() {
            break;
        }
    }
    let mut out = String::new();
    for p in &pipes {
        out.push_str(&String::from_utf8_lossy(&p.lock().unwrap_or_else(|e| e.into_inner())));
    }
    Outcome {
        ok: timed_out.is_none() && status.is_some_and(|s| s.success()),
        out,
        timed_out,
        secs: start.elapsed().as_secs_f64(),
    }
}

/// `cmd` under `bash -c` in `cwd`, bounded: `paths::sh` with a limit.
pub(crate) fn sh_within(cmd: &str, cwd: &Path, limit: Duration) -> Outcome {
    let mut c = Command::new("bash");
    c.arg("-c").arg(cmd).current_dir(cwd).env("GLYPH_RENDERER", crate::paths::renderer());
    run_within(&mut c, limit, true)
}

/// Every live process under `root` (not `root`), from one `ps` snapshot.
fn descendants(root: u32) -> Vec<u32> {
    let Ok(o) = Command::new("ps").args(["-A", "-o", "pid=,ppid="]).output() else { return Vec::new() };
    let pairs: Vec<(u32, u32)> = String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace().map(str::parse::<u32>);
            Some((w.next()?.ok()?, w.next()?.ok()?))
        })
        .collect();
    let mut found = vec![root];
    let mut i = 0;
    while i < found.len() {
        let parent = found[i];
        found.extend(pairs.iter().filter(|&&(_, pp)| pp == parent).map(|&(p, _)| p));
        i += 1;
    }
    found.remove(0);
    found
}

fn signal(pid: u32, sig: i32) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // SAFETY: kill(2) with a pid this runner spawned or found under it;
        // a pid already gone is ESRCH, which is ignored.
        unsafe {
            libc::kill(pid, sig);
        }
    }
}

/// Kill `root` and every process under it; with `stacks`, first record
/// their threads and abort the bottom ones for their stacks. Returns what
/// was recorded.
fn end_tree(root: u32, stacks: bool) -> String {
    let mut tree = vec![root];
    tree.extend(descendants(root));
    let mut record = String::new();
    if stacks {
        record.push_str(&threads(&tree));
        let parents: Vec<u32> = tree.iter().map(|&p| parent_of(p)).collect();
        let bottom: Vec<u32> = tree.iter().copied().filter(|p| !parents.contains(p)).collect();
        if cfg!(target_os = "linux") {
            for &p in &bottom {
                signal(p, libc::SIGABRT);
            }
            let deadline = Instant::now() + ABORT_GRACE;
            while bottom.iter().any(|&p| alive(p)) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        for &p in &tree {
            signal(p, libc::SIGKILL);
        }
        if cfg!(target_os = "linux") {
            for &p in &bottom {
                record.push_str(&dumped_stacks(p));
            }
        }
    } else {
        for &p in &tree {
            signal(p, libc::SIGKILL);
        }
    }
    record
}

/// A process's parent from its `/proc` stat (Linux), or 0.
fn parent_of(pid: u32) -> u32 {
    stat_fields(&format!("/proc/{pid}/stat")).and_then(|f| f.get(1).and_then(|s| s.parse().ok())).unwrap_or(0)
}

/// The fields after a `/proc` stat line's `(comm)`: state first.
fn stat_fields(path: &str) -> Option<Vec<String>> {
    let s = std::fs::read_to_string(path).ok()?;
    let rest = &s[s.rfind(')')? + 1..];
    Some(rest.split_whitespace().map(str::to_string).collect())
}

/// Whether `pid` is still running (not gone, not a zombie) on Linux.
fn alive(pid: u32) -> bool {
    stat_fields(&format!("/proc/{pid}/stat")).is_some_and(|f| f.first().is_some_and(|s| s != "Z"))
}

/// Each process's command line and threads: name, state, CPU seconds, and
/// the kernel function a sleeping thread waits in (Linux; empty elsewhere).
fn threads(tree: &[u32]) -> String {
    let mut s = String::new();
    if !cfg!(target_os = "linux") {
        return s;
    }
    let ticks = 100.0; // USER_HZ, 100 on every Linux this runs on
    for &p in tree {
        let cmdline = std::fs::read(format!("/proc/{p}/cmdline")).unwrap_or_default();
        let cmdline = String::from_utf8_lossy(&cmdline).replace('\0', " ");
        s.push_str(&format!("process {p}: {}\n", cmdline.trim()));
        let Ok(tasks) = std::fs::read_dir(format!("/proc/{p}/task")) else { continue };
        let mut rows: Vec<(f64, String)> = tasks
            .flatten()
            .filter_map(|t| {
                let dir = t.path();
                let comm = std::fs::read_to_string(dir.join("comm")).unwrap_or_default();
                let f = stat_fields(&dir.join("stat").to_string_lossy())?;
                let cpu = (f.get(11)?.parse::<f64>().ok()? + f.get(12)?.parse::<f64>().ok()?) / ticks;
                let wchan = std::fs::read_to_string(dir.join("wchan")).unwrap_or_default();
                let tid = t.file_name().to_string_lossy().into_owned();
                Some((cpu, format!("  thread {tid} {:<16} state {} cpu {cpu:.1} s wchan {}\n", comm.trim(), f[0], wchan.trim())))
            })
            .collect();
        rows.sort_by(|a, b| b.0.total_cmp(&a.0));
        for (_, row) in rows {
            s.push_str(&row);
        }
    }
    s
}

/// The thread stacks systemd-coredump recorded for `pid`'s core, waiting
/// for them to arrive (`DUMP_WAIT`), or why there are none.
fn dumped_stacks(pid: u32) -> String {
    let deadline = Instant::now() + DUMP_WAIT;
    loop {
        let out = Command::new("coredumpctl").args(["info", "--no-pager", &pid.to_string()]).output();
        match out {
            Ok(o) if String::from_utf8_lossy(&o.stdout).contains("Stack trace of thread") => {
                return format!("coredumpctl info {pid}:\n{}\n", String::from_utf8_lossy(&o.stdout));
            }
            Err(e) => return format!("no stacks for {pid}: coredumpctl did not run: {e}\n"),
            Ok(o) if Instant::now() >= deadline => {
                return format!(
                    "no stacks for {pid} after {} s: {}\n",
                    DUMP_WAIT.as_secs(),
                    String::from_utf8_lossy(&o.stderr).trim()
                )
            }
            Ok(_) => std::thread::sleep(Duration::from_millis(500)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether `pid` exists at all (zombies included): `kill(pid, 0)`.
    fn exists(pid: u32) -> bool {
        // SAFETY: signal 0 checks existence and sends nothing.
        unsafe { libc::kill(libc::pid_t::try_from(pid).unwrap(), 0) == 0 }
    }

    /// A process past its limit is killed with the process it started,
    /// within about the limit, and what it printed until then comes back
    /// with the timeout. Before 2026-11-01 nothing bounded a gate's
    /// processes: this one would have run its 30 s.
    #[test]
    fn a_process_past_its_limit_is_killed_with_everything_under_it() {
        let mut c = Command::new("bash");
        c.arg("-c").arg("sleep 30 & echo \"child $!\"; wait");
        let o = run_within(&mut c, Duration::from_millis(500), false);
        assert!(o.timed_out.is_some() && !o.ok, "a 30 s process under a 0.5 s limit must time out: {}", o.out);
        assert!(o.secs < 10.0, "killed at its limit, not when it ended: {} s", o.secs);
        let grandchild: u32 = o
            .out
            .lines()
            .find_map(|l| l.strip_prefix("child "))
            .and_then(|p| p.trim().parse().ok())
            .unwrap_or_else(|| panic!("its output until the kill comes back: {:?}", o.out));
        let gone = Instant::now() + Duration::from_secs(5);
        while exists(grandchild) && Instant::now() < gone {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!exists(grandchild), "the process it started ({grandchild}) must be killed too");
    }

    /// A process inside its limit is waited on and judged as before: its
    /// exit status, stdout then stderr, no timeout.
    #[test]
    fn a_process_inside_its_limit_ends_as_it_would_have() {
        let mut c = Command::new("bash");
        c.arg("-c").arg("echo out; echo err >&2; exit 3");
        let o = run_within(&mut c, Duration::from_secs(30), false);
        assert!(o.timed_out.is_none() && !o.ok, "it exited 3 on its own");
        assert_eq!(o.out, "out\nerr\n");
        let mut c = Command::new("true");
        assert!(run_within(&mut c, Duration::from_secs(30), false).ok);
    }
}
