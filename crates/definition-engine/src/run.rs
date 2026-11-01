//! A definition-run: a passage, the words selected in it, their expansions,
//! and the stack of substitution diffs.
//!
//! Version `k` of a run is its base text with its first `k` steps applied,
//! in order; each step is one replacement recorded against the version
//! before it (the span it replaced, the text it removed, the text it put
//! there). Steps are never edited or dropped: going back is a
//! [`Run::checkout`] of an earlier version (the later ones stay, to come back
//! to), and diverging from an earlier version is a [`Run::fork`] into a run of
//! its own. So a substitution is taken only at the tip, with the head there.
//!
//! A run read from disk is held to [`Run::validate`] before anything uses
//! it: every step's span fits the version it was made against and removed
//! exactly the text it says, so a hand-edited or truncated file is refused
//! instead of replayed into a different text.

use serde::{Deserialize, Serialize};

use crate::expansion::Expansion;
use crate::text::{context_of, Span};

/// The store format this module writes. A run file from another format is
/// refused by name rather than parsed into defaults.
pub const RUN_FORMAT: u32 = 1;

/// Where a run's base text came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    /// Typed or piped in.
    Inline,
    /// Read from a file, at the run's creation (later edits to the file do
    /// not reach the run).
    File { path: String },
    /// Version `version` of another run.
    Fork(ForkPoint),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkPoint {
    pub run: String,
    pub version: usize,
}

/// A selection waiting for, or holding, its expansion.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    /// The version whose text `span` and `context` were taken from.
    pub version: usize,
    pub span: Span,
    pub word: String,
    /// The sentence around the word, as [`context_of`] found it.
    pub context: String,
    /// Where the word starts in `context`: the word can occur in its
    /// context more than once, and an answer has to fit the right one.
    pub word_offset: usize,
    pub asked_at: u64,
    /// A static dictionary's senses, attached when the word was selected
    /// (`Run::expand_with`): takeable before any agent answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dictionary: Option<crate::dictionary::DictionaryEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expansion: Option<Expansion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered_at: Option<u64>,
    /// The step that took one of its answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied: Option<String>,
}

impl Request {
    /// The context with the word marked `⟦word⟧`, where it stands: brackets
    /// no passage is likely to hold, for an agent to read.
    pub fn marked_context(&self) -> String {
        self.marked_context_with("⟦", "⟧")
    }

    /// The context with the word between `open` and `close`.
    pub fn marked_context_with(&self, open: &str, close: &str) -> String {
        let end = self.word_offset + self.word.len();
        format!("{}{open}{}{close}{}", &self.context[..self.word_offset], self.word, &self.context[end..])
    }
}

/// Which answer a substitution took.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Choice {
    Substitute { index: usize },
    Definition { index: usize },
    Translation { index: usize },
    /// One of the dictionary's senses.
    Dictionary { index: usize },
    /// The reader's own text.
    Text { text: String },
}

/// One diff: a card on the run's stack.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub id: String,
    pub request: String,
    pub choice: Choice,
    /// The replaced range of the version before this step.
    pub span: Span,
    pub removed: String,
    pub inserted: String,
    pub made_at: u64,
}

impl Step {
    /// The step as a one-line card: the context before it, `[-removed-]`,
    /// `{+inserted+}`, the context after, newlines shown as `⏎`.
    pub fn card(&self, before: &str) -> String {
        let context = context_of(before, self.span);
        let lead = &before[context.start.min(self.span.start)..self.span.start];
        let tail = &before[self.span.end..context.end.max(self.span.end)];
        format!("{lead}[-{}-]{{+{}+}}{tail}", self.removed, self.inserted).replace('\n', "⏎")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub format: u32,
    pub id: String,
    pub title: String,
    pub created_at: u64,
    pub source: Source,
    pub base: String,
    /// The diff stack, oldest first.
    pub steps: Vec<Step>,
    /// The version being read: `0..=steps.len()`.
    pub head: usize,
    pub requests: Vec<Request>,
}

impl Run {
    pub fn new(id: String, title: String, source: Source, base: String, now: u64) -> Run {
        Run { format: RUN_FORMAT, id, title, created_at: now, source, base, steps: Vec::new(), head: 0, requests: Vec::new() }
    }

