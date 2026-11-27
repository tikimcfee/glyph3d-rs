//! Port of the pure helpers in `wall/WallBus.js`: gomuxai status ranks and colours
//! ([`BUS_STATUSES`], [`bus_status`]), target syntax validation and formatting
//! ([`is_bus_target`], [`bus_target`], [`local_bus_name`], [`gomux_screen_id`]),
//! attachability ([`bus_attachable`]), host/agent ordering and lookup ([`bus_hosts`],
//! [`find_bus_agent`]), and the remote attach shell command ([`attach_argv`]).

use std::cmp::Ordering;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Display rank, colour (`#rrggbb`), and canonical label for an agent status (`BUS_STATUSES`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BusStatusInfo {
    pub rank: u8,
    pub colour: &'static str,
    pub word: &'static str,
}

/// Known gomuxai agent statuses in display priority order (`WallBus.BUS_STATUSES`).
pub const BUS_STATUSES: &[(&str, BusStatusInfo)] = &[
    (
        "waiting",
        BusStatusInfo {
            rank: 0,
            colour: "#f2b45a",
            word: "waiting",
        },
    ),
    (
        "error",
        BusStatusInfo {
            rank: 1,
            colour: "#ef6f6c",
            word: "error",
        },
    ),
    (
        "processing",
        BusStatusInfo {
            rank: 2,
            colour: "#7fb0ff",
            word: "processing",
        },
    ),
    (
        "idle",
        BusStatusInfo {
            rank: 3,
            colour: "#8ef0b8",
            word: "idle",
        },
    ),
    (
        "unknown",
        BusStatusInfo {
            rank: 4,
            colour: "#8b95a7",
            word: "unknown",
        },
    ),
    (
        "ended",
        BusStatusInfo {
            rank: 5,
            colour: "#5d6574",
            word: "ended",
        },
    ),
];

/// Look up [`BusStatusInfo`] for `status`, falling back to `unknown` (`WallBus.busStatus`).
pub fn bus_status(status: Option<&str>) -> BusStatusInfo {
    if let Some(s) = status {
        if let Some((_, info)) = BUS_STATUSES.iter().find(|(k, _)| *k == s) {
            return *info;
        }
    }
    BusStatusInfo {
        rank: 4,
        colour: "#8b95a7",
        word: "unknown",
    }
}

fn is_valid_bus_name(part: &str) -> bool {
    let mut bytes = part.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    first.is_ascii_alphanumeric()
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Whether `s` is a valid gomuxai attach target (`name` or `name@host`, matching
/// `^[A-Za-z0-9][A-Za-z0-9_-]*(@[A-Za-z0-9][A-Za-z0-9_-]*)?$`; `WallBus.isBusTarget`).
pub fn is_bus_target(s: &str) -> bool {
    match s.split_once('@') {
        None => is_valid_bus_name(s),
        Some((name, host)) => is_valid_bus_name(name) && is_valid_bus_name(host),
    }
}

/// One agent entry in a [`GomuxSnapshot`] (`GomuxProvider.GomuxAgent`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GomuxAgent {
    pub name: String,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub parent: String,
    #[serde(default)]
    pub backend: String,
    #[serde(default)]
    pub session: String,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub data_dir: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub live: bool,
    #[serde(default)]
    pub attached: Option<bool>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub end_reason: Option<String>,
    #[serde(default)]
    pub event: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub tool_input: Option<String>,
    #[serde(default)]
    pub status_at: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub transcript: Option<String>,
    #[serde(default)]
    pub harness: Option<String>,
    #[serde(default)]
    pub transcript_size: Option<u64>,
    #[serde(default)]
    pub transcript_mtime: Option<u64>,
}

/// Snapshot of gomuxai agents across one or more hosts (`GomuxProvider.GomuxSnapshot`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GomuxSnapshot {
    #[serde(default)]
    pub gen: u64,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub dirs: Vec<String>,
    #[serde(default)]
    pub agents: Vec<GomuxAgent>,
}

/// Format the target `agent attach` takes for `agent`: `name`, or `name@host` when `host` is
/// non-empty (`WallBus.busTarget`).
pub fn bus_target(agent: &GomuxAgent) -> String {
    match agent.host.as_deref().filter(|h| !h.is_empty()) {
        Some(host) => format!("{}@{host}", agent.name),
        None => agent.name.clone(),
    }
}

/// If `target` (`name` or `name@host`) names an agent on THIS machine (no `@host`, or
/// `@<snap.host>` when `snap` names this machine), return its bare local `name`; otherwise
/// `None` (`WallBus.localBusName`).
pub fn local_bus_name<'a>(target: &'a str, snap: Option<&GomuxSnapshot>) -> Option<&'a str> {
    if !is_bus_target(target) {
        return None;
    }
    match target.split_once('@') {
        None => Some(target),
        Some((name, host)) => {
            let local_host = snap
                .and_then(|s| s.host.as_deref())
                .filter(|h| !h.is_empty())?;
            (host == local_host).then_some(name)
        }
    }
}

