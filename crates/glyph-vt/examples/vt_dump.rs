//! Feed VT bytes from a file or stdin into `glyph_vt::Vt` at a given grid size
//! and print the resulting visible screen, one line per row.
//!
//! Usage:
//!   cargo run -p glyph-vt --example vt_dump -- [--size COLSxROWS] [FILE]
//!   cargo run -p glyph-vt --example vt_dump -- [COLSxROWS] [FILE]

use std::env;
use std::fs;
use std::io::{self, Read};
use std::path::Path;
use std::process::ExitCode;

use glyph_vt::{Theme, Vt};
use term_cells::Screen;

const USAGE: &str = "\
Usage: vt_dump [OPTIONS] [COLSxROWS] [FILE]

Read VT bytes from FILE (or stdin when omitted or '-') into a glyph_vt::Vt of
COLSxROWS (default 80x24) and print the resulting screen, one line per row.

Options:
  -s, --size COLSxROWS   Grid size, e.g. 80x24 or 40x10
      --cols COLS        Grid width in columns
      --rows ROWS        Grid height in rows
  -h, --help             Print this help message";

fn parse_size(spec: &str) -> Option<(u16, u16)> {
    let (cols_str, rows_str) = spec.split_once('x').or_else(|| spec.split_once('X'))?;
    let cols: u16 = cols_str.parse().ok()?;
    let rows: u16 = rows_str.parse().ok()?;
    if cols == 0 || rows == 0 {
        return None;
    }
    Some((cols, rows))
}

fn main() -> ExitCode {
    let mut cols: u16 = 80;
    let mut rows: u16 = 24;
    let mut input_path: Option<String> = None;

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "-s" | "--size" => {
                let Some(val) = args.next() else {
                    eprintln!("vt_dump: {arg} requires COLSxROWS");
                    return ExitCode::FAILURE;
                };
                let Some((c, r)) = parse_size(&val) else {
                    eprintln!("vt_dump: invalid size '{val}' (expected COLSxROWS, positive u16)");
                    return ExitCode::FAILURE;
                };
                cols = c;
                rows = r;
            }
            "--cols" => {
                let Some(val) = args.next() else {
                    eprintln!("vt_dump: --cols requires a number");
                    return ExitCode::FAILURE;
                };
                let Ok(c) = val.parse::<u16>() else {
                    eprintln!("vt_dump: invalid --cols '{val}'");
                    return ExitCode::FAILURE;
                };
                if c == 0 {
                    eprintln!("vt_dump: --cols must be at least 1");
                    return ExitCode::FAILURE;
                }
                cols = c;
            }
            "--rows" => {
                let Some(val) = args.next() else {
                    eprintln!("vt_dump: --rows requires a number");
                    return ExitCode::FAILURE;
                };
                let Ok(r) = val.parse::<u16>() else {
                    eprintln!("vt_dump: invalid --rows '{val}'");
                    return ExitCode::FAILURE;
                };
                if r == 0 {
                    eprintln!("vt_dump: --rows must be at least 1");
                    return ExitCode::FAILURE;
                }
                rows = r;
            }
            _ if arg.starts_with("--size=") => {
                let val = &arg["--size=".len()..];
                let Some((c, r)) = parse_size(val) else {
                    eprintln!("vt_dump: invalid size '{val}' (expected COLSxROWS, positive u16)");
                    return ExitCode::FAILURE;
                };
                cols = c;
                rows = r;
            }
            _ => {
                if input_path.is_none() && !Path::new(&arg).exists() {
                    if let Some((c, r)) = parse_size(&arg) {
                        cols = c;
                        rows = r;
                        continue;
                    }
                }
                if input_path.is_some() {
                    eprintln!("vt_dump: unexpected argument '{arg}'\n\n{USAGE}");
                    return ExitCode::FAILURE;
                }
                input_path = Some(arg);
            }
        }
    }

    let bytes = match input_path.as_deref() {
        None | Some("-") => {
            let mut buf = Vec::new();
            if let Err(err) = io::stdin().read_to_end(&mut buf) {
                eprintln!("vt_dump: reading stdin: {err}");
                return ExitCode::FAILURE;
            }
            buf
        }
        Some(path) => match fs::read(path) {
            Ok(buf) => buf,
            Err(err) => {
                eprintln!("vt_dump: reading '{path}': {err}");
                return ExitCode::FAILURE;
            }
        },
    };

    let mut vt = Vt::new(cols, rows, &Theme::default());
    vt.feed(&bytes);

    let mut screen = Screen::new(cols, rows);
    vt.fill(&mut screen);

    for row in 0..rows {
        println!("{}", vt.row_text(row));
    }
    ExitCode::SUCCESS
}
