//! What a run looks like in space, and what a tap on it means.
//!
//! A host draws [`Panel`]s, named texts each at a world offset, and turns
//! a tap into a panel name and a byte offset (the glyph a pick hit). This
//! module decides both ends: [`compose`] lays a run out as panels, and
//! [`View::action`] says what a tap on byte `b` of panel `p` does. The
//! host draws, picks and applies ([`apply`]); it holds no definition logic
//! of its own, so the logic is tested here and every host shares it.
//!
//! The panels:
//! - [`PASSAGE`]: the head version's text, at the origin. A tap on a word
//!   asks for its expansion.
//! - [`OPTIONS`]: to the right, the expansion of the request in focus (the
//!   newest one made against the head and not yet taken): its substitutes,
//!   definitions and translations one per line, each line a tap target that
//!   takes it, and its related topics. While the agent has not answered, it
//!   says so.
//! - `card-v{k}`, one per version: the diff stack, to the left, the newest
//!   in front and each older card a step further back and higher, so the
//!   stack reads as a deck. Card `k` shows the step that made version `k`
//!   (card 0 is the base). A tap on a card checks its version out, and the
//!   head's card is marked `▶`.
//!
//! Widths are estimated from the panels' longest lines in cells (the
//! host's cell advance, wrapped at its wrap width), so a double-width
//! glyph or a passage past one page can make two panels overlap; the
//! layout's real extents would need a second layout pass.

use crate::run::{Choice, Run};
use crate::text::{word_at, Span};

pub const PASSAGE: &str = "passage.txt";
pub const OPTIONS: &str = "options.txt";
const CARD_PREFIX: &str = "card-v";
/// The column a card's text is word-wrapped at: a card is read at a glance
/// and its width sets the whole stack's. (Only a card is wrapped here: a
/// tap on the passage or the options maps back through their bytes, which
/// an inserted newline would move.)
pub const CARD_COLUMNS: usize = 44;
/// How the options panel marks the word in its context. Not the agent's
/// `⟦ ⟧`: the renderer's atlas has no glyph for those, and draws `« »`.
const MARK: (&str, &str) = ("«", "»");

/// `text` word-wrapped at `columns` (by chars; a word longer than a row is
/// cut), its own newlines kept.
pub fn word_wrap(text: &str, columns: usize) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / columns.max(1));
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let mut row = 0;
        for word in line.split(' ') {
            let length = word.chars().count();
            if row > 0 && row + 1 + length > columns {
                out.push('\n');
                row = 0;
            } else if row > 0 {
                out.push(' ');
                row += 1;
            }
            let mut chars = word.chars().peekable();
            while chars.peek().is_some() {
                if row == columns {
                    out.push('\n');
                    row = 0;
                }
                out.push(chars.next().expect("peeked"));
                row += 1;
            }
        }
    }
    out
}

/// The host's text metrics, in world units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metrics {
    /// One monospace cell's advance.
    pub cell_width: f32,
    /// One row's height (rows run down, -y).
    pub line_height: f32,
    /// The column a line wraps at.
    pub wrap_columns: usize,
}

impl Metrics {
    /// The gap between panels: four cells.
    fn gap(&self) -> f32 {
        4.0 * self.cell_width
    }

    /// A text's estimated width: its longest line in cells, capped at the
    /// wrap width.
    fn width_of(&self, text: &str) -> f32 {
        let longest = text.lines().map(|line| line.chars().count()).max().unwrap_or(0);
        longest.min(self.wrap_columns) as f32 * self.cell_width
    }
}

/// One named text at a world offset (its pen origin).
#[derive(Clone, Debug, PartialEq)]
pub struct Panel {
    pub name: String,
    pub text: String,
    pub offset: [f32; 3],
}

/// What a tap does.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Ask for the expansion of this span of the head text.
    Expand(Span),
    /// Take one of a request's answers.
    Take { request: String, choice: Choice },
    /// Read another version.
    Checkout(usize),
}