/// Display terminal id for a local gomuxai agent's direct `termd` subscription
/// (`gx-<sanitized>`, matching `WallBus.gomuxScreenId` and `cli/gomux.go`).
pub fn gomux_screen_id(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(61)
        .collect();
    format!("gx-{sanitized}")
}

/// Whether `agent` has a live session to attach to (`WallBus.busAttachable`).
///
/// An omitted `attached` (`None`, when a remote host could not report it) is treated as
/// attachable; only an explicit `Some(false)` refuses.
pub fn bus_attachable(agent: &GomuxAgent) -> bool {
    agent.attached != Some(false)
}

/// Enriched agent entry returned inside [`BusHostGroup`], carrying its computed `target`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BusAgent {
    pub agent: GomuxAgent,
    pub target: String,
}

impl std::ops::Deref for BusAgent {
    type Target = GomuxAgent;

    fn deref(&self) -> &Self::Target {
        &self.agent
    }
}

/// One host's sorted live agents as returned by [`bus_hosts`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BusHostGroup {
    pub host: String,
    pub agents: Vec<BusAgent>,
}

fn locale_like_cmp(a: &str, b: &str) -> Ordering {
    for (ca, cb) in a.chars().zip(b.chars()) {
        let la = ca.to_ascii_lowercase();
        let lb = cb.to_ascii_lowercase();
        match la.cmp(&lb) {
            Ordering::Equal => match ca.cmp(&cb) {
                Ordering::Equal => continue,
                // In JS `localeCompare`, lowercase sorts before uppercase for the same letter.
                other => return other.reverse(),
            },
            other => return other,
        }
    }
    a.chars().count().cmp(&b.chars().count())
}

/// Group live agents by host: this machine's host first (`""` or `snap.host`), then remote
/// hosts alphabetically; within each host, `waiting` first (`bus_status` rank), then by name
/// (`WallBus.busHosts`).
pub fn bus_hosts(snap: Option<&GomuxSnapshot>) -> Vec<BusHostGroup> {
    let Some(snap) = snap else {
        return Vec::new();
    };
    let mut by_host: BTreeMap<String, Vec<BusAgent>> = BTreeMap::new();
    let mut host_order: Vec<String> = Vec::new();

    for a in &snap.agents {
        if !a.live || a.name.is_empty() {
            continue;
        }
        let host = a.host.clone().unwrap_or_default();
        if !by_host.contains_key(&host) {
            host_order.push(host.clone());
        }
        by_host.entry(host).or_default().push(BusAgent {
            target: bus_target(a),
            agent: a.clone(),
        });
    }

    for list in by_host.values_mut() {
        list.sort_by(|x, y| {
            bus_status(x.status.as_deref())
                .rank
                .cmp(&bus_status(y.status.as_deref()).rank)
                .then_with(|| locale_like_cmp(&x.name, &y.name))
        });
    }

    let local_host = snap.host.as_deref().unwrap_or("");
    let here = |h: &str| -> u8 {
        if h.is_empty() || (!local_host.is_empty() && h == local_host) {
            0
        } else {
            1
        }
    };
    host_order.sort_by(|x, y| here(x).cmp(&here(y)).then_with(|| locale_like_cmp(x, y)));

    host_order
        .into_iter()
        .filter_map(|host| {
            by_host
                .remove(&host)
                .map(|agents| BusHostGroup { host, agents })
        })
        .collect()
}

