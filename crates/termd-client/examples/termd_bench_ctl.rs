//! `termd_bench_ctl`: hermetic scratch-namespace controller for `tools/wall-perf.sh`.
//!
//! Starts `N` self-expiring terminals inside an isolated `/tmp` scratch `g3d-termd`
//! directory (`G3D_TERMD_DIR`, `--idle-exit 5`, `--env-source daemon`), records the
//! daemon (`holder` + `brain`) and terminal PIDs in `<dir>/pids.txt`, and stops +
//! confirms every daemon and child process is dead before removing `<dir>`.
//!
//! Safety rules:
//! - Refuses live/shared namespaces `wall`, `agents`, `default`, `rs-wall`, and `probe`.
//! - Refuses any `--dir` not located under `/tmp/`.
//! - Wraps every spawned terminal command in a wall-clock (`--ttl-secs`) + kernel
//!   `RLIMIT_CPU` process-group watchdog so floods self-terminate even under `kill -9`.
//! - Never invokes `pkill`; signals only explicit recorded PIDs and direct child PIDs
//!   read from `/proc/<pid>/task/*/children` (Linux) or `pgrep -P <pid>`.
//! - Never removes `<dir>` unless all recorded daemon and child PIDs are confirmed dead.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use termd_client::{
    find_termd, ConnectOptions, Create, Target, Termd, MAX_SOCKET_PATH_BYTES,
};

const FORBIDDEN_NAMESPACES: [&str; 5] = ["wall", "agents", "default", "rs-wall", "probe"];

const TTL_SUPERVISOR_PY: &str = "\
import os, resource, signal, subprocess, sys
ttl = int(sys.argv[1])
cpu_cap = max(ttl, 5)
resource.setrlimit(resource.RLIMIT_CPU, (cpu_cap, cpu_cap))
p = subprocess.Popen(sys.argv[2:], start_new_session=True)
def _kill_group(*_):
    try:
        os.killpg(p.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    try:
        p.wait(timeout=1)
    except Exception:
        pass
    sys.exit(0)
for sig in (signal.SIGTERM, signal.SIGHUP, signal.SIGINT):
    signal.signal(sig, _kill_group)
try:
    rc = p.wait(timeout=ttl)
    _kill_group()
    sys.exit(rc)
except subprocess.TimeoutExpired:
    _kill_group()
";

const TTL_SUPERVISOR_SH: &str = "\
ttl=\"$1\"; shift; cpu_cap=\"$1\"; shift; \
ulimit -t \"$cpu_cap\" 2>/dev/null || true; \
(sleep \"$ttl\"; kill -KILL -\"$$\" 2>/dev/null; kill -KILL \"$$\" 2>/dev/null) </dev/null >/dev/null 2>&1 & \
wd_pid=\"$!\"; \
\"$@\"; rc=$?; \
kill -KILL \"$wd_pid\" 2>/dev/null || true; \
exit $rc";

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("termd_bench_ctl: {msg}");
    std::process::exit(1);
}

