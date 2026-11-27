//! CLI door onto a running native wall (`wallctl [--ns NAME] <verb and args...>`),
//! plus offline browser-wall store import when no window is open on `--ns NAME`.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use wall_control::{format_command_line, Client, Error as ControlError, Target, DEFAULT_NAMESPACE};

const USAGE: &str = "\
usage: wallctl [--ns NAME | --socket PATH] [<verb> [args...]]

Send command verbs to a running native wall over its local Unix control socket.
  --ns NAME      Target namespace (default: rs-wall -> wall-rs-wall.sock)
  --socket PATH  Explicit Unix socket path
  -h, --help     Print this help

With <verb> [args...], sends one command, prints the reply, and exits 0 (or 1 on ERR:).
With no <verb>, reads command lines from stdin over one connection until EOF.

Browser-wall store import (runs live on an open window, or writes the namespace store
offline when no window is open on --ns NAME):
  wallctl [--ns NAME] wall.import <browser-export.json> [browser-scope]";

struct ParsedCli {
    target: Target,
    command_argv: Vec<String>,
    show_help: bool,
}

fn parse_cli<I>(args: I) -> Result<ParsedCli, String>
where
    I: IntoIterator<Item = String>,
{
    let mut iter = args.into_iter();
    let mut ns: Option<String> = None;
    let mut socket: Option<String> = None;
    let mut command_argv: Vec<String> = Vec::new();

    while let Some(arg) = iter.next() {
        if !command_argv.is_empty() {
            // Once `<verb>` is seen, all remaining arguments belong to that verb
            // (so negative numbers like `wall.move g-1 -10 -20` are never parsed as flags).
            command_argv.push(arg);
            continue;
        }
        match arg.as_str() {
            "-h" | "--help" => {
                return Ok(ParsedCli {
                    target: Target::default(),
                    command_argv: Vec::new(),
                    show_help: true,
                });
            }
            "--" => {
                command_argv.extend(iter);
                break;
            }
            "--ns" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--ns requires a namespace name".to_string())?;
                ns = Some(val);
            }
            "--socket" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--socket requires a path".to_string())?;
                socket = Some(val);
            }
            _ if arg.starts_with("--ns=") => {
                ns = Some(arg["--ns=".len()..].to_string());
            }
            _ if arg.starts_with("--socket=") => {
                socket = Some(arg["--socket=".len()..].to_string());
            }
            _ if arg.starts_with('-') => {
                return Err(format!("unknown option '{arg}' (try --help)"));
            }
            _ => {
                command_argv.push(arg);
            }
        }
    }

    if ns.is_some() && socket.is_some() {
        return Err("pass either --ns or --socket, not both".to_string());
    }
    let target = if let Some(path) = socket {
        Target::socket(path)
    } else {
        Target::namespace(ns.unwrap_or_else(|| DEFAULT_NAMESPACE.to_string()))
    };

    Ok(ParsedCli {
        target,
        command_argv,
        show_help: false,
    })
}

fn resolve_for_peer_cwd(raw: &str) -> PathBuf {
    let p = Path::new(raw);
    if p.is_absolute() {
        return p.to_path_buf();
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(p),
        Err(_) => p.to_path_buf(),
    }
}

fn prepare_argv(argv: &[String]) -> Result<Vec<String>, String> {
    if argv.first().map(String::as_str) == Some("wall.import") {
        if !(2..=3).contains(&argv.len()) {
            return Err("usage: wall.import <browser-export.json> [browser-scope]".to_string());
        }
        let mut out = argv.to_vec();
        out[1] = resolve_for_peer_cwd(&argv[1]).to_string_lossy().into_owned();
        return Ok(out);
    }
    Ok(argv.to_vec())
}

fn is_not_listening(err: &ControlError) -> bool {
    matches!(
        err,
        ControlError::Io(io_err)
            if matches!(
                io_err.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            )
    )
}

fn run_offline_import(
    target: &Target,
    argv: &[String],
    default_dir: &Path,
) -> Result<String, String> {
    let (store_dir, ns) = match target {
        Target::Namespace(ns) => (default_dir, ns.as_str()),
        Target::NamespaceIn { dir, ns } => (dir.as_path(), ns.as_str()),
        Target::Socket(p) => {
            return Err(format!(
                "no wall window is listening on {}, and offline wall.import requires --ns <namespace>",
                p.display()
            ));
        }
    };
    let export_path = Path::new(&argv[1]);
    let source_scope = argv.get(2).map(String::as_str);
    let report = wall_core::import::import_browser_file(export_path, source_scope, store_dir, ns)?;
    Ok(report.summary_text())
}

