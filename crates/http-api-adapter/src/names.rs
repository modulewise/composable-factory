//! Mapping WIT names to the JSON names an API uses.
//!
//! Always from WIT to JSON, never the reverse. The WIT names are known from
//! the type, while arbitrary JSON names (`userID`, `@id`) cannot be parsed
//! back into WIT labels.

use std::collections::HashMap;
use std::str::FromStr;

use anyhow::{Result, bail};

/// How a WIT name's words are joined into a JSON name. A WIT name is words
/// separated by `-`, each all lowercase or all uppercase. An all-uppercase
/// word is an acronym, so it keeps its case in every convention.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Convention {
    /// The WIT name unchanged: `short-forecast`.
    #[default]
    Identity,
    /// `shortForecast`
    Camel,
    /// `ShortForecast`
    Pascal,
    /// `short_forecast`
    Snake,
}

impl Convention {
    /// The JSON name for `wit` in this convention.
    pub fn apply(self, wit: &str) -> String {
        let words = wit.split('-');
        match self {
            Convention::Identity => wit.to_string(),
            Convention::Snake => words.collect::<Vec<_>>().join("_"),
            Convention::Camel => words
                .enumerate()
                .map(|(index, word)| match index {
                    0 => word.to_string(),
                    _ => capitalize(word),
                })
                .collect(),
            Convention::Pascal => words.map(capitalize).collect(),
        }
    }
}

impl FromStr for Convention {
    type Err = anyhow::Error;

    fn from_str(name: &str) -> Result<Self> {
        Ok(match name {
            "identity" => Convention::Identity,
            "camelCase" => Convention::Camel,
            "PascalCase" => Convention::Pascal,
            "snake_case" => Convention::Snake,
            other => bail!(
                "unknown naming convention '{other}' \
                 (expected identity, camelCase, PascalCase or snake_case)"
            ),
        })
    }
}

/// A word with its first letter uppercased. An all-uppercase word is
/// unchanged, and so is a word starting with a digit.
fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

/// A convention plus overrides for the names it cannot produce.
#[derive(Clone, Debug, Default)]
pub struct Rules {
    convention: Convention,
    /// JSON names by WIT name, applied instead of the convention.
    overrides: HashMap<String, String>,
}

impl Rules {
    pub fn new(convention: Convention) -> Self {
        Rules {
            convention,
            overrides: HashMap::new(),
        }
    }

    /// Map `wit` to `json`, whatever the convention would produce.
    pub fn with_override(mut self, wit: impl Into<String>, json: impl Into<String>) -> Self {
        self.overrides.insert(wit.into(), json.into());
        self
    }

    /// The JSON name for one WIT name.
    pub fn apply(&self, wit: &str) -> String {
        match self.overrides.get(wit) {
            Some(json) => json.clone(),
            None => self.convention.apply(wit),
        }
    }

    /// The JSON names for the names one type declares (a record's fields, an
    /// enum's cases, or its flags), in the same order. Two WIT names mapping
    /// to one JSON name is an error, since the JSON could not tell them apart.
    pub fn apply_all(&self, names: &[&str]) -> Result<Vec<String>> {
        let mut mapped: Vec<String> = Vec::with_capacity(names.len());
        for wit in names {
            let json = self.apply(wit);
            if let Some(earlier) = mapped.iter().position(|other| *other == json) {
                bail!(
                    "'{}' and '{wit}' both map to the JSON name '{json}'",
                    names[earlier]
                );
            }
            mapped.push(json);
        }
        Ok(mapped)
    }
}

/// The rules for an API's JSON: one set for field names, and one for enum
/// values, the names that occur as JSON strings (enum cases, variant case
/// tags, flags), since an API may spell the two differently.
#[derive(Clone, Debug, Default)]
pub struct NameMap {
    pub fields: Rules,
    pub enums: Rules,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_leaves_the_name_unchanged() {
        assert_eq!(
            Convention::Identity.apply("short-forecast"),
            "short-forecast"
        );
    }

    #[test]
    fn camel_case_capitalizes_every_word_after_the_first() {
        assert_eq!(Convention::Camel.apply("short-forecast"), "shortForecast");
        assert_eq!(Convention::Camel.apply("name"), "name");
    }

    #[test]
    fn pascal_case_capitalizes_every_word() {
        assert_eq!(Convention::Pascal.apply("severe"), "Severe");
        assert_eq!(Convention::Pascal.apply("very-likely"), "VeryLikely");
    }

    #[test]
    fn snake_case_joins_the_words_with_underscores() {
        assert_eq!(Convention::Snake.apply("short-forecast"), "short_forecast");
    }

    #[test]
    fn an_acronym_keeps_its_case() {
        assert_eq!(Convention::Camel.apply("UGC"), "UGC");
        assert_eq!(Convention::Camel.apply("station-ID"), "stationID");
        assert_eq!(Convention::Pascal.apply("SAME"), "SAME");
        assert_eq!(Convention::Snake.apply("geocode-UGC"), "geocode_UGC");
    }

    #[test]
    fn a_word_starting_with_a_digit_is_unchanged() {
        // WIT allows a digit to start any word after the first.
        assert_eq!(
            Convention::Camel.apply("max-temperature-last-24-hours"),
            "maxTemperatureLast24Hours"
        );
    }

    #[test]
    fn a_convention_is_parsed_from_its_own_spelling() {
        assert_eq!(
            "identity".parse::<Convention>().unwrap(),
            Convention::Identity
        );
        assert_eq!(
            "camelCase".parse::<Convention>().unwrap(),
            Convention::Camel
        );
        assert_eq!(
            "PascalCase".parse::<Convention>().unwrap(),
            Convention::Pascal
        );
        assert_eq!(
            "snake_case".parse::<Convention>().unwrap(),
            Convention::Snake
        );
        let error = "kebab".parse::<Convention>().unwrap_err();
        assert!(format!("{error:#}").contains("unknown naming convention 'kebab'"));
    }

    #[test]
    fn an_override_replaces_the_convention() {
        let rules = Rules::new(Convention::Camel).with_override("id", "@id");
        assert_eq!(rules.apply("id"), "@id");
        assert_eq!(rules.apply("area-desc"), "areaDesc");
    }

    #[test]
    fn names_map_in_declaration_order() {
        let rules = Rules::new(Convention::Camel);
        assert_eq!(
            rules
                .apply_all(&["start-time", "is-daytime", "temperature"])
                .unwrap(),
            ["startTime", "isDaytime", "temperature"]
        );
    }

    #[test]
    fn two_names_the_convention_joins_alike_are_rejected() {
        // Capitalizing `b` makes it the same as the acronym `B`.
        let error = Rules::new(Convention::Camel)
            .apply_all(&["a-b", "a-B"])
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("'a-b' and 'a-B' both map to the JSON name 'aB'"),
            "{error:#}"
        );
    }

    #[test]
    fn an_override_colliding_with_the_convention_is_rejected() {
        let error = Rules::new(Convention::Snake)
            .with_override("area", "area_desc")
            .apply_all(&["area", "area-desc"])
            .unwrap_err();
        assert!(
            format!("{error:#}")
                .contains("'area' and 'area-desc' both map to the JSON name 'area_desc'"),
            "{error:#}"
        );
    }

    #[test]
    fn fields_and_cases_follow_their_own_rules() {
        let names = NameMap {
            fields: Rules::new(Convention::Camel),
            enums: Rules::new(Convention::Pascal),
        };
        assert_eq!(names.fields.apply("message-type"), "messageType");
        assert_eq!(names.enums.apply("severe"), "Severe");
    }
}