fn find_in_path(bin_name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(bin_name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn locate_termd_bin() -> Result<PathBuf, String> {
    if let Some(explicit) = std::env::var_os("G3D_TERMD_BIN").filter(|s| !s.is_empty()) {
        let p = PathBuf::from(explicit);
        if p.is_file() {
            return Ok(p);
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let local = PathBuf::from(home).join(".local/bin/g3d-termd");
        if local.is_file() {
            return Ok(local);
        }
    }
    find_termd()
}

fn locate_python3_bin() -> Result<PathBuf, String> {
    for candidate in ["/usr/bin/python3", "/usr/local/bin/python3", "/opt/homebrew/bin/python3"] {
        let p = PathBuf::from(candidate);
        if p.is_file() {
            return Ok(p);
        }
    }
    find_in_path("python3").ok_or_else(|| "could not find python3 on PATH".to_owned())
}

fn validate_scratch(dir: &Path, ns: &str) -> Result<(), String> {
    if FORBIDDEN_NAMESPACES.contains(&ns) {
        return Err(format!(
            "refusing shared/live namespace {ns:?} (wall, agents, default, rs-wall, and probe are off-limits)"
        ));
    }
    if !dir.starts_with("/tmp/") || dir == Path::new("/tmp") {
        return Err(format!(
            "scratch directory {} must be a subdirectory under /tmp/",
            dir.display()
        ));
    }
    let sock = dir.join(format!("termd-{ns}.sock"));
    if sock.as_os_str().len() > MAX_SOCKET_PATH_BYTES {
        return Err(format!(
            "scratch socket path {} exceeds {MAX_SOCKET_PATH_BYTES} bytes",
            sock.display()
        ));
    }
    Ok(())
}

fn append_pid_record(pids_path: &Path, role: &str, pid: u32) -> Result<(), String> {
    if pid <= 1 {
        return Ok(());
    }
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(pids_path)
        .map_err(|e| format!("open {}: {e}", pids_path.display()))?;
    writeln!(f, "{role} {pid}").map_err(|e| format!("write {}: {e}", pids_path.display()))?;
    f.flush().map_err(|e| format!("flush {}: {e}", pids_path.display()))
}

#[cfg(unix)]
fn pid_is_alive(pid: u32) -> bool {
    if pid <= 1 || pid > i32::MAX as u32 {
        return false;
    }
    extern "C" {
        fn kill(pid: std::os::raw::c_int, sig: std::os::raw::c_int) -> std::os::raw::c_int;
    }
    // SAFETY: `kill(pid, 0)` sends no signal and probes process existence/permission in POSIX.
    let rc = unsafe { kill(pid as std::os::raw::c_int, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(1) // EPERM means still alive
}

#[cfg(unix)]
fn send_signal(pid: i32, sig: i32) {
    extern "C" {
        fn kill(pid: std::os::raw::c_int, sig: std::os::raw::c_int) -> std::os::raw::c_int;
    }
    // SAFETY: `pid` is validated (> 1 or < -1 for process groups) before calling `send_signal`.
    let _ = unsafe { kill(pid, sig) };
}

fn direct_children_of(pid: u32) -> Vec<u32> {
    if pid <= 1 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let task_dir = PathBuf::from(format!("/proc/{pid}/task"));
    if let Ok(entries) = std::fs::read_dir(&task_dir) {
        for entry in entries.flatten() {
            let children_path = entry.path().join("children");
            if let Ok(text) = std::fs::read_to_string(&children_path) {
                for tok in text.split_whitespace() {
                    if let Ok(cpid) = tok.parse::<u32>() {
                        if cpid > 1 {
                            out.push(cpid);
                        }
                    }
                }
            }
        }
        if !out.is_empty() {
            out.sort_unstable();
            out.dedup();
            return out;
        }
    }

    let pgrep_bin = if Path::new("/usr/bin/pgrep").is_file() {
        PathBuf::from("/usr/bin/pgrep")
    } else if let Some(p) = find_in_path("pgrep") {
        p
    } else {
        return out;
    };
    if let Ok(output) = Command::new(pgrep_bin)
        .args(["-P", &pid.to_string()])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    {
        if let Ok(text) = String::from_utf8(output.stdout) {
            for line in text.lines() {
                if let Ok(cpid) = line.trim().parse::<u32>() {
                    if cpid > 1 {
                        out.push(cpid);
                    }
                }
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

fn kill_children_of(pid: u32, sig_num: i32) {
    for cpid in direct_children_of(pid) {
        if cpid <= i32::MAX as u32 {
            send_signal(cpid as i32, sig_num);
        }
    }
}

#[cfg(unix)]
fn raise_nofile_soft_limit(requested: u64) -> Result<(), String> {
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Rlimit {
        rlim_cur: u64,
        rlim_max: u64,
    }
    #[cfg(target_os = "macos")]
    const RLIMIT_NOFILE: std::os::raw::c_int = 8;
    #[cfg(target_os = "linux")]
    const RLIMIT_NOFILE: std::os::raw::c_int = 7;

    extern "C" {
        fn getrlimit(resource: std::os::raw::c_int, rlp: *mut Rlimit) -> std::os::raw::c_int;
        fn setrlimit(resource: std::os::raw::c_int, rlp: *const Rlimit) -> std::os::raw::c_int;
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let mut rl = Rlimit { rlim_cur: 0, rlim_max: 0 };
        // SAFETY: `RLIMIT_NOFILE` is valid on macOS/Linux and `rl` is an aligned local struct.
        let rc = unsafe { getrlimit(RLIMIT_NOFILE, &mut rl) };
        if rc != 0 {
            return Err(format!("getrlimit(RLIMIT_NOFILE): {}", std::io::Error::last_os_error()));
        }
        // On macOS, setting `rlim_cur` above `OPEN_MAX` (10240) without special sysctl can fail
        // with EINVAL unless capped gently when `rlim_max` is RLIM_INFINITY.
        let cap = if cfg!(target_os = "macos") {
            requested.min(10_240).min(rl.rlim_max)
        } else {
            requested.min(rl.rlim_max)
        };
        if cap > rl.rlim_cur {
            rl.rlim_cur = cap;
            // SAFETY: `rl` holds a valid `rlim_cur <= rlim_max`.
            let rc2 = unsafe { setrlimit(RLIMIT_NOFILE, &rl) };
            if rc2 != 0 {
                return Err(format!(
                    "setrlimit(RLIMIT_NOFILE, {cap}): {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
    }
    Ok(())
}

fn read_recorded_pids(pids_path: &Path) -> Vec<(String, u32)> {
    let Ok(text) = std::fs::read_to_string(pids_path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        if let (Some(role), Some(pid_s)) = (parts.next(), parts.next()) {
            if let Ok(pid) = pid_s.parse::<u32>() {
                if pid > 1 {
                    out.push((role.to_owned(), pid));
                }
            }
        }
    }
    out
}

fn cmd_start(mut args: impl Iterator<Item = String>) -> Result<(), String> {
    let mut dir: Option<PathBuf> = None;
    let mut ns = "wall-perf".to_owned();
    let mut count: usize = 1;
    let mut cols: u16 = 80;
    let mut rows: u16 = 24;
    let mut warmup_ms: u64 = 150;
    let mut ttl_secs: u64 = 15;
    let mut shell_ttl = false;
    let mut allow_limit = false;
    let mut nofile_opt: Option<u64> = None;
    let mut command: Vec<String> = Vec::new();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dir" => dir = Some(PathBuf::from(args.next().ok_or("--dir requires path")?)),
            "--ns" => ns = args.next().ok_or("--ns requires name")?,
            "--count" => {
                count = args
                    .next()
                    .ok_or("--count requires N")?
                    .parse()
                    .map_err(|_| "invalid --count")?;
            }
            "--cols" => {
                cols = args
                    .next()
                    .ok_or("--cols requires u16")?
                    .parse()
                    .map_err(|_| "invalid --cols")?;
            }
            "--rows" => {
                rows = args
                    .next()
                    .ok_or("--rows requires u16")?
                    .parse()
                    .map_err(|_| "invalid --rows")?;
            }
            "--warmup-ms" => {
                warmup_ms = args
                    .next()
                    .ok_or("--warmup-ms requires ms")?
                    .parse()
                    .map_err(|_| "invalid --warmup-ms")?;
            }
            "--ttl-secs" => {
                ttl_secs = args
                    .next()
                    .ok_or("--ttl-secs requires seconds")?
                    .parse()
                    .map_err(|_| "invalid --ttl-secs")?;
            }
            "--shell-ttl" => shell_ttl = true,
            "--allow-limit" => allow_limit = true,
            "--nofile" => {
                nofile_opt = Some(
                    args.next()
                        .ok_or("--nofile requires u64")?
                        .parse()
                        .map_err(|_| "invalid --nofile")?,
                );
            }
            "--" => {
                command.extend(args);
                break;
            }
            other => return Err(format!("unknown flag {other:?}")),
        }
    }

    let dir = dir.ok_or("missing --dir /tmp/...")?;
    if count == 0 {
        return Err("--count must be >= 1".to_owned());
    }
    if ttl_secs == 0 {
        return Err("--ttl-secs must be >= 1".to_owned());
    }
    if command.is_empty() {
        return Err("missing terminal command after `--`".to_owned());
    }
    validate_scratch(&dir, &ns)?;

    if let Some(nofile) = nofile_opt {
        raise_nofile_soft_limit(nofile)?;
    }

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(|e| format!("create {}: {e}", dir.display()))?;

    std::env::set_var("G3D_TERMD_DIR", &dir);
    std::env::set_var("G3D_TERMD_ENV_SOURCE", "daemon");

    let bin = locate_termd_bin()?;
    let sock = dir.join(format!("termd-{ns}.sock"));
    if !sock.exists() {
        let status = Command::new(&bin)
            .args([
                "--ns",
                &ns,
                "--daemon",
                "--idle-exit",
                "5",
                "--env-source",
                "daemon",
            ])
            .env("G3D_TERMD_DIR", &dir)
            .env("G3D_TERMD_ENV_SOURCE", "daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| format!("spawn {}: {e}", bin.display()))?;
        if !status.success() {
            return Err(format!("{} exited with {status}", bin.display()));
        }
    }

    let opts = ConnectOptions {
        termd_bin: Some(bin),
        startup_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(15),
        reconnect_window: Duration::from_secs(2),
    };
    let target = Target::namespace_in(&dir, &ns);
    let client = Termd::connect_or_start_with(&target, "termd-bench-ctl", opts, Box::new(|| {}))
        .map_err(|e| format!("connect_or_start: {e}"))?;

    let pids_path = dir.join("pids.txt");
    let welcome = client.handle().welcome();
    if let Some(holder) = &welcome.holder {
        append_pid_record(&pids_path, "holder", holder.pid)?;
    }
    if let Some(brain_pid) = welcome.pid {
        append_pid_record(&pids_path, "brain", brain_pid)?;
    }

    let supervised_argv = if shell_ttl {
        let cpu_cap = ttl_secs.max(5);
        let mut v = vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            TTL_SUPERVISOR_SH.to_owned(),
            "termd-ttl".to_owned(),
            ttl_secs.to_string(),
            cpu_cap.to_string(),
        ];
        v.extend(command);
        v
    } else {
        let py = locate_python3_bin()?;
        let mut v = vec![
            py.to_string_lossy().into_owned(),
            "-c".to_owned(),
            TTL_SUPERVISOR_PY.to_owned(),
            ttl_secs.to_string(),
        ];
        v.extend(command);
        v
    };

    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
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

    let mut created_count = 0usize;
    let mut limit_err: Option<String> = None;
    for idx in 1..=count {
        let id = format!("t{idx:02}");
        match client.create(&Create {
            id,
            argv: Some(supervised_argv.clone()),
            cols,
            rows,
            cwd: cwd.clone(),
            env_base: Some(env_base.clone()),
            ..Create::default()
        }) {
            Ok(created) => {
                created_count += 1;
                append_pid_record(&pids_path, "term", created.pid)?;
            }
            Err(e) => {
                if allow_limit {
                    let clean = e
                        .to_string()
                        .replace(['\r', '\n', '\t'], " ")
                        .trim()
                        .to_owned();
                    limit_err = Some(clean);
                    break;
                }
                return Err(format!("create t{idx:02}: {e}"));
            }
        }
    }

    if warmup_ms > 0 && created_count > 0 {
        std::thread::sleep(Duration::from_millis(warmup_ms));
    }
    if allow_limit {
        match limit_err {
            Some(err) => println!(
                "termd_bench_ctl: status created={created_count} requested={count} refused=1 error={err}"
            ),
            None => println!(
                "termd_bench_ctl: status created={created_count} requested={count} refused=0 error=none"
            ),
        }
    }
    Ok(())
}

fn cmd_stop(mut args: impl Iterator<Item = String>) -> Result<(), String> {
    let mut dir: Option<PathBuf> = None;
    let mut ns = "wall-perf".to_owned();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dir" => dir = Some(PathBuf::from(args.next().ok_or("--dir requires path")?)),
            "--ns" => ns = args.next().ok_or("--ns requires name")?,
            other => return Err(format!("unknown flag {other:?}")),
        }
    }

    let dir = dir.ok_or("missing --dir /tmp/...")?;
    validate_scratch(&dir, &ns)?;
    if !dir.exists() {
        return Ok(());
    }

    let pids_path = dir.join("pids.txt");
    let mut tracked = read_recorded_pids(&pids_path);

    let sock = dir.join(format!("termd-{ns}.sock"));
    if sock.exists() {
        let bin = locate_termd_bin().ok();
        let opts = ConnectOptions {
            termd_bin: bin,
            startup_timeout: Duration::from_secs(1),
            request_timeout: Duration::from_secs(2),
            reconnect_window: Duration::from_millis(100),
        };
        let target = Target::namespace_in(&dir, &ns);
        if let Ok(client) = Termd::connect_with(&target, "termd-bench-stop", opts, Box::new(|| {})) {
            let welcome = client.handle().welcome();
            if let Some(h) = &welcome.holder {
                tracked.push(("holder".to_owned(), h.pid));
            }
            if let Some(b) = welcome.pid {
                tracked.push(("brain".to_owned(), b));
            }
            if let Ok(terms) = client.list() {
                for t in terms {
                    if t.pid > 1 {
                        tracked.push(("term".to_owned(), t.pid));
                    }
                }
            }
            let _ = client.stop();
        }
    }

    tracked.sort_by_key(|(_, pid)| *pid);
    tracked.dedup_by_key(|(_, pid)| *pid);

    // Wait briefly for graceful `stop()` to tear down the daemon and PTY sessions.
    let graceful_deadline = Instant::now() + Duration::from_millis(400);
    while Instant::now() < graceful_deadline {
        if tracked.iter().all(|(_, pid)| !pid_is_alive(*pid)) && !sock.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    // SIGTERM then SIGKILL any surviving terminal groups, child processes, and daemons.
    const SIGTERM_NUM: i32 = 15;
    const SIGKILL_NUM: i32 = 9;
    for (sig_num, wait_ms) in [(SIGTERM_NUM, 250u64), (SIGKILL_NUM, 1500u64)] {
        if tracked.iter().all(|(_, pid)| !pid_is_alive(*pid)) {
            break;
        }
        for (role, pid) in &tracked {
            if !pid_is_alive(*pid) {
                continue;
            }
            kill_children_of(*pid, sig_num);
            if role == "term" && *pid <= i32::MAX as u32 {
                send_signal(-(*pid as i32), sig_num);
            }
            if *pid <= i32::MAX as u32 {
                send_signal(*pid as i32, sig_num);
            }
        }
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        while Instant::now() < deadline {
            if tracked.iter().all(|(_, pid)| !pid_is_alive(*pid)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    let survivors: Vec<(String, u32)> = tracked
        .into_iter()
        .filter(|(_, pid)| pid_is_alive(*pid))
        .collect();
    if !survivors.is_empty() {
        return Err(format!(
            "refusing to remove {}: processes still alive: {:?}",
            dir.display(),
            survivors
        ));
    }

    std::fs::remove_dir_all(&dir).map_err(|e| format!("remove {}: {e}", dir.display()))
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(sub) = args.next() else {
        die("usage: termd_bench_ctl <start|stop> --dir /tmp/... --ns <scratch-ns> [...]");
    };
    let res = match sub.as_str() {
        "start" => cmd_start(args),
        "stop" => cmd_stop(args),
        other => Err(format!("unknown subcommand {other:?} (expected start or stop)")),
    };
    if let Err(e) = res {
        die(e);
    }
}