    /// The newest version's number.
    pub fn tip(&self) -> usize {
        self.steps.len()
    }

    /// The text of version `version`.
    pub fn text_at(&self, version: usize) -> Result<String, String> {
        if version > self.tip() {
            return Err(format!("run {} has versions 0..={}, not {version}", self.id, self.tip()));
        }
        let mut text = self.base.clone();
        for step in &self.steps[..version] {
            text.replace_range(step.span.start..step.span.end, &step.inserted);
        }
        Ok(text)
    }

    pub fn head_text(&self) -> String {
        self.text_at(self.head).expect("a validated run's head is one of its versions")
    }

    pub fn request(&self, id: &str) -> Result<&Request, String> {
        self.requests.iter().find(|r| r.id == id).ok_or_else(|| format!("run {} has no request {id}", self.id))
    }

    fn request_mut(&mut self, id: &str) -> Result<&mut Request, String> {
        let run = self.id.clone();
        self.requests.iter_mut().find(|r| r.id == id).ok_or_else(|| format!("run {run} has no request {id}"))
    }

    /// Ask for the expansion of `span` of the head text.
    pub fn expand(&mut self, span: Span, now: u64) -> Result<&Request, String> {
        let text = self.head_text();
        let span = Span::checked(&text, span.start, span.end)?;
        if span.of(&text).trim().is_empty() {
            return Err("a selection needs at least one non-blank character".into());
        }
        let context = context_of(&text, span);
        let request = Request {
            id: format!("q{}", self.requests.len() + 1),
            version: self.head,
            span,
            word: span.of(&text).to_string(),
            context: context.of(&text).to_string(),
            word_offset: span.start - context.start,
            asked_at: now,
            dictionary: None,
            expansion: None,
            answered_at: None,
            applied: None,
        };
        self.requests.push(request);
        Ok(self.requests.last().expect("just pushed"))
    }

    /// [`Run::expand`], with the first of `dictionaries` that knows the word
    /// attaching its senses to the request.
    pub fn expand_with(&mut self, span: Span, now: u64, dictionaries: &crate::dictionary::Dictionaries) -> Result<&Request, String> {
        let index = {
            let request = self.expand(span, now)?;
            let entry = dictionaries.lookup(&request.word);
            (self.requests.len() - 1, entry)
        };
        self.requests[index.0].dictionary = index.1;
        Ok(&self.requests[index.0])
    }

    /// Keep an agent's expansion for a request. An answered request can be
    /// answered again (a better answer replaces the first) until one of its
    /// answers is taken.
    pub fn answer(&mut self, request: &str, expansion: Expansion, now: u64) -> Result<(), String> {
        expansion.validate()?;
        let request = self.request_mut(request)?;
        if let Some(step) = &request.applied {
            return Err(format!("request {} was already taken by step {step}", request.id));
        }
        request.expansion = Some(expansion);
        request.answered_at = Some(now);
        Ok(())
    }

