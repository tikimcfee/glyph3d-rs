---
name: definition-engine
description: Answer and drive definition-runs through the `defn` CLI — wait for a reader's word selections (`defn pending --wait`), answer each with definitions, translations, related topics and substitutes that fit its context, and, when asked, start runs, select words, substitute, fork. Use when the user asks to answer, serve, drain or watch definition requests, to run the definition loop, or to work with `defn` / definition-runs.
---

# The definition engine, from the agent's side

`crates/definition-engine` is the core; root `AGENTS.md` § Layout says what
it is. A **run** is a passage and its history. A reader **selects** a word
(a request, `q1`, `q2`, ...), an agent **answers** it with an expansion, and
the reader **takes** one answer, which makes a **step** (`d1`, ...): one
diff, one card, the next version of the text. The reader can then select
inside the new text again. Your job is usually the answer: fast, grounded in
the word's context, and shaped so that taking it reads well.

## Setup

```sh
cargo build --release -p definition-engine     # once; then:
DEFN=<repo>/target/release/defn                # absolute: `run` cds elsewhere
```

Do not add a cargo alias for it. In a worktree, cargo concatenates alias
arrays from every ancestor `.cargo/config.toml`, and only `cargo glyph`
strips the doubled prefix (root `AGENTS.md` § The tool).

`--store DIR` and `--json` are global: they go anywhere on the line,
before or after the verb. A request is named by its run AND its id: every
run numbers its own requests from `q1`.

The store is `$DEFN_STORE` (default `$XDG_DATA_HOME/glyph3d/definition-runs`,
else `~/.local/share/glyph3d/definition-runs`); `--store DIR` overrides it.
**You and the reader must use the same store.** If `pending` is
empty while the reader says they asked, check that first. Never edit a run's
JSON by hand: a run is replayed against its invariants on every load, and a
hand edit gets the whole run refused.

## Answering: the drain loop

1. Wait for work, in the background, so you are woken when a reader asks:

   ```sh
   $DEFN --json pending --wait --timeout 1800
   ```

   It returns as soon as anything is pending (at once if something already
   is) and prints every pending request; after `--timeout` SECONDS with
   none, it prints `[]`. In Claude Code, run it with `run_in_background`: its
   exit re-invokes you, so do not poll its output. Re-arm it after
   answering unless the user has said to stop.

   Each item looks like this:

   ```json
   {"run": "root-note-dmjirqy8tb3k", "request": "q1", "word": "hueristic",
    "marked": "* develop a casual ⟦hueristic⟧ and learn to navigate the space, or decide to end the session.",
    "version": 0, "span": {"start": 1309, "end": 1318}}
   ```

   `marked` is the sentence or list item the word stands in, with the
   selected occurrence in `⟦ ⟧`. `span` is its byte range in version
   `version` of the run's text.
2. For each item, compose an expansion (below). When the sentence is too
   thin, read around it: `$DEFN show <run> --at <version>` prints the text
   the request was made against.
3. Answer through stdin, with a quoted heredoc so the JSON needs no escaping:

   ```sh
   $DEFN answer <run> <request> <<'JSON'
   { "definitions": [...], "translations": [...], "related": [...], "substitutes": [...] }
   JSON
   ```

   A refusal names its cause: an empty section entry, a misspelt key (unknown
   keys are refused, never ignored), or a request already taken. You can
   answer a request again to replace your answer until the reader takes one.
4. Answer every item the wait returned, then re-arm. Several requests at
   once are normal: a reader taps faster than any agent answers.

Latency is the point: the reader is waiting in a 3D view. Answer the
pending items before any other work, and keep each answer to what the
reader needs to choose.

## What a good expansion holds

The schema, exactly (all four keys optional, at least one entry overall):

```json
{
  "definitions": [{"text": "...", "part_of_speech": "noun"}],
  "translations": [{"language": "es", "text": "..."}],
  "related": [{"name": "...", "category": "..."}],
  "substitutes": ["...", "..."]
}
```

