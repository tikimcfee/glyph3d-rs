//! Read-only smoke check against gomuxai's `agent export --json --all -q`.
//!
//! Discovers `~/.local/bin/agent` (or `agent` on `PATH`), runs a single non-following snapshot via
//! [`agent_feed::export_once`], and prints the parsed host, store count, total/live agent counts,
//! and host-grouped live roster (`by_host()`). Never spawns, sends, attaches, or cleans up agents.

//!
//! `export_smoke --fold <file.jsonl>` instead folds a recorded follow stream (`agent export
//! --json --follow --all -q > file.jsonl`) line by line through [`agent_feed::FleetState`] and
//! prints how many lines folded, how many it could not read (with the first error), and the live
//! roster it ends with: the check that the fold reads what the real CLI prints.

use agent_feed::{export_once, AgentCli, FleetOptions, FleetSnapshot, FleetState};

fn print_roster(snap: &FleetSnapshot) {
    for group in snap.by_host() {
        println!("host {:?} ({} live):", group.host, group.agents.len());
        for entry in group.agents {
            println!(
                "  {} status={:?} attachable={} harness={:?} session_id={:?}",
                entry.target, entry.status, entry.attachable, entry.harness, entry.session_id,
            );
        }
    }
}

fn fold_file(path: &str) {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("reading {path}: {e}");
        std::process::exit(1);
    });
    let mut state = FleetState::new();
    let (mut lines, mut changed, mut failed) = (0usize, 0usize, 0usize);
    let mut first_error = None;
    for raw in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        lines += 1;
        match state.apply_line(raw) {
            Ok(true) => changed += 1,
            Ok(false) => {}
            Err(e) => {
                failed += 1;
                first_error.get_or_insert(e);
            }
        }
    }
    println!("folded {lines} lines: {changed} changed the fleet, {failed} unreadable");
    if let Some(e) = first_error {
        println!("first error: {e}");
    }
    print_roster(state.snapshot());
    if failed > 0 {
        std::process::exit(1);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let [flag, path] = args.as_slice() {
        if flag == "--fold" {
            return fold_file(path);
        }
    }
    let cli = AgentCli::discover();
    println!("agent binary: {}", cli.program.display());
    match export_once(&cli, &FleetOptions::default()) {
        Err(e) => {
            eprintln!("export_once failed: {e}");
            std::process::exit(1);
        }
        Ok(snap) => {
            let live_count = snap.agents.iter().filter(|a| a.live).count();
            println!(
                "snapshot gen={} host={:?} stores={} agents={} (live={}, departed={})",
                snap.gen,
                snap.host,
                snap.dirs.len(),
                snap.agents.len(),
                live_count,
                snap.agents.len().saturating_sub(live_count),
            );
            print_roster(&snap);
        }
    }
}
