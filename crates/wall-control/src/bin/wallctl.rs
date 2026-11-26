//! CLI door onto a running native wall (`wallctl [--ns NAME] <verb and args...>`).

use std::io::{self, BufRead, Write};
use std::process::ExitCode;

use wall_control::{format_command_line, Client, Target, DEFAULT_NAMESPACE};

const USAGE: &str = "\
usage: wallctl [--ns NAME | --socket PATH] [<verb> [args...]]

Send command verbs to a running native wall over its local Unix control socket.
  --ns NAME      Target namespace (default: rs-wall -> wall-rs-wall.sock)
  --socket PATH  Explicit Unix socket path
  -h, --help     Print this help

With <verb> [args...], sends one command, prints the reply, and exits 0 (or 1 on ERR:).
With no <verb>, reads command lines from stdin over one connection until EOF.";

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

    let mut client = match Client::connect(&cli.target) {
        Ok(client) => client,
        Err(err) => {
            let _ = writeln!(io::stderr(), "ERR: {err}");
            return ExitCode::FAILURE;
        }
    };

    let mut stdout = io::stdout().lock();

    if !cli.command_argv.is_empty() {
        let line = format_command_line(&cli.command_argv);
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