    /// Put one of a request's answers, or the reader's own text, in the
    /// selected word's place: a new step on the stack, and the head moves to
    /// the version it makes.
    pub fn substitute(&mut self, request: &str, choice: Choice, now: u64) -> Result<&Step, String> {
        if self.head != self.tip() {
            return Err(format!(
                "run {} is checked out at version {} of {}: substitute at the tip, or fork this version into its own run",
                self.id,
                self.head,
                self.tip()
            ));
        }
        let tip = self.tip();
        let found = self.request(request)?;
        if found.version != tip {
            return Err(format!(
                "request {request} was made against version {}, and the tip is {tip}: expand the word again in the current text",
                found.version
            ));
        }
        if let Some(step) = &found.applied {
            return Err(format!("request {request} was already taken by step {step}"));
        }
        let inserted = match &choice {
            Choice::Text { text } => text.clone(),
            Choice::Dictionary { index } => {
                let senses = found.dictionary.as_ref().map_or(&[][..], |d| &d.senses[..]);
                senses
                    .get(*index)
                    .ok_or_else(|| format!("request {request} has {} dictionary senses, no index {index}", senses.len()))?
                    .clone()
            }
            other => {
                let expansion = found.expansion.as_ref().ok_or_else(|| format!("request {request} has no answer yet; only --text can stand in for it"))?;
                let pick = |kind: &str, index: usize, len: usize| -> Result<usize, String> {
                    (index < len).then_some(index).ok_or_else(|| format!("request {request} has {len} {kind}, no index {index}"))
                };
                match *other {
                    Choice::Substitute { index } => expansion.substitutes[pick("substitutes", index, expansion.substitutes.len())?].clone(),
                    Choice::Definition { index } => expansion.definitions[pick("definitions", index, expansion.definitions.len())?].text.clone(),
                    Choice::Translation { index } => expansion.translations[pick("translations", index, expansion.translations.len())?].text.clone(),
                    Choice::Text { .. } | Choice::Dictionary { .. } => unreachable!("matched above"),
                }
            }
        };
        let step = Step {
            id: format!("d{}", self.steps.len() + 1),
            request: request.to_string(),
            choice,
            span: found.span,
            removed: found.word.clone(),
            inserted,
            made_at: now,
        };
        let step_id = step.id.clone();
        self.request_mut(request)?.applied = Some(step_id);
        self.steps.push(step);
        self.head = self.tip();
        Ok(self.steps.last().expect("just pushed"))
    }

    /// Read an earlier (or later) version. Nothing is dropped.
    pub fn checkout(&mut self, version: usize) -> Result<(), String> {
        if version > self.tip() {
            return Err(format!("run {} has versions 0..={}, not {version}", self.id, self.tip()));
        }
        self.head = version;
        Ok(())
    }

    /// A new run whose history is this one's up to `version`: its steps
    /// before that version, the requests made against those versions, and
    /// the requests made against `version` itself that no step took (they
    /// can still be taken in the fork). A request's step sits at the version
    /// it was made against, so every kept request's step is kept with it.
    pub fn fork(&self, version: usize, id: String, title: String, now: u64) -> Result<Run, String> {
        if version > self.tip() {
            return Err(format!("run {} has versions 0..={}, not {version}", self.id, self.tip()));
        }
        let requests = self
            .requests
            .iter()
            .filter(|r| r.version < version || (r.version == version && r.applied.is_none()))
            .cloned()
            .collect();
        let mut fork = Run::new(id, title, Source::Fork(ForkPoint { run: self.id.clone(), version }), self.base.clone(), now);
        fork.steps = self.steps[..version].to_vec();
        fork.head = version;
        fork.requests = requests;
        fork.validate()?;
        Ok(fork)
    }

