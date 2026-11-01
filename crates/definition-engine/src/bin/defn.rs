//! `defn`: the definition engine's command line, the reader's and the
//! agent's door onto the same store.
//!
//! The loop, by hand:
//!
//! ```text
//! defn new "Letter" --file letter.txt      # a run; prints its id
//! defn expand letter --word ephemeral      # a request; prints q1
//! defn pending --json                      # the agent's queue
//! defn answer letter q1 '{"substitutes": ["fleeting"]}'
//! defn options letter q1                   # the answers, numbered from 1
//! defn substitute letter q1 --pick 1       # a step: the next version
//! defn log letter                          # the diff stack, as cards
//! ```
//!
//! Every run argument takes a unique prefix of a run id. Every number a
//! reader types (`--pick`, `--nth`, `--definition`) counts from 1; byte
//! offsets (`--at`, `--span`) count from 0, as a pick reports them.
//! `--json` prints what an agent parses; without it, what a reader reads.

use std::io::Read as _;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{ArgGroup, Args, Parser, Subcommand};
use definition_engine::layers::{self, Layer};
use definition_engine::text::{find_word, word_at};
use definition_engine::{now_seconds, Choice, Expansion, Source, Span, Store};

#[derive(Parser)]
#[command(name = "defn", about = "The definition engine: select a word, expand it, substitute, recurse.")]
struct Cli {
    /// The store directory (default: $DEFN_STORE, else
    /// $XDG_DATA_HOME/glyph3d/definition-runs).
    #[arg(long, global = true)]
    store: Option<PathBuf>,
    /// Print JSON for a program to parse.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start a run from a passage.
    New {
        title: String,
        #[arg(long, conflicts_with = "file", required_unless_present = "file")]
        text: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Every run in the store.
    List,
    /// A run's text at its head, or at --at.
    Show {
        run: String,
        #[arg(long)]
        at: Option<usize>,
    },
    /// Select a word in the head text and ask for its expansion.
    #[command(group(ArgGroup::new("selection").required(true)))]
    Expand {
        run: String,
        /// The word, matched whole.
        #[arg(long, group = "selection")]
        word: Option<String>,
        /// With --word: which occurrence, from 1 (default 1). Refused beside
        /// the other selections by conflict, not `requires = "word"`: clap
        /// 4.6 accepted `--at 0 --nth 2` under that (measured, with and
        /// without a default), and one selection is required anyway.
        #[arg(long, conflicts_with_all = ["at", "span"])]
        nth: Option<usize>,
        /// The word around this byte offset (a tap on one glyph).
        #[arg(long, group = "selection")]
        at: Option<usize>,
        /// An exact byte range.
        #[arg(long, num_args = 2, value_names = ["START", "END"], group = "selection")]
        span: Option<Vec<usize>>,
    },
    /// Requests no agent has answered, across every run.
    Pending {
        /// Block until there is at least one, then print them and exit:
        /// run it in the background and the agent wakes when a reader asks.
        #[arg(long)]
        wait: bool,
        /// With --wait: give up after this many seconds and print none.
        #[arg(long, requires = "wait")]
        timeout: Option<f64>,
    },
    /// Answer a request with an expansion's JSON (inline, else stdin).
    Answer { run: String, request: String, expansion: Option<String> },
    /// A request's answers, numbered from 1.
    Options { run: String, request: String },
    /// Put one answer (or --text) in the selected word's place.
    Substitute {
        run: String,
        request: String,
        #[command(flatten)]
        choice: ChoiceArgs,
    },
    /// The diff stack, oldest first, one card per step.
    Log { run: String },
    /// Read another version; later versions are kept.
    Checkout { run: String, version: usize },
    /// Copy a run's history up to a version into a run of its own.
    Fork {
        run: String,
        /// The version to fork at (default: the head).
        #[arg(long)]
        at: Option<usize>,
        #[arg(long)]
        title: Option<String>,
    },
    /// Classify a tree by layer: code, binary, directory, named.
    Layers {
        root: PathBuf,
        /// List every path under its layers, not just the counts.
        #[arg(long)]
        all: bool,
    },
}

#[derive(Args)]
#[group(required = true, multiple = false)]
struct ChoiceArgs {
    /// A substitute, numbered from 1.
    #[arg(long)]
    pick: Option<usize>,
    /// A definition, numbered from 1.
    #[arg(long)]
    definition: Option<usize>,
    /// A translation, numbered from 1.
    #[arg(long)]
    translation: Option<usize>,
    /// Your own text.
    #[arg(long)]
    text: Option<String>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("defn: {error}");
            ExitCode::FAILURE
        }
    }
}

