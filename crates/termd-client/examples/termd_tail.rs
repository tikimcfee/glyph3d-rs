//! `termd_tail`: connect to or start a `g3d-termd` namespace, create (or adopt) a shell,
//! stream its output to stdout, and send stdin lines as `paste` requests (followed by `Enter`
//! when a line ends in `\n` so interactive shells with bracketed paste execute each line).
//!
//! Usage:
//!   G3D_TERMD_DIR=/tmp/g3d-try cargo run -p termd-client --example termd_tail -- <namespace> [id]

use std::io::{BufRead, Write};
use termd_client::{Create, Event, IfExists, Key, Role, Target, Termd};

const FORBIDDEN_NAMESPACES: [&str; 3] = ["wall", "agents", "default"];

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(ns) = args.next() else {
        eprintln!(
            "usage: termd_tail <namespace> [terminal-id]\n\
             hint:  G3D_TERMD_DIR=/tmp/g3d-tail cargo run -p termd-client --example termd_tail -- scratch"
        );
        std::process::exit(2);
    };
    if FORBIDDEN_NAMESPACES.contains(&ns.as_str()) {
        eprintln!(
            "termd_tail: refusing live namespace {ns:?} (pick a scratch namespace, e.g. `scratch`)"
        );
        std::process::exit(2);
    }
    let term_id = args.next().unwrap_or_else(|| "tail-shell".to_owned());

    let target = Target::namespace(&ns);
    let sock_path = match target.resolve_socket() {
        Ok(path) => path,
        Err(msg) => {
            eprintln!("termd_tail: {msg}");
            std::process::exit(2);
        }
    };

    let client = match Termd::connect_or_start(&target, "termd-tail", Box::new(|| {})) {
        Ok(client) => client,
        Err(err) => {
            eprintln!("termd_tail: {} -> {err}", sock_path.display());
            std::process::exit(1);
        }
    };

    let created = match client.create(&Create {
        id: term_id.clone(),
        cols: 80,
        rows: 24,
        if_exists: Some(IfExists::Adopt),
        ..Create::default()
    }) {
        Ok(info) => info,
        Err(err) => {
            eprintln!("termd_tail: create `{term_id}`: {err}");
            std::process::exit(1);
        }
    };

    let sub = match client.subscribe(&term_id, Role::Own, Some((80, 24)), 0) {
        Ok(sub) => sub,
        Err(err) => {
            eprintln!("termd_tail: subscribe `{term_id}`: {err}");
            std::process::exit(1);
        }
    };

    eprintln!(
        "[termd_tail] {} `{}` (handle {}, pid {}, {}x{}) at {}",
        if created.created { "created" } else { "adopted" },
        created.id,
        sub.handle,
        created.pid,
        sub.cols,
        sub.rows,
        sock_path.display()
    );

    let writer_handle = client.handle();
    let handle_id = sub.handle;

    // Read stdin lines on a worker thread and send each line as a paste.
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut line = String::new();
        loop {
            line.clear();
            match stdin.lock().read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let had_newline = line.ends_with('\n') || line.ends_with('\r');
                    let body = line.trim_end_matches(['\r', '\n']);
                    if !body.is_empty() {
                        if let Err(err) = writer_handle.paste(handle_id, body) {
                            eprintln!("[termd_tail] paste error: {err}");
                            break;
                        }
                    }
                    if had_newline {
                        let enter = Key {
                            code: "Enter".into(),
                            key: "Enter".into(),
                            ..Key::default()
                        };
                        if let Err(err) = writer_handle.key(handle_id, &enter) {
                            eprintln!("[termd_tail] key error: {err}");
                            break;
                        }
                    }
                }
                Err(err) => {
                    eprintln!("[termd_tail] stdin error: {err}");
                    break;
                }
            }
        }
    });

    let stdout = std::io::stdout();
    while let Ok(event) = client.events().recv() {
        match event {
            Event::Repaint { handle, bytes, .. } | Event::Output { handle, bytes, .. }
                if handle == sub.handle =>
            {
                let mut out = stdout.lock();
                let _ = out.write_all(&bytes);
                let _ = out.flush();
            }
            Event::Reconnected { generation, .. } => {
                eprintln!("\n[termd_tail] reconnected (brain generation {generation:?})");
            }
            Event::Exit { handle, code, signal, .. } if handle == sub.handle => {
                eprintln!("\n[termd_tail] `{term_id}` exited (code={code:?}, signal={signal:?})");
            }
            Event::Closed { handle } if handle == sub.handle => {
                break;
            }
            Event::Disconnected { message, .. } => {
                eprintln!("\n[termd_tail] disconnected: {message}");
                std::process::exit(1);
            }
            _ => {}
        }
    }
}