    /// The invariants a run read from disk must hold before it is replayed.
    pub fn validate(&self) -> Result<(), String> {
        if self.format != RUN_FORMAT {
            return Err(format!("run {} is format {}; this engine reads format {RUN_FORMAT}", self.id, self.format));
        }
        if self.head > self.tip() {
            return Err(format!("run {}: head {} is past the tip {}", self.id, self.head, self.tip()));
        }
        let mut text = self.base.clone();
        let mut versions = vec![text.clone()];
        for (index, step) in self.steps.iter().enumerate() {
            let span = Span::checked(&text, step.span.start, step.span.end).map_err(|e| format!("run {} step {}: {e}", self.id, step.id))?;
            if span.of(&text) != step.removed {
                return Err(format!(
                    "run {} step {}: version {index} has {:?} where the step removed {:?}",
                    self.id,
                    step.id,
                    span.of(&text),
                    step.removed
                ));
            }
            let request = self.request(&step.request).map_err(|e| format!("step {}: {e}", step.id))?;
            if request.applied.as_deref() != Some(step.id.as_str()) || request.version != index {
                return Err(format!("run {} step {}: request {} does not record it at version {index}", self.id, step.id, request.id));
            }
            text.replace_range(span.start..span.end, &step.inserted);
            versions.push(text.clone());
        }
        for request in &self.requests {
            let version = versions
                .get(request.version)
                .ok_or_else(|| format!("run {} request {}: version {} does not exist", self.id, request.id, request.version))?;
            let span = Span::checked(version, request.span.start, request.span.end).map_err(|e| format!("run {} request {}: {e}", self.id, request.id))?;
            if span.of(version) != request.word {
                return Err(format!("run {} request {}: version {} has {:?}, not {:?}", self.id, request.id, request.version, span.of(version), request.word));
            }
            if request.context.get(request.word_offset..).is_none_or(|rest| !rest.starts_with(&request.word)) {
                return Err(format!("run {} request {}: its context does not hold the word at {}", self.id, request.id, request.word_offset));
            }
            if let Some(expansion) = &request.expansion {
                expansion.validate().map_err(|e| format!("run {} request {}: {e}", self.id, request.id))?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::find_word;

    fn answered(substitutes: &[&str], definition: &str) -> Expansion {
        Expansion {
            definitions: vec![crate::Sense { text: definition.into(), part_of_speech: None }],
            substitutes: substitutes.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn run(text: &str) -> Run {
        Run::new("r".into(), "t".into(), Source::Inline, text.into(), 1)
    }

    #[test]
    fn the_loop_recurses_into_the_text_it_made() {
        let mut run = run("An ephemeral joy.");
        let span = find_word(&run.head_text(), "ephemeral", 1).unwrap();
        let q1 = run.expand(span, 2).unwrap().id.clone();
        assert_eq!(run.request(&q1).unwrap().context, "An ephemeral joy.");
        assert_eq!(run.request(&q1).unwrap().marked_context(), "An ⟦ephemeral⟧ joy.");
        run.answer(&q1, answered(&["fleeting"], "lasting a very short time"), 3).unwrap();
        run.substitute(&q1, Choice::Definition { index: 0 }, 4).unwrap();
        assert_eq!(run.head_text(), "An lasting a very short time joy.");

        // Select inside what the substitution put there.
        let span = find_word(&run.head_text(), "short", 1).unwrap();
        let q2 = run.expand(span, 5).unwrap().id.clone();
        assert_eq!(run.request(&q2).unwrap().version, 1);
        run.answer(&q2, answered(&["brief"], "of small extent"), 6).unwrap();
        run.substitute(&q2, Choice::Substitute { index: 0 }, 7).unwrap();
        assert_eq!(run.head_text(), "An lasting a very brief time joy.");
        assert_eq!(run.text_at(0).unwrap(), "An ephemeral joy.");
        run.validate().unwrap();
    }

    #[test]
    fn substitution_is_refused_off_the_tip_and_for_stale_requests() {
        let mut run = run("one two");
        let q1 = run.expand(Span { start: 0, end: 3 }, 1).unwrap().id.clone();
        let q2 = run.expand(Span { start: 4, end: 7 }, 1).unwrap().id.clone();
        run.substitute(&q1, Choice::Text { text: "1".into() }, 2).unwrap();
        // q2's span was taken from version 0; the text has moved.
        assert!(run.substitute(&q2, Choice::Text { text: "2".into() }, 3).unwrap_err().contains("version 0"));
        assert!(run.substitute(&q1, Choice::Text { text: "x".into() }, 3).is_err(), "taken twice");
        run.checkout(0).unwrap();
        let q3 = run.expand(Span { start: 0, end: 3 }, 4).unwrap().id.clone();
        assert!(run.substitute(&q3, Choice::Text { text: "uno".into() }, 5).unwrap_err().contains("fork"));
        assert_eq!(run.tip(), 1, "checkout dropped nothing");
    }

    #[test]
    fn a_choice_past_the_answers_is_refused() {
        let mut run = run("word");
        let q1 = run.expand(Span { start: 0, end: 4 }, 1).unwrap().id.clone();
        assert!(run.substitute(&q1, Choice::Substitute { index: 0 }, 2).unwrap_err().contains("no answer"));
        run.answer(&q1, answered(&["term"], "a unit of language"), 2).unwrap();
        assert!(run.substitute(&q1, Choice::Substitute { index: 1 }, 3).unwrap_err().contains("no index 1"));
        assert!(run.substitute(&q1, Choice::Translation { index: 0 }, 3).is_err());
    }

    #[test]
    fn a_dictionary_sense_is_takeable_before_any_answer() {
        let dictionaries = crate::dictionary::Dictionaries::new(vec![crate::dictionary::Dictionary::from_entries(
            "webster",
            [("joy".to_string(), "1. The passion or emotion excited by the acquisition of good. 2. That which causes joy.".to_string())],
        )]);
        let mut run = run("An ephemeral joy.");
        let span = find_word(&run.head_text(), "joy", 1).unwrap();
        let q = run.expand_with(span, 1, &dictionaries).unwrap();
        assert_eq!(q.dictionary.as_ref().map(|d| d.senses.len()), Some(2));
        assert!(q.expansion.is_none(), "still pending for an agent");
        let q = q.id.clone();
        assert!(run.substitute(&q, Choice::Dictionary { index: 2 }, 2).unwrap_err().contains("2 dictionary senses"));
        run.substitute(&q, Choice::Dictionary { index: 1 }, 2).unwrap();
        assert_eq!(run.head_text(), "An ephemeral That which causes joy..");
        let span = find_word(&run.head_text(), "ephemeral", 1).unwrap();
        assert!(run.expand_with(span, 3, &dictionaries).unwrap().dictionary.is_none(), "a word no dictionary knows");
    }

    #[test]
    fn a_fork_keeps_the_history_before_its_version() {
        let mut run = run("a b c");
        for (word, with) in [("a", "A"), ("b", "B"), ("c", "C")] {
            let span = find_word(&run.head_text(), word, 1).unwrap();
            let q = run.expand(span, 1).unwrap().id.clone();
            run.substitute(&q, Choice::Text { text: with.into() }, 2).unwrap();
        }
        let fork = run.fork(1, "f".into(), "fork".into(), 3).unwrap();
        assert_eq!(fork.head_text(), "A b c");
        assert_eq!(fork.steps.len(), 1);
        assert_eq!(fork.requests.len(), 1, "requests against later versions stay behind");
        assert_eq!(fork.source, Source::Fork(ForkPoint { run: "r".into(), version: 1 }));
    }

    #[test]
    fn a_tampered_run_is_refused() {
        let mut run = run("one two");
        let q = run.expand(Span { start: 0, end: 3 }, 1).unwrap().id.clone();
        run.substitute(&q, Choice::Text { text: "1".into() }, 2).unwrap();
        run.validate().unwrap();

        let mut moved = run.clone();
        moved.steps[0].span.start = 1;
        assert!(moved.validate().is_err(), "span no longer covers what it removed");

        let mut rebased = run.clone();
        rebased.base = "uno two".into();
        assert!(rebased.validate().is_err(), "base no longer holds the removed word");

        let mut misplaced = run.clone();
        misplaced.requests[0].word_offset = 1;
        assert!(misplaced.validate().unwrap_err().contains("context"), "the word is not where its offset says");

        let mut future = run.clone();
        future.format = RUN_FORMAT + 1;
        assert!(future.validate().unwrap_err().contains("format"));
    }

    #[test]
    fn a_card_shows_the_diff_in_its_sentence() {
        let mut run = run("Intro. The quick fox ran.\nNext.");
        let span = find_word(&run.head_text(), "quick", 1).unwrap();
        let q = run.expand(span, 1).unwrap().id.clone();
        run.substitute(&q, Choice::Text { text: "swift".into() }, 2).unwrap();
        assert_eq!(run.steps[0].card(&run.text_at(0).unwrap()), "The [-quick-]{+swift+} fox ran.");
    }
}