/// Find the live agent named by `target` (`name` or `name@host`) in `snap` (`WallBus.find`).
///
/// A bare `name` also matches `name@<snap.host>` when `snap.host` is populated.
pub fn find_bus_agent(snap: Option<&GomuxSnapshot>, target: &str) -> Option<BusAgent> {
    let hosts = bus_hosts(snap);
    for h in &hosts {
        for a in &h.agents {
            if a.target == target {
                return Some(a.clone());
            }
        }
    }
    if !target.is_empty() && !target.contains('@') {
        if let Some(local_host) = snap
            .and_then(|s| s.host.as_deref())
            .filter(|h| !h.is_empty())
        {
            for h in hosts {
                if h.host != local_host {
                    continue;
                }
                for a in h.agents {
                    if a.name == target {
                        return Some(a);
                    }
                }
            }
        }
    }
    None
}

/// Find the DEPARTED agent (cleaned up: `live: false`) `target` names in `snap`, matched as
/// [`find_bus_agent`] matches a live one. `None` when the target is live, unknown to the
/// snapshot, or there is no snapshot: only a record of its end says an agent is gone.
pub fn find_departed_bus_agent<'a>(
    snap: Option<&'a GomuxSnapshot>,
    target: &str,
) -> Option<&'a GomuxAgent> {
    let snap = snap?;
    if target.is_empty() || find_bus_agent(Some(snap), target).is_some() {
        return None;
    }
    let local = local_bus_name(target, Some(snap));
    let local_host = snap.host.as_deref().unwrap_or("");
    snap.agents.iter().find(|a| {
        !a.live
            && (bus_target(a) == target
                || local.is_some_and(|name| {
                    a.name == name && a.host.as_deref().unwrap_or("") == local_host
                }))
    })
}

const ATTACH_RESET: &str = "\\033[?1000l\\033[?1002l\\033[?1003l\\033[?1006l\\033[?1015l\\033[?1004l\\033[?2004l\\033[?1049l\\033[?25h\\033[0m";

/// What the attach terminal says, once, when its agent is not an agent any more. `%s` is the
/// target.
pub const ATTACH_GONE_NOTE: &str =
    "[%s is not an agent any more (it was cleaned up), so there is nothing to attach to. Enter closes.]";

