//! Repository filesystem walking and file filtering.

use std::path::{Path, PathBuf};

/// Per-file read cap. Doubles as the ordinal-wall guard (2^24 B = 16 MiB).
pub const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

pub const SKIP_DIRS: &[&str] = &[
    ".git", ".svn", ".hg", "node_modules", "target", ".pixi", "dist", "build", "out",
    ".next", ".nuxt", ".cache", "coverage", "__pycache__", ".idea", ".vscode",
    ".turbo", ".vercel", "tmp", "temp",
];

pub const SOURCE_EXTENSIONS: &[&str] = &[
    "rs", "js", "jsx", "ts", "tsx", "mjs", "cjs", "go", "py", "mojo", "c", "h", "cpp",
    "hpp", "cc", "hh", "cs", "java", "kt", "swift", "rb", "md", "markdown", "json",
    "jsonc", "toml", "yaml", "yml", "xml", "wgsl", "glsl", "vert", "frag", "metal",
    "css", "scss", "html", "htm", "sh", "bash", "zsh", "fish", "lock", "txt", "sql",
    "lua", "zig", "ex", "exs", "hs", "ml", "clj", "scala", "php", "pl", "r", "jl",
    "nim", "d", "vue", "svelte",
];

/// One walked source file.
#[derive(Clone, Debug)]
pub struct RepoFile {
    pub rel_path: String,
    /// Parent directory (relative, "" at the root) — the group-tint key.
    pub dir: String,
    pub bytes: Vec<u8>,
    pub newline_count: usize,
}

#[derive(Clone, Debug)]
pub struct WalkResult {
    pub files: Vec<RepoFile>,
    pub total_bytes: usize,
    pub skipped_large: usize,
    pub skipped_non_utf8: usize,
    pub dirs_visited: usize,
    /// The walk's own wall time, measured where the walk happens. The load's
    /// `walk` stat reads THIS — the old span inside `load_repo_from_walk`
    /// timed nothing (the walk arrives already done), which is why it printed
    /// 0.000s on corpora that take real milliseconds to read.
    pub walk_dur: std::time::Duration,
}

impl WalkResult {
    /// The in-memory walk: content the CALLER owns (P1-live — the seam's
    /// envelope bytes), not a directory. There are no skip semantics to
    /// report — the caller already decided what exists — so the counters are
    /// zero and `dirs_visited` is 1 (the notional root).
    pub fn from_files(files: Vec<RepoFile>) -> WalkResult {
        let total_bytes = files.iter().map(|f| f.bytes.len()).sum();
        WalkResult {
            files,
            total_bytes,
            skipped_large: 0,
            skipped_non_utf8: 0,
            dirs_visited: 1,
            walk_dur: std::time::Duration::ZERO,
        }
    }
}

impl RepoFile {
    /// An in-memory file: `rel_path` as it would appear under a repo root
    /// (the dir-tint key is its parent, "" at the root — same rule the
    /// walker derives).
    pub fn in_memory(rel_path: impl Into<String>, bytes: Vec<u8>) -> RepoFile {
        let rel_path = rel_path.into();
        let dir = rel_path
            .rfind('/')
            .map(|i| rel_path[..i].to_string())
            .unwrap_or_default();
        let newline_count = memchr::memchr_iter(b'\n', &bytes).count();
        RepoFile { rel_path, dir, bytes, newline_count }
    }
}

/// Recursive walk, deterministic order (files sorted by relative path).
pub fn walk_repo(root: &Path) -> WalkResult {
    let _sp = tracing::info_span!("repo.walk", root = %root.display()).entered();
    let t0 = std::time::Instant::now();
    let mut candidates: Vec<(String, PathBuf)> = Vec::new();
    let mut dirs_visited = 0usize;
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        dirs_visited += 1;
        for entry in rd.flatten() {
            let file_name_os = entry.file_name();
            let Some(name) = file_name_os.to_str() else {
                continue;
            };
            if name.starts_with('.') && name != ".github" {
                continue;
            }
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_dir() {
                if SKIP_DIRS.contains(&name) {
                    continue;
                }
                stack.push(entry.path());
            } else if ft.is_file() {
                let Some((_, ext)) = name.rsplit_once('.') else {
                    continue;
                };
                let ext_lower = ext.to_ascii_lowercase();
                if !SOURCE_EXTENSIONS.contains(&ext_lower.as_str()) {
                    continue;
                }
                let path = entry.path();
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                candidates.push((rel, path));
            }
        }
    }
    candidates.sort_by(|a, b| a.0.cmp(&b.0));

    use rayon::prelude::*;

    enum CandidateReadResult {
        File(RepoFile),
        SkippedLarge,
        SkippedNonUtf8,
        Error,
    }

    let read_results: Vec<CandidateReadResult> = candidates
        .into_par_iter()
        .map(|(rel, path)| {
            let meta = match std::fs::metadata(&path) {
                Ok(m) => m,
                Err(_) => return CandidateReadResult::Error,
            };
            if meta.len() > MAX_FILE_BYTES {
                return CandidateReadResult::SkippedLarge;
            }
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(_) => return CandidateReadResult::Error,
            };
            if simdutf8::basic::from_utf8(&bytes).is_err() {
                return CandidateReadResult::SkippedNonUtf8;
            }
            let newline_count = memchr::memchr_iter(b'\n', &bytes).count();
            let dir = rel
                .rsplit_once('/')
                .map(|(d, _)| d.to_string())
                .unwrap_or_default();
            CandidateReadResult::File(RepoFile {
                rel_path: rel,
                dir,
                bytes,
                newline_count,
            })
        })
        .collect();

    let mut files = Vec::with_capacity(read_results.len());
    let mut total_bytes = 0usize;
    let mut skipped_large = 0usize;
    let mut skipped_non_utf8 = 0usize;
    for res in read_results {
        match res {
            CandidateReadResult::File(f) => {
                total_bytes += f.bytes.len();
                files.push(f);
            }
            CandidateReadResult::SkippedLarge => {
                skipped_large += 1;
            }
            CandidateReadResult::SkippedNonUtf8 => {
                skipped_non_utf8 += 1;
            }
            CandidateReadResult::Error => {}
        }
    }
    WalkResult {
        files,
        total_bytes,
        skipped_large,
        skipped_non_utf8,
        dirs_visited,
        walk_dur: t0.elapsed(),
    }
}