/// A run, laid out.
#[derive(Clone, Debug)]
pub struct View {
    pub panels: Vec<Panel>,
    /// The request [`OPTIONS`] shows, and which of its lines take what.
    focus: Option<String>,
    option_lines: Vec<(usize, Choice)>,
    /// Which version each card shows.
    cards: Vec<(String, usize)>,
}

/// The request [`OPTIONS`] shows: the newest made against the head that no
/// step has taken.
pub fn focus(run: &Run) -> Option<&crate::run::Request> {
    run.requests.iter().rev().find(|r| r.version == run.head && r.applied.is_none())
}

/// Lay `run` out. `status` is a line the host wants shown (the last
/// refusal, a hint); it closes the options panel.
pub fn compose(run: &Run, status: Option<&str>, metrics: Metrics) -> View {
    let passage = run.head_text();
    let gap = metrics.gap();
    let mut panels = vec![Panel { name: PASSAGE.into(), text: passage.clone(), offset: [0.0, 0.0, 0.0] }];

    // The options panel, line by line, noting which lines take what.
    let mut lines: Vec<String> = Vec::new();
    let mut option_lines = Vec::new();
    let focused = focus(run);
    match focused {
        None => lines.push("tap a word to define it".into()),
        Some(request) => {
            lines.push(format!("{}  {}{}{}", request.id, MARK.0, request.word, MARK.1));
            lines.push(request.marked_context_with(MARK.0, MARK.1));
            match &request.expansion {
                None => lines.push("waiting for an answer…".into()),
                Some(expansion) => {
                    let mut section = |title: &str, entries: Vec<(String, Choice)>, lines: &mut Vec<String>| {
                        if entries.is_empty() {
                            return;
                        }
                        lines.push(String::new());
                        lines.push(title.into());
                        for (text, choice) in entries {
                            option_lines.push((lines.len(), choice));
                            lines.push(text);
                        }
                    };
                    let substitutes = expansion.substitutes.iter().enumerate();
                    section(
                        "take one:",
                        substitutes.map(|(index, s)| (format!("  {}. {s}", index + 1), Choice::Substitute { index })).collect(),
                        &mut lines,
                    );
                    let definitions = expansion.definitions.iter().enumerate();
                    section(
                        "definitions:",
                        definitions
                            .map(|(index, d)| {
                                let pos = d.part_of_speech.as_ref().map(|p| format!("({p}) ")).unwrap_or_default();
                                (format!("  {pos}{}", d.text), Choice::Definition { index })
                            })
                            .collect(),
                        &mut lines,
                    );
                    let translations = expansion.translations.iter().enumerate();
                    section(
                        "translations:",
                        translations.map(|(index, t)| (format!("  [{}] {}", t.language, t.text), Choice::Translation { index })).collect(),
                        &mut lines,
                    );
                    if !expansion.related.is_empty() {
                        lines.push(String::new());
                        lines.push("related:".into());
                        lines.extend(expansion.related.iter().map(|t| format!("  - {} ({})", t.name, t.category)));
                    }
                }
            }
        }
    }
    if let Some(status) = status {
        lines.push(String::new());
        lines.push(status.to_string());
    }
    let options = lines.join("\n");
    panels.push(Panel {
        name: OPTIONS.into(),
        offset: [metrics.width_of(&passage) + gap, 0.0, 0.0],
        text: options,
    });

    // The card stack: one per version, newest in front.
    let mut card_texts = Vec::with_capacity(run.tip() + 1);
    card_texts.push((0, word_wrap(&format!("v0  base\n{}", run.title), CARD_COLUMNS)));
    for (index, step) in run.steps.iter().enumerate() {
        let before = run.text_at(index).expect("a validated run replays every version");
        let card = format!("v{}  {} → {}\n{}", index + 1, step.removed, step.inserted, step.card(&before));
        card_texts.push((index + 1, word_wrap(&card, CARD_COLUMNS)));
    }
    let stack_width = card_texts.iter().map(|(_, text)| metrics.width_of(text) + 2.0 * metrics.cell_width).fold(0.0, f32::max);
    let tip = run.tip();
    let mut cards = Vec::with_capacity(card_texts.len());
    for (version, text) in card_texts {
        let behind = (tip - version) as f32;
        let marker = if version == run.head { "▶ " } else { "  " };
        let name = format!("{CARD_PREFIX}{version}.txt");
        panels.push(Panel {
            name: name.clone(),
            text: format!("{marker}{text}"),
            offset: [-(stack_width + gap), behind * 2.0 * metrics.line_height, -behind * 3.0 * metrics.line_height],
        });
        cards.push((name, version));
    }
    View { panels, focus: focused.map(|r| r.id.clone()), option_lines, cards }
}

