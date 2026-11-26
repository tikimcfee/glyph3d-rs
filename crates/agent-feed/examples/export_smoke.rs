//! Read-only smoke check against gomuxai's `agent export --json --all -q`.
//!
//! Discovers `~/.local/bin/agent` (or `agent` on `PATH`), runs a single non-following snapshot via
//! [`agent_feed::export_once`], and prints the parsed host, store count, total/live agent counts,
//! and host-grouped live roster (`by_host()`). Never spawns, sends, attaches, or cleans up agents.

use agent_feed::{export_once, AgentCli, FleetOptions};

fn main() {
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
            for group in snap.by_host() {
                println!("host {:?} ({} live):", group.host, group.agents.len());
                for entry in group.agents {
                    println!(
                        "  {} status={:?} attachable={} harness={:?} session_id={:?}",
                        entry.target,
                        entry.status,
                        entry.attachable,
                        entry.harness,
                        entry.session_id,
                    );
                }
            }
        }
    }
}