fn print_reply(stdout: &mut impl Write, text: &str) -> io::Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    stdout.write_all(text.as_bytes())?;
    if !text.ends_with('\n') {
        stdout.write_all(b"\n")?;
    }
    stdout.flush()
}

fn main() -> ExitCode {
    let cli = match parse_cli(std::env::args().skip(1)) {
        Ok(cli) => cli,
        Err(msg) => {
            let _ = writeln!(io::stderr(), "ERR: {msg}");
            return ExitCode::FAILURE;
        }
    };

    if cli.show_help {
        let _ = writeln!(io::stdout(), "{USAGE}");
        return ExitCode::SUCCESS;
    }

    let command_argv = match prepare_argv(&cli.command_argv) {
        Ok(argv) => argv,
        Err(msg) => {
            let _ = writeln!(io::stderr(), "ERR: {msg}");
            return ExitCode::FAILURE;
        }
    };

    let mut stdout = io::stdout().lock();

    let mut client = match Client::connect(&cli.target) {
        Ok(client) => client,
        Err(ref err)
            if is_not_listening(err)
                && command_argv.first().map(String::as_str) == Some("wall.import") =>
        {
            let store_dir = wall_core::store::default_store_dir();
            return match run_offline_import(&cli.target, &command_argv, &store_dir) {
                Ok(summary) => {
                    if let Err(err) = print_reply(&mut stdout, &summary) {
                        let _ = writeln!(io::stderr(), "ERR: {err}");
                        return ExitCode::FAILURE;
                    }
                    ExitCode::SUCCESS
                }
                Err(msg) => {
                    let _ = writeln!(io::stderr(), "ERR: {msg}");
                    ExitCode::FAILURE
                }
            };
        }
        Err(err) => {
            let _ = writeln!(io::stderr(), "ERR: {err}");
            return ExitCode::FAILURE;
        }
    };

    if !command_argv.is_empty() {
        let line = format_command_line(&command_argv);
        return match client.send(&line) {
            Ok(reply) => {
                if let Err(err) = print_reply(&mut stdout, &reply.text) {
                    let _ = writeln!(io::stderr(), "ERR: {err}");
                    return ExitCode::FAILURE;
                }
                if reply.is_err() {
                    ExitCode::FAILURE
                } else {
                    ExitCode::SUCCESS
                }
            }
            Err(err) => {
                let _ = writeln!(io::stderr(), "ERR: {err}");
                ExitCode::FAILURE
            }
        };
    }

    let stdin = io::stdin();
    let mut any_err = false;
    for raw_line in stdin.lock().lines() {
        let line = match raw_line {
            Ok(l) => l,
            Err(err) => {
                let _ = writeln!(io::stderr(), "ERR: {err}");
                return ExitCode::FAILURE;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        match client.send(&line) {
            Ok(reply) => {
                if let Err(err) = print_reply(&mut stdout, &reply.text) {
                    let _ = writeln!(io::stderr(), "ERR: {err}");
                    return ExitCode::FAILURE;
                }
                if reply.is_err() {
                    any_err = true;
                }
            }
            Err(err) => {
                let _ = writeln!(io::stderr(), "ERR: {err}");
                return ExitCode::FAILURE;
            }
        }
    }

    if any_err {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use wall_core::geom::Point;
    use wall_core::store::WallStore;

    #[test]
    fn offline_wall_import_writes_target_namespace_store() {
        let dir = std::env::temp_dir().join(format!("wallctl-imp-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let export_file = dir.join("browser.json");
        fs::write(
            &export_file,
            r#"{"glyph3d.wall.relay-8765.positions":"{\"t1\":{\"x\":12,\"y\":-24}}","glyph3d.wall.relay-8765.names":"{\"t1\":\"api\"}"}"#,
        )
        .unwrap();

        let argv = vec![
            "wall.import".to_string(),
            export_file.to_string_lossy().into_owned(),
        ];
        let summary = run_offline_import(&Target::namespace("probe"), &argv, &dir)
            .expect("offline import succeeds");
        assert!(summary.starts_with("OK: imported browser localStorage (relay-8765)"), "{summary}");

        let store = WallStore::open(&dir, "probe");
        assert_eq!(store.position("t1"), Some(Point::new(12.0, -24.0)));
        assert_eq!(store.name("t1"), Some("api"));

        let _ = fs::remove_dir_all(&dir);
    }
}
