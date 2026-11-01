//! What a tree holds, by layer.
//!
//! Four layers, and a path can sit in more than one: `notes.r` is code AND
//! an extension-named file, `disk.img` is a raw binary AND an
//! extension-named file, and a directory is only a directory. So a path's
//! classification is a [`LayerSet`], and [`walk`] files each path under every
//! layer it belongs to.
//!
//! Code is known by extension (the table below). Binary is known by content,
//! never by extension: a NUL byte or invalid UTF-8 in a file's first
//! [`SNIFF_BYTES`] bytes. Neither is a guarantee (a UTF-16 text file reads as
//! binary, a `.txt` that holds Rust stays text), which is why the
//! extension-named layer keeps what the name claims beside what the bytes
//! showed.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use serde::Serialize;

/// How much of a file the binary sniff reads.
pub const SNIFF_BYTES: usize = 8192;

/// Directory names [`walk`] does not descend into: version control and
/// build output, which would bury what the tree is about.
pub const SKIPPED_DIRECTORIES: &[&str] = &[".git", "target", "node_modules", ".pixi", ".venv", "__pycache__"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    Code,
    Binary,
    Directory,
    Named,
}

impl Layer {
    pub const ALL: [Layer; 4] = [Layer::Code, Layer::Binary, Layer::Directory, Layer::Named];

    pub fn name(self) -> &'static str {
        match self {
            Layer::Code => "code",
            Layer::Binary => "binary",
            Layer::Directory => "directory",
            Layer::Named => "named",
        }
    }
}

/// Every layer one path belongs to.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct LayerSet {
    /// The language, when the path is code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<&'static str>,
    pub binary: bool,
    pub directory: bool,
    /// The extension the name claims (lowercased, no dot).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extension: Option<String>,
}

impl LayerSet {
    pub fn layers(&self) -> Vec<Layer> {
        let mut layers = Vec::new();
        if self.code.is_some() {
            layers.push(Layer::Code);
        }
        if self.binary {
            layers.push(Layer::Binary);
        }
        if self.directory {
            layers.push(Layer::Directory);
        }
        if self.extension.is_some() {
            layers.push(Layer::Named);
        }
        layers
    }
}

/// The language a source extension names, or None.
pub fn language_of(extension: &str) -> Option<&'static str> {
    Some(match extension {
        "rs" => "rust",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "c++",
        "m" | "mm" => "objective-c",
        "swift" => "swift",
        "metal" => "metal",
        "wgsl" => "wgsl",
        "glsl" | "vert" | "frag" | "comp" => "glsl",
        "hlsl" => "hlsl",
        "go" => "go",
        "py" => "python",
        "mojo" | "🔥" => "mojo",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" | "mts" | "tsx" | "jsx" => "typescript",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "rb" => "ruby",
        "r" => "r",
        "jl" => "julia",
        "lua" => "lua",
        "zig" => "zig",
        "hs" => "haskell",
        "ml" | "mli" => "ocaml",
        "ex" | "exs" => "elixir",
        "erl" => "erlang",
        "cs" => "c#",
        "fs" => "f#",
        "scala" => "scala",
        "php" => "php",
        "pl" | "pm" => "perl",
        "sh" | "bash" | "zsh" | "fish" => "shell",
        "sql" => "sql",
        _ => return None,
    })
}

/// The bytes are not text: a NUL, or a UTF-8 error before the sniffed
/// prefix's end (a char cut by the prefix's end is not an error).
pub fn looks_binary(prefix: &[u8], whole_file: bool) -> bool {
    if prefix.contains(&0) {
        return true;
    }
    match std::str::from_utf8(prefix) {
        Ok(_) => false,
        Err(error) => whole_file || error.error_len().is_some(),
    }
}

/// Classify one path. Reads up to [`SNIFF_BYTES`] of a file; a file that
/// cannot be read is classified by its name alone.
pub fn classify(path: &Path) -> Result<LayerSet, String> {
    let metadata = fs::metadata(path).map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
    if metadata.is_dir() {
        return Ok(LayerSet { directory: true, ..Default::default() });
    }
    let extension = path.extension().and_then(|e| e.to_str()).map(str::to_lowercase);
    let code = extension.as_deref().and_then(language_of);
    let binary = match fs::File::open(path) {
        Ok(file) => {
            let mut prefix = Vec::with_capacity(SNIFF_BYTES);
            file.take(SNIFF_BYTES as u64).read_to_end(&mut prefix).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            looks_binary(&prefix, (prefix.len() as u64) == metadata.len())
        }
        Err(_) => false,
    };
    Ok(LayerSet { code, binary, directory: false, extension })
}