- **Substitutes are the loop's engine.** Each one replaces exactly the
  `⟦ ⟧` span and nothing around it, so it must read correctly in `marked`
  with the brackets swapped for it. In "a casual ⟦hueristic⟧ and learn",
  "rule of thumb" fits and "a rule of thumb" does not (the article is
  already there). Same
  inflection (tense, number, person), same capitalization as the selected
  span (capitalized at a sentence start, lowercase mid-sentence), no
  article the sentence already has. Give 3 to 6, ordered from
  plainest to most precise, and end with one short explanatory phrase that
  defines the word inline ("hueristic" becomes "rule of thumb that guides
  a guess"). That phrase is the "transformed definition text" a
  reader expands into when one word is not enough. It fits the slot too:
  read the sentence with it in place before you send it. Fix an obvious misspelling
  in your first substitute (the reader's text is theirs, but a substitute
  can correct it).
- **Definitions:** the sense the word has *in this context* first, then
  others if they help, each one plain sentence, with `part_of_speech`.
- **Code and this repo.** When the passage is code, a path or project
  prose, or the word is an identifier, define it as the code defines it, not
  as a dictionary does. Read the source before answering `HyperLayout` or
  `Carrel`, but only as deep as the answer needs: the defining item's doc
  comment or module header (the LSP's hover, or `rg` for its definition),
  or its paragraph in root `AGENTS.md`. Not the implementation: the reader is
  waiting. For a run made with `--file`, the file's path
  is in the run's `source`.
- **Translations:** check `$DEFN_LANGUAGES` once when you start
  (comma-separated tags, `es,ja,pt-BR`); when it is unset, Spanish and
  Japanese. Each translation is plain text in the language's own script,
  with no gloss or parenthetical, because a reader can take a translation
  into the text, and selecting the translated word gets its own expansion.
  Skip identifiers and proper nouns that do not translate.
- **Related:** 2 to 5 topics, each with a free-text category (nothing
  validates it) that says which direction it leads (`linguistics`, `rust`, `glyph3d-rs`, `history`, ...). These are the
  reader's exits from the word, so vary the categories.
- If you are unsure of a sense (jargon you cannot ground, an ambiguous
  abbreviation), say so in the definition text ("(uncertain) ...") rather
  than presenting a guess as fact.

## Driving the loop for the reader

Only when asked. Choosing an answer is the reader's act, so do not take one
on their behalf unless they say which. Every command takes a unique prefix of
a run id. Every number a reader types counts from 1; byte offsets count
from 0.

| Intent | Command |
| :--- | :--- |
| Start a run | `$DEFN new "<title>" --file <path>` or `--text "<passage>"` (prints the id) |
| List runs | `$DEFN list` (`v<head>/<tip>`, pending count) |
| Read | `$DEFN show <run> [--at <version>]` |
| Select | `$DEFN expand <run> --word <w> [--nth N]`, `--at <byte>` (a tap), `--span <start> <end>` |
| See answers | `$DEFN options <run> <request>` (numbered from 1) |
| Take one | `$DEFN substitute <run> <request> --pick N`, `--definition N`, `--translation N` or `--text "<own>"` |
| The card stack | `$DEFN log <run>` (`[-removed-]{+inserted+}` in its sentence; `*` marks the head) |
| Go back | `$DEFN checkout <run> <version>` (later versions are kept) |
| Branch | `$DEFN fork <run> [--at <version>] [--title T]` |
| A tree by layer | `$DEFN layers <dir> [--all]` (code, binary, directory, named) |

Add `--json` before the verb for machine-readable output.

Two refusals are the model working, not bugs:

- *"request qN was made against version K, and the tip is M"*: the text
  moved under that selection. Expand the word again in the current text.
- *"checked out at version K"*: a substitution is taken only at the tip.
  Check out the tip, or fork the version into its own run.