/// Build the `sh -c` argv a remote `wall.attach <name>@<host>` terminal runs (`WallBus.attachArgv`).
///
/// Validated with [`is_bus_target`] first; `target` is passed exclusively as positional `$1`,
/// never interpolated into the shell script text. `agent_program` is passed as `$2` in the
/// same way.
///
/// **Which `agent`.** `agent_program` is the binary the wall's own fleet feed runs, when the
/// wall resolved one to a path: the terminal then runs that same binary and looks nowhere
/// else. The agents it lists and the agents it attaches come from one CLI and one store, a
/// configured `agent_binary` is honoured here too, and a test's stub CLI is the only one its
/// terminals can reach. Until 2026-11-27 the script always looked `agent` up itself: termd
/// gives a terminal `PATH=/usr/bin:/bin`, so it took the login shell's PATH, and a smoke
/// whose wall was fed by a stub attached with the REAL CLI. With `None` (the wall found no
/// binary) the script still looks it up that way.
///
/// **When it ends.** A clean detach closes the terminal. A failed attach offers another try,
/// since most failures pass (the bus or ssh was down, the agent's session is gone until it
/// is resumed). One does not: the agent was cleaned up. Until 2026-11-27 that looped: every
/// Enter ran `agent attach` again, which said `agent "x" not found` again. `agent attach`
/// exits 1 for every failure, so after one the script asks `agent status` (which needs no
/// terminal) and, when the answer is that there is no such agent, says so and closes on the
/// next Enter.
pub fn attach_argv(target: &str, agent_program: Option<&str>) -> Result<Vec<String>, String> {
    if !is_bus_target(target) {
        return Err(format!(
            "'{target}' isn't an agent name (name or name@host)"
        ));
    }
    let script = [
        "A=\"${2:-}\"".to_string(),
        "if [ -z \"$A\" ]; then".to_string(),
        "  command -v agent >/dev/null 2>&1 || PATH=\"$(\"${SHELL:-/bin/sh}\" -lc 'printf %s \"$PATH\"' 2>/dev/null):$PATH\"".to_string(),
        "  A=agent".to_string(),
        "fi".to_string(),
        "n=\"${1%%@*}\"".to_string(),
        "while :; do".to_string(),
        "  \"$A\" attach \"$1\"; s=$?".to_string(),
        "  [ \"$s\" -eq 0 ] && exit 0".to_string(),
        format!("  printf '{ATTACH_RESET}'"),
        "  stty sane 2>/dev/null".to_string(),
        // Both agent CLIs (Go and Rust) word a missing agent `agent "<name>" not found`.
        "  w=\"$(\"$A\" status \"$1\" 2>&1 </dev/null)\"".to_string(),
        "  case \"$w\" in *\"agent \\\"$n\\\" not found\"*)".to_string(),
        format!("    printf '\\r\\n{ATTACH_GONE_NOTE}\\r\\n' \"$1\""),
        "    read -r a; exit 0;;".to_string(),
        "  esac".to_string(),
        "  printf '\\r\\n[agent attach %s ended with status %d. Enter attaches again; q then Enter closes.]\\r\\n' \"$1\" \"$s\"".to_string(),
        "  read -r a || exit 0".to_string(),
        "  [ \"$a\" = q ] && exit 0".to_string(),
        "done".to_string(),
    ]
    .join("\n");
    let mut argv = vec![
        "sh".to_string(),
        "-c".to_string(),
        script,
        "wall-attach".to_string(),
        target.to_string(),
    ];
    if let Some(program) = agent_program.filter(|p| !p.is_empty()) {
        argv.push(program.to_string());
    }
    Ok(argv)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    use super::*;
    use crate::store::WallStore;

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(prefix: &str) -> Self {
            let n = TEST_SEQ.fetch_add(1, AtomicOrdering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "wall-core-bus-{prefix}-{}-{n}",
                std::process::id()
            ));
            fs::create_dir_all(&dir).unwrap();
            ScratchDir(dir)
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn make_agent(name: &str) -> GomuxAgent {
        GomuxAgent {
            name: name.to_string(),
            live: true,
            attached: Some(true),
            status: Some("idle".to_string()),
            ..GomuxAgent::default()
        }
    }

    #[test]
    fn target_is_name_here_and_name_at_host_on_another_host() {
        assert_eq!(bus_target(&make_agent("helper")), "helper");
        let remote = GomuxAgent {
            host: Some("workstation".to_string()),
            ..make_agent("helper")
        };
        assert_eq!(bus_target(&remote), "helper@workstation");

        for ok in ["helper", "helper-2", "a_b@workstation", "X@laptop-2"] {
            assert!(is_bus_target(ok), "expected valid target: {ok}");
        }
        for bad in ["", "-x", "a b", "a@", "@h", "a@b@c", "a;rm", "a.b", "$(x)"] {
            assert!(!is_bus_target(bad), "expected invalid target: {bad}");
        }
    }

    #[test]
    fn hosts_live_agents_only_this_host_first_then_by_name_waiting_first_within_host() {
        let snap = GomuxSnapshot {
            gen: 1,
            host: None,
            dirs: Vec::new(),
            agents: vec![
                GomuxAgent {
                    host: Some("workstation".into()),
                    status: Some("processing".into()),
                    ..make_agent("zeta")
                },
                GomuxAgent {
                    status: Some("idle".into()),
                    ..make_agent("b")
                },
                GomuxAgent {
                    status: Some("ended".into()),
                    ..make_agent("a")
                },
                GomuxAgent {
                    status: Some("waiting".into()),
                    ..make_agent("c")
                },
                GomuxAgent {
                    live: false,
                    ..make_agent("gone")
                },
                GomuxAgent {
                    host: Some("aardvark".into()),
                    ..make_agent("alpha")
                },
                GomuxAgent {
                    status: Some("idle".into()),
                    ..make_agent("d")
                },
            ],
        };
        let hosts = bus_hosts(Some(&snap));
        let host_names: Vec<&str> = hosts.iter().map(|h| h.host.as_str()).collect();
        assert_eq!(host_names, vec!["", "aardvark", "workstation"]);
        let local_names: Vec<&str> = hosts[0].agents.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(local_names, vec!["c", "b", "d", "a"]);
        assert_eq!(hosts[2].agents[0].target, "zeta@workstation");
        assert!(bus_hosts(None).is_empty());

        let named_snap = GomuxSnapshot {
            gen: 1,
            host: Some("workstation".into()),
            dirs: Vec::new(),
            agents: vec![
                GomuxAgent {
                    host: Some("aardvark".into()),
                    ..make_agent("far")
                },
                GomuxAgent {
                    host: Some("workstation".into()),
                    ..make_agent("here")
                },
                GomuxAgent {
                    host: Some("laptop".into()),
                    attached: None,
                    ..make_agent("far")
                },
            ],
        };
        let named = bus_hosts(Some(&named_snap));
        let named_hosts: Vec<&str> = named.iter().map(|h| h.host.as_str()).collect();
        assert_eq!(named_hosts, vec!["workstation", "aardvark", "laptop"]);
        let targets: Vec<&str> = named.iter().map(|h| h.agents[0].target.as_str()).collect();
        assert_eq!(
            targets,
            vec!["here@workstation", "far@aardvark", "far@laptop"]
        );
    }

    #[test]
    fn attachable_unless_host_says_session_is_gone_and_unknown_status_reads_as_unknown() {
        assert!(bus_attachable(&make_agent("a")));
        assert!(bus_attachable(&GomuxAgent {
            attached: None,
            ..make_agent("a")
        }));
        assert!(!bus_attachable(&GomuxAgent {
            attached: Some(false),
            ..make_agent("a")
        }));
        assert_eq!(bus_status(Some("nonsense")).word, "unknown");
        assert_eq!(bus_status(None).word, "unknown");
    }

    #[test]
    fn attach_argv_refuses_non_agent_names_and_never_embeds_target_in_script() {
        assert!(attach_argv("a b", None)
            .unwrap_err()
            .contains("isn't an agent name"));
        assert!(attach_argv("$(touch x)", None).is_err());
        let argv = attach_argv("helper@workstation", None).unwrap();
        assert_eq!(&argv[0..2], &["sh", "-c"]);
        assert!(!argv[2].contains("helper"));
        assert_eq!(argv.last().unwrap(), "helper@workstation");

        // The wall's own `agent` binary rides as `$2`, never in the script's text either.
        let named = attach_argv("helper@workstation", Some("/opt/tools/bin/agent")).unwrap();
        assert_eq!(named[2], argv[2], "one script, whoever names the binary");
        assert!(!named[2].contains("/opt/tools"));
        assert_eq!(&named[3..], &["wall-attach", "helper@workstation", "/opt/tools/bin/agent"]);
        assert_eq!(attach_argv("helper", Some("")).unwrap().len(), 5, "an empty program is none");
    }

    /// The bug of 2026-11-27: a smoke's wall listed agents through a stub CLI, and the
    /// terminal a click opened ran the real one, found through the login shell's PATH.
    /// Given the wall's binary, the attach program runs that and looks nowhere else.
    #[test]
    fn attach_program_runs_the_named_agent_binary_and_never_looks_another_up() {
        let dir = ScratchDir::new("named");
        let log_path = dir.0.join("ran");
        let script_for = |who: &str, exit: i32| {
            format!(
                "#!/bin/sh\nprintf '{who} %s\\n' \"$*\" >> \"{}\"\nexit {exit}\n",
                log_path.display()
            )
        };
        let write = |path: &std::path::Path, body: String| {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        };
        // The wall's binary, off every PATH; a decoy `agent` first on PATH; and a login
        // shell whose PATH leads to another decoy.
        let named = dir.0.join("wall/agent");
        let on_path = dir.0.join("path/agent");
        let login_agent = dir.0.join("login/agent");
        let login_sh = dir.0.join("login-sh");
        write(&named, script_for("named", 0));
        write(&on_path, script_for("on-path", 0));
        write(&login_agent, script_for("login", 0));
        write(
            &login_sh,
            format!(
                "#!/bin/sh\nprintf 'login-shell\\n' >> \"{}\"\n[ \"$1\" = -lc ] && printf %s \"{}:/usr/bin:/bin\"\n",
                log_path.display(),
                login_agent.parent().unwrap().display()
            ),
        );

        let run = |program: Option<&str>, path: String| -> String {
            let _ = fs::remove_file(&log_path);
            let argv = attach_argv("helper@workstation", program).unwrap();
            let out = Command::new(&argv[0])
                .args(&argv[1..])
                .env("PATH", path)
                .env("SHELL", &login_sh)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .unwrap();
            assert_eq!(out.status.code(), Some(0));
            fs::read_to_string(&log_path).unwrap_or_default()
        };
        let decoy_first = format!("{}:/usr/bin:/bin", on_path.parent().unwrap().display());

        // Named: that binary, though another `agent` is first on PATH, and no login shell.
        assert_eq!(
            run(Some(named.to_str().unwrap()), decoy_first.clone()),
            "named attach helper@workstation\n"
        );
        // And with no `agent` on PATH at all, still no login shell asked.
        assert_eq!(
            run(Some(named.to_str().unwrap()), "/usr/bin:/bin".to_string()),
            "named attach helper@workstation\n"
        );
        // Not named: looked up as before, PATH first, then the login shell's.
        assert_eq!(run(None, decoy_first), "on-path attach helper@workstation\n");
        assert_eq!(
            run(None, "/usr/bin:/bin".to_string()),
            "login-shell\nlogin attach helper@workstation\n"
        );

        // A failed attach asks the SAME binary whether the agent is still there.
        write(&named, script_for("named", 1));
        assert_eq!(
            run(Some(named.to_str().unwrap()), "/usr/bin:/bin".to_string()),
            "named attach helper@workstation\nnamed status helper@workstation\n"
        );
    }

    #[test]
    fn attach_program_exits_on_clean_detach_resets_terminal_on_failure_and_retries_on_enter() {
        let dir = ScratchDir::new("sh");
        let log_path = dir.0.join("args");
        let agent_path = dir.0.join("agent");
        // `attach` exits `$AGENT_EXIT`; `status` answers a live agent.
        let stub_script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"{}\"\n[ \"$1\" = status ] && {{ echo 'helper: idle'; exit 0; }}\nexit \"${{AGENT_EXIT:-0}}\"\n",
            log_path.display()
        );
        fs::write(&agent_path, stub_script).unwrap();
        fs::set_permissions(&agent_path, fs::Permissions::from_mode(0o755)).unwrap();

        let run = |exit_code: i32, input: &str| -> std::process::Output {
            let _ = fs::remove_file(&log_path);
            let argv = attach_argv("helper@workstation", None).unwrap();
            let mut child = Command::new(&argv[0])
                .args(&argv[1..])
                .env("PATH", format!("{}:/usr/bin:/bin", dir.0.display()))
                .env("AGENT_EXIT", exit_code.to_string())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            {
                let mut stdin = child.stdin.take().unwrap();
                stdin.write_all(input.as_bytes()).unwrap();
            }
            child.wait_with_output().unwrap()
        };

        let ok = run(0, "");
        assert_eq!(ok.status.code(), Some(0));
        assert_eq!(String::from_utf8_lossy(&ok.stdout), "");
        assert_eq!(
            fs::read_to_string(&log_path).unwrap(),
            "attach\nhelper@workstation\n"
        );

        let reason =
            "[agent attach helper@workstation ended with status 255. Enter attaches again; q then Enter closes.]";
        let closed = run(255, "q\n");
        assert_eq!(closed.status.code(), Some(0));
        let closed_out = String::from_utf8_lossy(&closed.stdout);
        assert_eq!(closed_out.matches(reason).count(), 1);
        // After a failed attach the script asks once whether the agent still exists.
        assert_eq!(
            fs::read_to_string(&log_path).unwrap(),
            "attach\nhelper@workstation\nstatus\nhelper@workstation\n"
        );
        let reason_pos = closed_out.find(reason).unwrap();
        let before = &closed_out[..reason_pos];
        for off in [
            "?1000l", "?1002l", "?1003l", "?1006l", "?1004l", "?2004l", "?1049l", "?25h",
        ] {
            assert!(
                before.contains(&format!("\x1b[{off}")),
                "missing reset sequence {off} before reason"
            );
        }

        // Enter attaches again; EOF closes.
        let again = run(1, "\n");
        assert_eq!(again.status.code(), Some(0));
        assert_eq!(
            fs::read_to_string(&log_path).unwrap(),
            "attach\nhelper@workstation\nstatus\nhelper@workstation\nattach\nhelper@workstation\nstatus\nhelper@workstation\n"
        );
        let again_out = String::from_utf8_lossy(&again.stdout);
        assert_eq!(again_out.matches("ended with status 1.").count(), 2);

        // When termd's PATH lacks `agent`, `$SHELL -lc` supplies the login PATH.
        let bin = ScratchDir::new("login-sh");
        let login_sh = bin.0.join("login-sh");
        fs::write(
            &login_sh,
            format!(
                "#!/bin/sh\n[ \"$1\" = -lc ] && printf %s \"{}:/usr/bin:/bin\"\n",
                dir.0.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&login_sh, fs::Permissions::from_mode(0o755)).unwrap();
        let _ = fs::remove_file(&log_path);
        let argv = attach_argv("helper", None).unwrap();
        let child = Command::new(&argv[0])
            .args(&argv[1..])
            .env("PATH", "/usr/bin:/bin")
            .env("SHELL", &login_sh)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let via_login = child.wait_with_output().unwrap();
        assert_eq!(via_login.status.code(), Some(0));
        assert_eq!(fs::read_to_string(&log_path).unwrap(), "attach\nhelper\n");
    }

    /// The bug of 2026-11-27: a row for a cleaned-up agent was clicked, and its terminal
    /// printed `agent "x" not found` and "Enter attaches again" for as long as Enter was
    /// pressed. A missing agent is said once and never tried again; any other failure still
    /// offers the retry.
    #[test]
    fn attach_program_never_retries_an_agent_that_is_gone_and_still_retries_other_failures() {
        let dir = ScratchDir::new("gone");
        let log_path = dir.0.join("args");
        let agent_path = dir.0.join("agent");
        // `attach` always fails, printing `$AGENT_SAYS` as the real CLI prints its error;
        // `status` prints the same and fails too.
        let stub_script = format!(
            "#!/bin/sh\nprintf '%s ' \"$@\" >> \"{log}\"\nprintf '\\n' >> \"{log}\"\nprintf '%s\\n' \"$AGENT_SAYS\" >&2\nexit 1\n",
            log = log_path.display()
        );
        fs::write(&agent_path, stub_script).unwrap();
        fs::set_permissions(&agent_path, fs::Permissions::from_mode(0o755)).unwrap();

        let run = |target: &str, says: &str, input: &str| -> (String, String) {
            let _ = fs::remove_file(&log_path);
            let argv = attach_argv(target, None).unwrap();
            let mut child = Command::new(&argv[0])
                .args(&argv[1..])
                .env("PATH", format!("{}:/usr/bin:/bin", dir.0.display()))
                .env("AGENT_SAYS", says)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            {
                let mut stdin = child.stdin.take().unwrap();
                stdin.write_all(input.as_bytes()).unwrap();
            }
            let out = child.wait_with_output().unwrap();
            assert_eq!(out.status.code(), Some(0), "the wrapper itself always ends cleanly");
            (
                String::from_utf8_lossy(&out.stdout).into_owned(),
                fs::read_to_string(&log_path).unwrap(),
            )
        };
        let gone_note = |target: &str| ATTACH_GONE_NOTE.replace("%s", target);
        let retry = "Enter attaches again";

        // Another host's agent, as the Rust CLI words it. Three Enters: one attach, no more.
        let (out, log) = run(
            "helper@workstation",
            r#"Error: agent "helper" not found on host workstation (3 agents there: agent list --host workstation)"#,
            "\n\n\n",
        );
        assert_eq!(log, "attach helper@workstation \nstatus helper@workstation \n");
        assert_eq!(out.matches(&gone_note("helper@workstation")).count(), 1, "{out}");
        assert!(!out.contains(retry), "a missing agent offers no retry: {out}");

        // This host's, as the Go CLI words it (no host), and `q` closes it as well.
        let (out, log) = run("helper", r#"Error: agent "helper" not found"#, "q\n");
        assert_eq!(log, "attach helper \nstatus helper \n");
        assert_eq!(out.matches(&gone_note("helper")).count(), 1, "{out}");
        assert!(!out.contains(retry));

        // The terminal going away (EOF) closes it too.
        let (out, log) = run("helper", r#"Error: agent "helper" not found"#, "");
        assert_eq!(log.matches("attach").count(), 1);
        assert!(out.contains(&gone_note("helper")));

        // Anything else can pass, so Enter tries again: the bus down, the session gone
        // until `agent resume`, and a not-found that names ANOTHER agent.
        for says in [
            "Error: asking workstation where its agent binary is: the bus is not running",
            "Error: helper has ended (quit): its session agent-helper is gone, so there is nothing to attach to. agent resume helper --attach starts it again",
            r#"Error: agent "helper-2" not found on host workstation"#,
        ] {
            let (out, log) = run("helper@workstation", says, "\n");
            assert_eq!(log.matches("attach helper@workstation").count(), 2, "{says}: {log}");
            assert_eq!(out.matches(retry).count(), 2, "{says}: {out}");
            assert!(!out.contains("is not an agent any more"), "{says}: {out}");
        }
    }

    #[test]
    fn store_remembers_attached_agent_per_scope_and_forgets_with_terminal() {
        let dir = ScratchDir::new("store-attach");
        let mut s = WallStore::open(&dir.0, "8099");
        s.set_attached("term-1", Some("helper"));
        s.set_attached("term-2", Some("far@workstation"));
        assert_eq!(
            WallStore::open(&dir.0, "8099").attached("term-2"),
            Some("far@workstation")
        );
        assert_eq!(WallStore::open(&dir.0, "8093").attached("term-2"), None);
        assert_eq!(s.attached_to("helper"), vec!["term-1"]);
        s.forget("term-1");
        assert_eq!(s.attached("term-1"), None);
        assert_eq!(s.prune([] as [&str; 0]), vec!["term-2"]);
        assert!(WallStore::open(&dir.0, "8099")
            .attached_to("far@workstation")
            .is_empty());
    }

    #[test]
    fn local_bus_name_and_find_bus_agent_distinguish_local_and_remote_targets() {
        let snap = GomuxSnapshot {
            gen: 1,
            host: Some("laptop-2".into()),
            dirs: Vec::new(),
            agents: vec![
                GomuxAgent {
                    host: Some("laptop-2".into()),
                    session: "agent-wall-m2-agy".into(),
                    namespace: Some("agents".into()),
                    ..make_agent("wall-m2-agy")
                },
                GomuxAgent {
                    host: Some("workstation".into()),
                    session: "agent-glyph3d-lead".into(),
                    ..make_agent("glyph3d-lead")
                },
            ],
        };
        assert_eq!(
            local_bus_name("wall-m2-agy", Some(&snap)),
            Some("wall-m2-agy")
        );
        assert_eq!(
            local_bus_name("wall-m2-agy@laptop-2", Some(&snap)),
            Some("wall-m2-agy")
        );
        assert_eq!(local_bus_name("glyph3d-lead@workstation", Some(&snap)), None);
        assert_eq!(local_bus_name("glyph3d-lead@workstation", None), None);
        assert_eq!(gomux_screen_id("wall-m2-agy"), "gx-wall-m2-agy");

        assert_eq!(
            find_bus_agent(Some(&snap), "wall-m2-agy")
                .unwrap()
                .name,
            "wall-m2-agy"
        );
        assert_eq!(
            find_bus_agent(Some(&snap), "wall-m2-agy@laptop-2")
                .unwrap()
                .name,
            "wall-m2-agy"
        );
        assert_eq!(
            find_bus_agent(Some(&snap), "glyph3d-lead@workstation")
                .unwrap()
                .name,
            "glyph3d-lead"
        );
        assert!(find_bus_agent(Some(&snap), "glyph3d-lead").is_none());
    }
}
