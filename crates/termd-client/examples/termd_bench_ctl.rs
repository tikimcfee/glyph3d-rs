//! `termd_bench_ctl`: hermetic scratch-namespace controller for `tools/wall-perf.sh`.
//!
//! Starts `N` terminals running a given command inside an isolated `/tmp` scratch
//! `g3d-termd` directory (`G3D_TERMD_DIR`, `G3D_TERMD_ENV_SOURCE=daemon`) and stops
//! + removes that scratch daemon cleanly afterward.
//!
//! Safety rules:
//! - Refuses live namespaces `wall`, `agents`, and `default`.
//! - Refuses any `--dir` not located under `/tmp/`.

use std::collections::BTreeMap;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use termd_client::{
    find_termd, ConnectOptions, Create, Target, Termd, MAX_SOCKET_PATH_BYTES,
};

const FORBIDDEN_NAMESPACES: [&str; 3] = ["wall", "agents", "default"];

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("termd_bench_ctl: {msg}");
    std::process::exit(1);
}

fn locate_termd_bin() -> Result<PathBuf, String> {
    let local = PathBuf::from("/Users/lugo/.local/bin/g3d-termd");
    if local.is_file() {
        return Ok(local);
    }
    find_termd()
}

fn validate_scratch(dir: &Path, ns: &str) -> Result<(), String> {
    if FORBIDDEN_NAMESPACES.contains(&ns) {
        return Err(format!(
            "refusing live namespace {ns:?} (wall, agents, and default are off-limits)"
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

fn cmd_start(mut args: impl Iterator<Item = String>) -> Result<(), String> {
    let mut dir: Option<PathBuf> = None;
    let mut ns = "wall-perf".to_owned();
    let mut count: usize = 1;
    let mut cols: u16 = 100;
    let mut rows: u16 = 30;
    let mut warmup_ms: u64 = 150;
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
    if command.is_empty() {
        return Err("missing terminal command after `--`".to_owned());
    }
    validate_scratch(&dir, &ns)?;

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(|e| format!("create {}: {e}", dir.display()))?;

    std::env::set_var("G3D_TERMD_DIR", &dir);
    std::env::set_var("G3D_TERMD_ENV_SOURCE", "daemon");

    let bin = locate_termd_bin()?;
    let opts = ConnectOptions {
        termd_bin: Some(bin),
        startup_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(10),
        reconnect_window: Duration::from_secs(2),
    };
    let target = Target::namespace_in(&dir, &ns);
    let client = Termd::connect_or_start_with(&target, "termd-bench-ctl", opts, Box::new(|| {}))
        .map_err(|e| format!("connect_or_start: {e}"))?;

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

    for idx in 1..=count {
        let id = format!("t{idx:02}");
        client
            .create(&Create {
                id,
                argv: Some(command.clone()),
                cols,
                rows,
                cwd: cwd.clone(),
                env_base: Some(env_base.clone()),
                ..Create::default()
            })
            .map_err(|e| format!("create t{idx:02}: {e}"))?;
    }

    if warmup_ms > 0 {
        std::thread::sleep(Duration::from_millis(warmup_ms));
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

    let sock = dir.join(format!("termd-{ns}.sock"));
    let lock = PathBuf::from(format!("{}.lock", sock.display()));
    if sock.exists() {
        let bin = locate_termd_bin().ok();
        let opts = ConnectOptions {
            termd_bin: bin,
            startup_timeout: Duration::from_secs(1),
            request_timeout: Duration::from_secs(3),
            reconnect_window: Duration::from_millis(100),
        };
        let target = Target::namespace_in(&dir, &ns);
        if let Ok(client) = Termd::connect_with(&target, "termd-bench-stop", opts, Box::new(|| {})) {
            let _ = client.stop();
        }
        let deadline = Instant::now() + Duration::from_millis(1500);
        while sock.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    if lock.exists() {
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
    if dir.exists() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    Ok(())
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