impl View {
    /// What a tap on byte `byte` of panel `name` does, if anything.
    pub fn action(&self, run: &Run, name: &str, byte: usize) -> Option<Action> {
        if name == PASSAGE {
            return word_at(&run.head_text(), byte).map(Action::Expand);
        }
        if name == OPTIONS {
            let text = &self.panels.iter().find(|p| p.name == OPTIONS)?.text;
            let line = text.get(..byte)?.matches('\n').count();
            let (_, choice) = self.option_lines.iter().find(|(at, _)| *at == line)?;
            return Some(Action::Take { request: self.focus.clone()?, choice: choice.clone() });
        }
        self.cards.iter().find(|(card, _)| card == name).map(|&(_, version)| Action::Checkout(version))
    }
}

/// Apply a tap to the run. Expanding is refused off the tip: an answer to
/// it could never be taken there (a substitution is made at the tip).
pub fn apply(run: &mut Run, action: Action, now: u64) -> Result<(), String> {
    match action {
        Action::Expand(span) => {
            if run.head != run.tip() {
                return Err(format!("reading v{} of {}: tap the front card to return, or fork this version", run.head, run.tip()));
            }
            run.expand(span, now).map(|_| ())
        }
        Action::Take { request, choice } => run.substitute(&request, choice, now).map(|_| ()),
        Action::Checkout(version) => run.checkout(version),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::Source;
    use crate::text::find_word;
    use crate::Expansion;

    const METRICS: Metrics = Metrics { cell_width: 0.5, line_height: 1.25, wrap_columns: 100 };

    fn panel<'a>(view: &'a View, name: &str) -> &'a Panel {
        view.panels.iter().find(|p| p.name == name).unwrap_or_else(|| panic!("no panel {name}"))
    }

    fn answered() -> Expansion {
        serde_json::from_str(
            r#"{"substitutes": ["fleeting", "brief"], "definitions": [{"text": "lasting a short time", "part_of_speech": "adjective"}],
                "translations": [{"language": "es", "text": "efímero"}], "related": [{"name": "impermanence", "category": "philosophy"}]}"#,
        )
        .unwrap()
    }

    #[test]
    fn the_whole_loop_by_taps() {
        let mut run = Run::new("r".into(), "Letter".into(), Source::Inline, "An ephemeral joy.".into(), 1);
        let view = compose(&run, None, METRICS);
        assert_eq!(panel(&view, OPTIONS).text, "tap a word to define it");

        // A tap inside "ephemeral" asks for it.
        let tap = view.action(&run, PASSAGE, 6).expect("a word");
        assert_eq!(tap, Action::Expand(find_word("An ephemeral joy.", "ephemeral", 1).unwrap()));
        apply(&mut run, tap, 2).unwrap();
        let view = compose(&run, None, METRICS);
        assert!(panel(&view, OPTIONS).text.contains("waiting for an answer"));
        assert!(panel(&view, OPTIONS).text.contains("An «ephemeral» joy."), "the word marked in its sentence, in glyphs the atlas draws");

        // The agent answers; the options panel lists them, one line each.
        run.answer("q1", answered(), 3).unwrap();
        let view = compose(&run, None, METRICS);
        let options = &panel(&view, OPTIONS).text;
        let brief = options.find("2. brief").expect(options);
        assert_eq!(
            view.action(&run, OPTIONS, brief + 3),
            Some(Action::Take { request: "q1".into(), choice: Choice::Substitute { index: 1 } })
        );
        let spanish = options.find("efímero").unwrap();
        assert_eq!(view.action(&run, OPTIONS, spanish).unwrap(), Action::Take { request: "q1".into(), choice: Choice::Translation { index: 0 } });
        assert_eq!(view.action(&run, OPTIONS, options.find("take one").unwrap()), None, "a heading takes nothing");
        assert_eq!(view.action(&run, OPTIONS, options.find("impermanence").unwrap()), None, "related topics are not substitutes");

        let take = view.action(&run, OPTIONS, brief).unwrap();
        apply(&mut run, take, 4).unwrap();
        assert_eq!(run.head_text(), "An brief joy.");
        let view = compose(&run, None, METRICS);
        assert_eq!(panel(&view, OPTIONS).text, "tap a word to define it", "the taken request leaves focus");

        // Two cards; the front one is the head's, and a tap on the back one
        // checks the base out.
        let front = panel(&view, "card-v1.txt");
        let back = panel(&view, "card-v0.txt");
        assert!(front.text.starts_with("▶ v1  ephemeral → brief"), "{}", front.text);
        assert!(back.offset[2] < front.offset[2] && back.offset[1] > front.offset[1], "older cards sit further back and higher");
        assert_eq!(view.action(&run, "card-v0.txt", 0), Some(Action::Checkout(0)));
        apply(&mut run, Action::Checkout(0), 5).unwrap();
        assert_eq!(run.head_text(), "An ephemeral joy.");

        // Off the tip a tap on a word is refused, and says how to get back.
        let view = compose(&run, None, METRICS);
        let tap = view.action(&run, PASSAGE, 6).unwrap();
        assert!(apply(&mut run, tap, 6).unwrap_err().contains("front card"));
    }

    #[test]
    fn panels_do_not_overlap_by_their_estimates() {
        let mut run = Run::new("r".into(), "t".into(), Source::Inline, "a short passage\nwith a much longer second line than the first".into(), 1);
        run.expand(Span { start: 0, end: 1 }, 1).unwrap();
        let view = compose(&run, Some("a status line"), METRICS);
        let passage = panel(&view, PASSAGE);
        let options = panel(&view, OPTIONS);
        assert!(options.offset[0] >= METRICS.width_of(&passage.text) + METRICS.gap() - 1e-6);
        assert!(options.text.ends_with("a status line"));
        let card = panel(&view, "card-v0.txt");
        assert!(card.offset[0] + METRICS.width_of(&card.text) < 0.0, "the stack ends left of the passage");
    }

    #[test]
    fn cards_wrap_at_their_width_and_keep_their_words() {
        assert_eq!(word_wrap("one two three", 7), "one two\nthree");
        assert_eq!(word_wrap("a\nbb cc", 4), "a\nbb\ncc");
        assert_eq!(word_wrap("abcdefghij", 4), "abcd\nefgh\nij", "a word past a row is cut");
        let long = "x ".repeat(60);
        assert!(word_wrap(&long, CARD_COLUMNS).lines().all(|line| line.chars().count() <= CARD_COLUMNS));

        let mut run = Run::new("r".into(), "t".into(), Source::Inline, format!("{} word {}", "lead ".repeat(30), "tail ".repeat(30)), 1);
        let span = find_word(&run.head_text(), "word", 1).unwrap();
        run.expand(span, 1).unwrap();
        run.substitute("q1", Choice::Text { text: "term".into() }, 2).unwrap();
        let view = compose(&run, None, METRICS);
        let card = panel(&view, "card-v1.txt");
        assert!(card.text.lines().all(|line| line.chars().count() <= CARD_COLUMNS + 2), "{}", card.text);
        assert!(card.text.contains("[-word-]{+term+}"));
    }

    #[test]
    fn a_tap_off_any_target_does_nothing() {
        let run = Run::new("r".into(), "t".into(), Source::Inline, "one, two".into(), 1);
        let view = compose(&run, None, METRICS);
        assert_eq!(view.action(&run, PASSAGE, 3), None, "the comma");
        assert_eq!(view.action(&run, OPTIONS, 0), None, "the hint");
        assert_eq!(view.action(&run, "elsewhere.txt", 0), None);
    }
}
