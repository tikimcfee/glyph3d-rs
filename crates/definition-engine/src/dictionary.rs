//! Static dictionaries: definitions that need no agent.
//!
//! A dictionary is a JSON object of `{headword: definition text}`, lowercase
//! headwords (the shape of the Gutenberg Webster's 1913 JSON in glyph3d-js's
//! word-wall example and viz's test data, 102,217 entries). A store reads
//! every `*.json` under `<store>/dictionaries/` and every path in
//! `$DEFN_DICTIONARY` (`:`-separated), in that order; the first dictionary
//! that knows a word answers for it. The data stays where it lives: a store
//! links or copies it in, and this tree vendors none of it.
//!
//! When a reader selects a word the dictionary's senses are attached to the
//! request at once ([`crate::Run::expand_with`]), so the options panel has
//! something to take before any agent has answered; an agent, when one is
//! running, still sees the request and adds what a dictionary cannot (the
//! sense in context, substitutes that fit the slot, translations, the repo's
//! meaning of a word). A 1913 dictionary has no "software" and a "glyph" that
//! is a groove in a frieze, which is why both sources stay.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The environment variable naming dictionary files beyond the store's own.
pub const DICTIONARY_ENV: &str = "DEFN_DICTIONARY";

/// The longest definition text taken to be a bare cross-reference ("Same as
/// Programme.") and followed to its target.
const CROSS_REFERENCE_MAX_BYTES: usize = 60;

/// A dictionary's answer for one selected word, kept on its request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DictionaryEntry {
    /// Which dictionary answered (its file's stem).
    pub source: String,
    /// The headword found: the word itself, or the form an inflection
    /// fallback reached ("codes" finds "code").
    pub headword: String,
    /// The definition text split at its numbered senses, in order (text
    /// before "1." is a sense of its own); a followed cross-reference's
    /// senses come after.
    pub senses: Vec<String>,
}

pub struct Dictionary {
    name: String,
    entries: HashMap<String, String>,
}

impl Dictionary {
    pub fn from_entries(name: impl Into<String>, entries: impl IntoIterator<Item = (String, String)>) -> Dictionary {
        let entries = entries.into_iter().map(|(word, text)| (word.to_lowercase(), text)).collect();
        Dictionary { name: name.into(), entries }
    }

    /// A `{word: text}` JSON file, named by its stem.
    pub fn load(path: &Path) -> Result<Dictionary, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("cannot read dictionary {}: {e}", path.display()))?;
        let entries: HashMap<String, String> =
            serde_json::from_slice(&bytes).map_err(|e| format!("dictionary {} is not a {{word: text}} JSON object: {e}", path.display()))?;
        let name = path.file_stem().map_or_else(|| path.display().to_string(), |s| s.to_string_lossy().into_owned());
        Ok(Dictionary::from_entries(name, entries))
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entry for `word`: the word lowercased, else the first inflection
    /// fallback this dictionary has.
    pub fn lookup(&self, word: &str) -> Option<DictionaryEntry> {
        let (headword, text) = candidates(word).into_iter().find_map(|c| self.entries.get(&c).map(|text| (c, text)))?;
        let mut senses = split_senses(text);
        if let Some(target) = cross_reference(text) {
            if target != headword {
                if let Some(more) = self.entries.get(&target) {
                    senses.extend(split_senses(more));
                }
            }
        }
        Some(DictionaryEntry { source: self.name.clone(), headword, senses })
    }
}

/// Every dictionary a store reads, in the order they are asked.
#[derive(Default)]
pub struct Dictionaries {
    list: Vec<Dictionary>,
}

impl Dictionaries {
    pub fn new(list: Vec<Dictionary>) -> Dictionaries {
        Dictionaries { list }
    }

    /// The `*.json` files under `dir` (sorted by name; a missing dir is no
    /// dictionaries), then each path in `$DEFN_DICTIONARY`.
    pub fn load(dir: &Path) -> Result<Dictionaries, String> {
        let mut paths: Vec<PathBuf> = match std::fs::read_dir(dir) {
            Ok(entries) => entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "json"))
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(format!("cannot list {}: {e}", dir.display())),
        };
        paths.sort();
        if let Some(extra) = std::env::var_os(DICTIONARY_ENV) {
            paths.extend(std::env::split_paths(&extra).filter(|p| !p.as_os_str().is_empty()));
        }
        Ok(Dictionaries { list: paths.iter().map(|p| Dictionary::load(p)).collect::<Result<_, _>>()? })
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub fn names(&self) -> Vec<&str> {
        self.list.iter().map(Dictionary::name).collect()
    }

    /// The first dictionary's entry for `word`.
    pub fn lookup(&self, word: &str) -> Option<DictionaryEntry> {
        self.list.iter().find_map(|d| d.lookup(word))
    }
}

/// The headwords to try for `word`, in order: itself lowercased, then
/// common English inflections taken off (plural, past, gerund, adverb,
/// possessive). Crude by design: a miss costs nothing, the agent remains.
fn candidates(word: &str) -> Vec<String> {
    let w = word.to_lowercase();
    let w = w.trim_end_matches("'s").trim_end_matches("\u{2019}s").to_string();
    let mut out = vec![w.clone()];
    let mut push = |s: String| {
        if s.chars().count() >= 2 && !out.contains(&s) {
            out.push(s);
        }
    };
    if let Some(stem) = w.strip_suffix("ies") {
        push(format!("{stem}y"));
    }
    if let Some(stem) = w.strip_suffix("es") {
        push(stem.to_string());
    }
    if let Some(stem) = w.strip_suffix('s') {
        push(stem.to_string());
    }
    if let Some(stem) = w.strip_suffix("ied") {
        push(format!("{stem}y"));
    }
    if let Some(stem) = w.strip_suffix("ed") {
        push(stem.to_string());
        push(format!("{stem}e"));
    }
    if let Some(stem) = w.strip_suffix("ing") {
        push(stem.to_string());
        push(format!("{stem}e"));
    }
    if let Some(stem) = w.strip_suffix("ly") {
        push(stem.to_string());
    }
    out
}

