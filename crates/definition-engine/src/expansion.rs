//! An expansion: what an agent answers a request with.
//!
//! The JSON an agent hands `defn answer` is exactly [`Expansion`]'s serde
//! form, unknown fields refused, so a misspelt key fails loud instead of
//! leaving a section silently empty:
//!
//! ```json
//! {
//!   "definitions": [{"text": "lasting a very short time", "part_of_speech": "adjective"}],
//!   "translations": [{"language": "es", "text": "efímero"}],
//!   "related": [{"name": "impermanence", "category": "philosophy"}],
//!   "substitutes": ["fleeting", "short-lived"]
//! }
//! ```

use serde::{Deserialize, Serialize};

/// Everything known about one selected word in its context.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expansion {
    /// Senses of the word, the one it has in its context first.
    #[serde(default)]
    pub definitions: Vec<Sense>,
    #[serde(default)]
    pub translations: Vec<Translation>,
    /// Topics the word leads to, each named and categorized.
    #[serde(default)]
    pub related: Vec<Topic>,
    /// Words or phrases that can stand in the word's place in its context.
    #[serde(default)]
    pub substitutes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sense {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part_of_speech: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Translation {
    /// A language tag ("es", "ja", "pt-BR").
    pub language: String,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Topic {
    pub name: String,
    pub category: String,
}

impl Expansion {
    /// Refuse an expansion with nothing in it, or with an empty string
    /// anywhere: an empty substitute would delete the word, and an empty
    /// definition is a card with nothing on it.
    pub fn validate(&self) -> Result<(), String> {
        if self.definitions.is_empty() && self.translations.is_empty() && self.related.is_empty() && self.substitutes.is_empty() {
            return Err("an expansion needs at least one definition, translation, related topic or substitute".into());
        }
        let blank = |what: &str, index: usize| Err(format!("{what} {index} is empty"));
        for (index, sense) in self.definitions.iter().enumerate() {
            if sense.text.trim().is_empty() {
                return blank("definition", index);
            }
        }
        for (index, translation) in self.translations.iter().enumerate() {
            if translation.language.trim().is_empty() || translation.text.trim().is_empty() {
                return blank("translation", index);
            }
        }
        for (index, topic) in self.related.iter().enumerate() {
            if topic.name.trim().is_empty() || topic.category.trim().is_empty() {
                return blank("related topic", index);
            }
        }
        for (index, substitute) in self.substitutes.iter().enumerate() {
            if substitute.trim().is_empty() {
                return blank("substitute", index);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_documented_json_parses_and_validates() {
        let json = r#"{
          "definitions": [{"text": "lasting a very short time", "part_of_speech": "adjective"}],
          "translations": [{"language": "es", "text": "efímero"}],
          "related": [{"name": "impermanence", "category": "philosophy"}],
          "substitutes": ["fleeting", "short-lived"]
        }"#;
        let expansion: Expansion = serde_json::from_str(json).unwrap();
        expansion.validate().unwrap();
        assert_eq!(expansion.substitutes, ["fleeting", "short-lived"]);
    }

    #[test]
    fn a_misspelt_key_is_refused_not_ignored() {
        let json = r#"{"substitute": ["fleeting"]}"#;
        assert!(serde_json::from_str::<Expansion>(json).is_err());
    }

    #[test]
    fn empty_expansions_and_blank_entries_are_refused() {
        assert!(Expansion::default().validate().is_err());
        let blank = Expansion { substitutes: vec!["  ".into()], ..Default::default() };
        assert!(blank.validate().unwrap_err().contains("substitute 0"));
    }
}