/// One classified path of a walk, relative to the walk's root.
#[derive(Clone, Debug, Serialize)]
pub struct Classified {
    pub path: PathBuf,
    pub layers: LayerSet,
}

/// Every path under `root` (not `root` itself), classified, depth-first in
/// name order, skipping [`SKIPPED_DIRECTORIES`] and symlinks.
pub fn walk(root: &Path) -> Result<Vec<Classified>, String> {
    let mut out = Vec::new();
    walk_into(root, root, &mut out)?;
    Ok(out)
}

fn walk_into(root: &Path, directory: &Path, out: &mut Vec<Classified>) -> Result<(), String> {
    let mut entries: Vec<fs::DirEntry> = fs::read_dir(directory)
        .map_err(|e| format!("cannot list {}: {e}", directory.display()))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("cannot list {}: {e}", directory.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let kind = entry.file_type().map_err(|e| format!("cannot stat {}: {e}", entry.path().display()))?;
        if kind.is_symlink() {
            continue;
        }
        let path = entry.path();
        if kind.is_dir() && SKIPPED_DIRECTORIES.iter().any(|skip| entry.file_name() == *skip) {
            continue;
        }
        let layers = classify(&path)?;
        let relative = path.strip_prefix(root).expect("a walked path is under its root").to_path_buf();
        out.push(Classified { path: relative, layers });
        if kind.is_dir() {
            walk_into(root, &path, out)?;
        }
    }
    Ok(())
}

/// A walk filed by layer: each layer's paths, in walk order.
pub fn by_layer(classified: &[Classified]) -> BTreeMap<Layer, Vec<&Classified>> {
    let mut map: BTreeMap<Layer, Vec<&Classified>> = Layer::ALL.iter().map(|&layer| (layer, Vec::new())).collect();
    for item in classified {
        for layer in item.layers.layers() {
            map.get_mut(&layer).expect("every layer is in the map").push(item);
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::ScratchDir;

    #[test]
    fn a_path_sits_in_every_layer_it_belongs_to() {
        let scratch = ScratchDir::new("layers");
        let root = &scratch.0;
        fs::create_dir_all(root.join("src/deep")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".git/HEAD"), "ref").unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("notes.R"), "x <- 1").unwrap();
        fs::write(root.join("disk.img"), [0u8, 1, 2, 3]).unwrap();
        fs::write(root.join("README"), "plain").unwrap();
        fs::write(root.join("blob"), [0xffu8, 0xfe, 0x41]).unwrap();

        let walked = walk(root).unwrap();
        let find = |path: &str| walked.iter().find(|c| c.path == Path::new(path)).unwrap_or_else(|| panic!("{path} not walked")).layers.clone();
        assert_eq!(find("src").layers(), [Layer::Directory]);
        assert_eq!(find("src/main.rs").layers(), [Layer::Code, Layer::Named]);
        assert_eq!(find("notes.R").code, Some("r"), "extensions are matched lowercased");
        assert_eq!(find("disk.img").layers(), [Layer::Binary, Layer::Named]);
        assert_eq!(find("README").layers(), Vec::<Layer>::new());
        assert_eq!(find("blob").layers(), [Layer::Binary]);
        assert!(walked.iter().all(|c| !c.path.starts_with(".git")), "version control is skipped");

        let map = by_layer(&walked);
        assert_eq!(map[&Layer::Named].len(), 3);
        assert_eq!(map[&Layer::Directory].len(), 2);
    }

    #[test]
    fn a_char_cut_by_the_sniff_is_not_binary() {
        let text = "é".repeat(10);
        let cut = &text.as_bytes()[..5];
        assert!(!looks_binary(cut, false));
        assert!(looks_binary(cut, true), "the same cut at the file's end is invalid UTF-8");
        assert!(looks_binary(b"a\0b", false));
    }
}