/// A definition text split at its numbered senses: "1. ", "2. ", ... in
/// order, each at the text's start or after whitespace. Numbers out of
/// sequence stay inside their sense, so "see 3. above" does not split.
/// Each sense's whitespace runs collapse to one space: a sense is one line
/// of the options panel (a tap maps back by line) and is inserted as shown.
fn split_senses(text: &str) -> Vec<String> {
    let mut cuts = Vec::new();
    let mut next = 1;
    let mut from = 0;
    while let Some(at) = find_marker(text, next, from) {
        cuts.push(at);
        from = at + 1;
        next += 1;
    }
    let mut senses = Vec::new();
    let head_end = cuts.first().copied().unwrap_or(text.len());
    let one_line = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let head = one_line(&text[..head_end]);
    if !head.is_empty() {
        senses.push(head);
    }
    for (index, &at) in cuts.iter().enumerate() {
        let end = cuts.get(index + 1).copied().unwrap_or(text.len());
        let marker = format!("{}. ", index + 1);
        let sense = one_line(&text[at + marker.len()..end]);
        if !sense.is_empty() {
            senses.push(sense);
        }
    }
    senses
}

/// Where the marker `"{number}. "` starts, at or after `from`, at the
/// text's start or after whitespace.
fn find_marker(text: &str, number: usize, from: usize) -> Option<usize> {
    let marker = format!("{number}. ");
    let mut search = from;
    while let Some(found) = text.get(search..)?.find(&marker) {
        let at = search + found;
        if at == 0 || text[..at].ends_with(char::is_whitespace) {
            return Some(at);
        }
        search = at + 1;
    }
    None
}

/// The target of a bare cross-reference ("Same as Programme." or "See
/// Triglyph."), lowercased; None for any longer definition.
fn cross_reference(text: &str) -> Option<String> {
    let text = text.trim();
    if text.len() > CROSS_REFERENCE_MAX_BYTES {
        return None;
    }
    let rest = text.strip_prefix("Same as ").or_else(|| text.strip_prefix("See "))?;
    let target: String = rest.chars().take_while(|c| c.is_alphabetic() || *c == '-').collect();
    (!target.is_empty()).then(|| target.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn webster() -> Dictionary {
        Dictionary::from_entries(
            "webster",
            [
                ("code", "1. A body of law, sanctioned by legislation. 2. Any system of rules or regulations relating to one subject."),
                ("create", "Created; composed. [Obs.]\n\n1. To bring into being; to form out of nothing. 2. To cause; to produce."),
                ("program", "Same as Programme."),
                ("programme", "1. A brief outline of the order to be pursued. 2. A preliminary advertisement."),
                ("provide", "1. To look out for in advance; to procure beforehand. 2. To furnish; to supply."),
                ("study", "1. A setting of the mind upon a subject; see 3. below. 2. A building or room for study."),
                ("quickly", "In a quick manner."),
            ]
            .map(|(w, t)| (w.to_string(), t.to_string())),
        )
    }

    #[test]
    fn senses_split_at_their_numbers_in_order() {
        let entry = webster().lookup("create").unwrap();
        assert_eq!(entry.senses, ["Created; composed. [Obs.]", "To bring into being; to form out of nothing.", "To cause; to produce."]);
        let study = webster().lookup("study").unwrap();
        assert_eq!(study.senses.len(), 2, "an out-of-sequence number stays inside its sense: {:?}", study.senses);
        assert!(study.senses[0].ends_with("see 3. below."));
    }

    #[test]
    fn inflections_find_their_headword() {
        let d = webster();
        for (word, headword) in [("Codes", "code"), ("provided", "provide"), ("creating", "create"), ("studies", "study"), ("studied", "study"), ("code's", "code"), ("Provides", "provide")] {
            assert_eq!(d.lookup(word).map(|e| e.headword), Some(headword.to_string()), "{word}");
        }
        assert_eq!(d.lookup("software"), None);
    }

    #[test]
    fn a_bare_cross_reference_is_followed_once() {
        let entry = webster().lookup("program").unwrap();
        assert_eq!(entry.headword, "program");
        assert_eq!(entry.senses, ["Same as Programme.", "A brief outline of the order to be pursued.", "A preliminary advertisement."]);
    }

    #[test]
    fn a_store_reads_its_dictionaries_dir_in_name_order() {
        let scratch = crate::store::tests::ScratchDir::new("dictionaries");
        let dir = scratch.0.join("dictionaries");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("b-second.json"), r#"{"word": "from the second"}"#).unwrap();
        std::fs::write(dir.join("a-first.json"), r#"{"word": "from the first", "only": "here"}"#).unwrap();
        std::fs::write(dir.join("notes.txt"), "not a dictionary").unwrap();
        let dictionaries = Dictionaries::load(&dir).unwrap();
        assert_eq!(dictionaries.names(), ["a-first", "b-second"]);
        assert_eq!(dictionaries.lookup("word").unwrap().source, "a-first");
        assert!(Dictionaries::load(&scratch.0.join("absent")).unwrap().is_empty());
        std::fs::write(dir.join("c-bad.json"), "[1, 2]").unwrap();
        assert!(Dictionaries::load(&dir).err().expect("a bad file is refused").contains("c-bad.json"));
    }
}