fn print_json(value: &impl serde::Serialize) {
    println!("{}", serde_json::to_string_pretty(value).expect("output serializes"));
}

/// A 1-based number from the command line as a 0-based index.
fn index_from(number: usize, what: &str) -> Result<usize, String> {
    number.checked_sub(1).ok_or_else(|| format!("{what} counts from 1"))
}

fn run(cli: Cli) -> Result<(), String> {
    let root = match cli.store {
        Some(root) => root,
        None => Store::default_root()?,
    };
    let store = Store::open(root)?;
    let json = cli.json;
    match cli.command {
        Command::New { title, text, file } => {
            let (source, base) = match (text, file) {
                (Some(text), None) => (Source::Inline, text),
                (None, Some(path)) => {
                    let base = std::fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
                    (Source::File { path: path.display().to_string() }, base)
                }
                _ => unreachable!("clap requires exactly one of --text and --file"),
            };
            let run = store.create(&title, source, base)?;
            if json {
                print_json(&serde_json::json!({ "run": run.id }));
            } else {
                println!("{}", run.id);
            }
        }
        Command::List => {
            let summaries = store.summaries()?;
            if json {
                print_json(&summaries);
            } else {
                for s in summaries {
                    println!("{}  v{}/{}  {} pending  {}", s.id, s.head, s.tip, s.pending, s.title);
                }
            }
        }
        Command::Show { run, at } => {
            let run = store.load(&run)?;
            let version = at.unwrap_or(run.head);
            let text = run.text_at(version)?;
            if json {
                print_json(&serde_json::json!({ "run": run.id, "version": version, "tip": run.tip(), "text": text }));
            } else {
                print!("{text}");
                if !text.ends_with('\n') {
                    println!();
                }
            }
        }
        Command::Expand { run, word, nth, at, span } => {
            let mut run = store.load(&run)?;
            let text = run.head_text();
            let span = match (word, at, span) {
                (Some(word), None, None) => {
                    let nth = nth.unwrap_or(1);
                    find_word(&text, &word, nth).ok_or_else(|| format!("version {} has no whole word {word:?} (occurrence {nth})", run.head))?
                }
                (None, Some(at), None) => word_at(&text, at).ok_or_else(|| format!("byte {at} of version {} is not inside a word", run.head))?,
                (None, None, Some(span)) => Span::checked(&text, span[0], span[1])?,
                _ => unreachable!("clap requires exactly one selection"),
            };
            let request = run.expand(span, now_seconds())?.clone();
            store.save(&run)?;
            if json {
                print_json(&serde_json::json!({ "run": run.id, "request": request }));
            } else {
                println!("{}  {:?}  in: {}", request.id, request.word, request.marked_context());
            }
        }
        Command::Pending { wait, timeout } => {
            let pending = if wait {
                let timeout = timeout.map(std::time::Duration::from_secs_f64);
                store.wait_for_pending(timeout, definition_engine::store::WAIT_POLL)?
            } else {
                store.pending()?
            };
            if json {
                let items: Vec<_> = pending
                    .iter()
                    .map(|(run, r)| {
                        serde_json::json!({
                            "run": run,
                            "request": r.id,
                            "word": r.word,
                            "marked": r.marked_context(),
                            "version": r.version,
                            "span": r.span,
                        })
                    })
                    .collect();
                print_json(&items);
            } else {
                for (run, r) in pending {
                    println!("{run} {}  {:?}  in: {}", r.id, r.word, r.marked_context());
                }
            }
        }
        Command::Answer { run, request, expansion: body } => {
            let body = match body {
                Some(body) => body,
                None => {
                    let mut body = String::new();
                    std::io::stdin().read_to_string(&mut body).map_err(|e| format!("cannot read stdin: {e}"))?;
                    body
                }
            };
            let expansion: Expansion = serde_json::from_str(&body).map_err(|e| format!("the expansion is not valid: {e}"))?;
            let mut run = store.load(&run)?;
            run.answer(&request, expansion, now_seconds())?;
            store.save(&run)?;
            if json {
                print_json(&serde_json::json!({ "run": run.id, "request": request, "answered": true }));
            } else {
                println!("{request} answered");
            }
        }
        Command::Options { run, request } => {
            let run = store.load(&run)?;
            let request = run.request(&request)?;
            if json {
                print_json(request);
                return Ok(());
            }
            println!("{}  {:?}  in: {}", request.id, request.word, request.marked_context());
            let Some(expansion) = &request.expansion else {
                println!("  (not answered yet)");
                return Ok(());
            };
            print_options(expansion);
            if let Some(step) = &request.applied {
                println!("  taken by {step}");
            }
        }
        Command::Substitute { run, request, choice } => {
            let choice = match (choice.pick, choice.definition, choice.translation, choice.text) {
                (Some(n), None, None, None) => Choice::Substitute { index: index_from(n, "--pick")? },
                (None, Some(n), None, None) => Choice::Definition { index: index_from(n, "--definition")? },
                (None, None, Some(n), None) => Choice::Translation { index: index_from(n, "--translation")? },
                (None, None, None, Some(text)) => Choice::Text { text },
                _ => unreachable!("clap requires exactly one choice"),
            };
            let mut run = store.load(&run)?;
            let step = run.substitute(&request, choice, now_seconds())?.clone();
            store.save(&run)?;
            if json {
                print_json(&serde_json::json!({ "run": run.id, "step": step, "version": run.head, "text": run.head_text() }));
            } else {
                println!("{}  v{}  {:?} -> {:?}", step.id, run.head, step.removed, step.inserted);
            }
        }
        Command::Log { run } => {
            let run = store.load(&run)?;
            if json {
                print_json(&run.steps);
                return Ok(());
            }
            for (version, step) in run.steps.iter().enumerate() {
                let marker = if version + 1 == run.head { "*" } else { " " };
                println!("{marker} v{}  {} ({})  {}", version + 1, step.id, step.request, step.card(&run.text_at(version)?));
            }
            if run.head == 0 {
                println!("* v0  (base)");
            }
        }
        Command::Checkout { run, version } => {
            let mut run = store.load(&run)?;
            run.checkout(version)?;
            store.save(&run)?;
            if json {
                print_json(&serde_json::json!({ "run": run.id, "version": run.head, "tip": run.tip() }));
            } else {
                println!("{} at v{} of {}", run.id, run.head, run.tip());
            }
        }
        Command::Fork { run, at, title } => {
            let parent = store.load(&run)?;
            let version = at.unwrap_or(parent.head);
            let title = title.unwrap_or_else(|| format!("{} (fork at v{version})", parent.title));
            let fork = parent.fork(version, Store::new_id(&title), title, now_seconds())?;
            store.write_new(&fork)?;
            if json {
                print_json(&serde_json::json!({ "run": fork.id, "from": parent.id, "version": version }));
            } else {
                println!("{}", fork.id);
            }
        }
        Command::Layers { root, all } => {
            let walked = layers::walk(&root)?;
            if json {
                print_json(&walked);
                return Ok(());
            }
            let map = layers::by_layer(&walked);
            for layer in Layer::ALL {
                let items = &map[&layer];
                println!("{:<9} {}", layer.name(), items.len());
                if all {
                    for item in items {
                        let note = match (layer, item.layers.code, &item.layers.extension) {
                            (Layer::Code, Some(language), _) => format!("  ({language})"),
                            (Layer::Named, _, Some(extension)) => format!("  (.{extension})"),
                            _ => String::new(),
                        };
                        println!("  {}{note}", item.path.display());
                    }
                }
            }
        }
    }
    Ok(())
}

fn print_options(expansion: &Expansion) {
    if !expansion.substitutes.is_empty() {
        println!("  substitutes (--pick N):");
        for (i, s) in expansion.substitutes.iter().enumerate() {
            println!("    {}. {s}", i + 1);
        }
    }
    if !expansion.definitions.is_empty() {
        println!("  definitions (--definition N):");
        for (i, d) in expansion.definitions.iter().enumerate() {
            match &d.part_of_speech {
                Some(pos) => println!("    {}. ({pos}) {}", i + 1, d.text),
                None => println!("    {}. {}", i + 1, d.text),
            }
        }
    }
    if !expansion.translations.is_empty() {
        println!("  translations (--translation N):");
        for (i, t) in expansion.translations.iter().enumerate() {
            println!("    {}. [{}] {}", i + 1, t.language, t.text);
        }
    }
    if !expansion.related.is_empty() {
        println!("  related:");
        for t in &expansion.related {
            println!("    - {} ({})", t.name, t.category);
        }
    }
}
