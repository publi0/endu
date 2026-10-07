//! Canonical names shared by remote hints and conservative local restoration.

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::{Arc, LazyLock, RwLock};

use serde::{Deserialize, Serialize};
use unicode_properties::{GeneralCategoryGroup, UnicodeGeneralCategory};

pub const MAX_TERMS: usize = 2_000;
pub const MAX_TERM_BYTES: usize = 128;
const MAX_WORDS: usize = 8;
static CURRENT: LazyLock<RwLock<Snapshot>> = LazyLock::new(|| RwLock::new(Snapshot::default()));

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Vocabulary {
    pub terms: Vec<String>,
    pub remote_hints: bool,
    pub restore_names: bool,
    pub approximate: bool,
}

impl Default for Vocabulary {
    fn default() -> Self {
        Self {
            terms: Vec::new(),
            remote_hints: true,
            restore_names: true,
            approximate: false,
        }
    }
}

impl Vocabulary {
    pub fn validate(&self) -> Result<(), String> {
        if self.terms.len() > MAX_TERMS {
            return Err(format!("Use at most {MAX_TERMS} names."));
        }
        let mut seen = BTreeMap::new();
        for (index, term) in self.terms.iter().enumerate() {
            if term.trim() != term
                || term.is_empty()
                || term.len() > MAX_TERM_BYTES
                || term
                    .chars()
                    .any(|c| c.is_control() || !(lexical(c) || c == ' '))
                || normalize(term).is_empty()
                || tokens(term).len() > MAX_WORDS
            {
                return Err(format!(
                    "Name {} must be a short name or phrase, without control characters, URLs or paths.",
                    index + 1
                ));
            }
            if let Some(previous) = seen.insert(normalize(term), index) {
                return Err(format!(
                    "Names {} and {} have the same normalized spelling.",
                    previous + 1,
                    index + 1
                ));
            }
        }
        Ok(())
    }

    pub fn apply_runtime(&self) {
        *CURRENT.write().unwrap_or_else(|e| e.into_inner()) = Snapshot::new(self.clone());
    }
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot(Arc<Prepared>);

#[derive(Debug, Default)]
struct Prepared {
    vocabulary: Vocabulary,
    exact: BTreeMap<String, usize>,
    normalized: Vec<Vec<char>>,
    max_length: usize,
}

pub struct RestoredText {
    pub text: String,
    pub preserve_initial_case: bool,
}

impl Snapshot {
    pub fn current() -> Self {
        CURRENT.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn new(vocabulary: Vocabulary) -> Self {
        // Every persisted/UI candidate is validated before it reaches runtime.
        // Keep malformed direct callers bounded and inert as well.
        if vocabulary.validate().is_err() {
            return Self::default();
        }
        let exact: BTreeMap<_, _> = vocabulary
            .terms
            .iter()
            .enumerate()
            .map(|(index, name)| (normalize(name), index))
            .collect();
        let normalized: Vec<Vec<char>> = vocabulary
            .terms
            .iter()
            .map(|name| normalize(name).chars().collect())
            .collect();
        let max_length = normalized.iter().map(Vec::len).max().unwrap_or(0);
        Self(Arc::new(Prepared {
            vocabulary,
            exact,
            normalized,
            max_length,
        }))
    }

    pub fn settings(&self) -> &Vocabulary {
        &self.0.vocabulary
    }

    /// Limit outgoing names independently of the larger local dictionary.
    /// UTF-8 bytes provide a conservative budget without guessing a tokenizer.
    pub fn remote_terms(&self, max_terms: usize, max_bytes: usize) -> Vec<String> {
        if !self.settings().remote_hints {
            return Vec::new();
        }
        let mut bytes = 0;
        self.settings()
            .terms
            .iter()
            .take(max_terms)
            .take_while(|name| {
                bytes += name.len() + 1;
                bytes <= max_bytes
            })
            .cloned()
            .collect()
    }

    pub fn restore(&self, text: &str) -> RestoredText {
        let mut restored = RestoredText {
            text: String::new(),
            preserve_initial_case: false,
        };
        if !self.settings().restore_names || self.0.exact.is_empty() {
            restored.text = text.to_owned();
            return restored;
        }
        let words = tokens(text);
        let protected = protected_ranges(text);
        let mut cursor = 0;
        let mut index = 0;
        while index < words.len() {
            let start = words[index].start;
            let mut exact = None;
            let mut fuzzy = None;
            let mut ambiguous = false;
            for end_index in index..words.len().min(index + MAX_WORDS) {
                if end_index > index
                    && !text[words[end_index - 1].end..words[end_index].start]
                        .chars()
                        .all(|c| {
                            c.is_whitespace() && !matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
                        })
                {
                    break;
                }
                let end = words[end_index].end;
                if protected.iter().any(|r| start < r.end && end > r.start) {
                    break;
                }
                let normalized = normalize(&text[start..end]);
                let chars: Vec<_> = normalized.chars().collect();
                if chars.len() > self.0.max_length + 1 {
                    break;
                }
                if let Some(&term) = self.0.exact.get(&normalized) {
                    exact = Some((end_index, end, term));
                } else if self.settings().approximate && chars.len() >= 7 {
                    for (term, candidate) in self.0.normalized.iter().enumerate() {
                        if candidate.len() >= 7
                            && chars
                                .iter()
                                .filter(|c| c.is_numeric())
                                .eq(candidate.iter().filter(|c| c.is_numeric()))
                            && one_edit(&chars, candidate)
                        {
                            let matched = (end_index, end, term);
                            if fuzzy.is_some_and(|previous| previous != matched) {
                                ambiguous = true;
                            }
                            fuzzy = Some(matched);
                        }
                    }
                }
            }
            if let Some((end_index, end, term)) = exact.or(if ambiguous { None } else { fuzzy }) {
                restored.text.push_str(&text[cursor..start]);
                let canonical = &self.settings().terms[term];
                if !restored.text.chars().any(char::is_alphabetic)
                    && canonical.chars().any(char::is_alphabetic)
                {
                    restored.preserve_initial_case = true;
                }
                restored.text.push_str(canonical);
                cursor = end;
                index = end_index + 1;
            } else {
                index += 1;
            }
        }
        restored.text.push_str(&text[cursor..]);
        restored
    }
}

fn lexical(c: char) -> bool {
    c.is_alphanumeric()
        || c.general_category_group() == GeneralCategoryGroup::Mark
        || matches!(c, '-' | '_' | '+' | '.' | '\'' | '’')
}

fn normalize(text: &str) -> String {
    text.chars()
        .filter(|c| {
            c.is_alphanumeric()
                || *c == '+'
                || c.general_category_group() == GeneralCategoryGroup::Mark
        })
        .flat_map(char::to_lowercase)
        .collect()
}

fn tokens(text: &str) -> Vec<Range<usize>> {
    let mut result = Vec::new();
    let mut start = None;
    for (index, c) in text
        .char_indices()
        .chain(std::iter::once((text.len(), ' ')))
    {
        if lexical(c) {
            start.get_or_insert(index);
        } else if let Some(begin) = start.take() {
            let token = text[begin..index].trim_matches(['.', '\'', '’']);
            if token.chars().any(char::is_alphanumeric) {
                let offset = text[begin..index].find(token).unwrap_or(0);
                result.push(begin + offset..begin + offset + token.len());
            }
        }
    }
    result
}

fn protected_ranges(text: &str) -> Vec<Range<usize>> {
    let mut result = Vec::new();
    let mut offset = 0;
    for piece in text.split_inclusive(char::is_whitespace) {
        if piece.contains(['/', '\\', '@', ':']) {
            result.push(offset..offset + piece.len());
        }
        offset += piece.len();
    }
    let mut start = None;
    for (index, c) in text.char_indices() {
        if c == '`' {
            if let Some(begin) = start.take() {
                result.push(begin..index + 1);
            } else {
                start = Some(index);
            }
        }
    }
    if let Some(start) = start {
        result.push(start..text.len());
    }
    result
}

fn one_edit(left: &[char], right: &[char]) -> bool {
    if left.len().abs_diff(right.len()) > 1 {
        return false;
    }
    let (mut i, mut j, mut differences) = (0, 0, 0);
    while i < left.len() && j < right.len() {
        if left[i] == right[j] {
            i += 1;
            j += 1;
            continue;
        }
        differences += 1;
        if differences > 1 {
            return false;
        }
        match left.len().cmp(&right.len()) {
            std::cmp::Ordering::Less => j += 1,
            std::cmp::Ordering::Greater => i += 1,
            std::cmp::Ordering::Equal => {
                i += 1;
                j += 1;
            }
        }
    }
    differences + usize::from(i < left.len() || j < right.len()) == 1
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot(names: &[&str], approximate: bool) -> Snapshot {
        Snapshot::new(Vocabulary {
            terms: names.iter().map(|s| s.to_string()).collect(),
            approximate,
            ..Default::default()
        })
    }
    #[test]
    fn names_restore_case_spaces_hyphens_and_longest_phrases_without_cascades() {
        let vocabulary = snapshot(
            &["nimbus-files", "Claude", "Claude Code", "OpenRouter", "C++"],
            false,
        );
        assert_eq!(
            vocabulary
                .restore("NIMBUSFILES, nimbus files e CLAUDE CODE; open router e C++.")
                .text,
            "nimbus-files, nimbus-files e Claude Code; OpenRouter e C++."
        );
        assert_eq!(
            vocabulary.restore("connimbusfiles nimbusfilesx").text,
            "connimbusfiles nimbusfilesx"
        );
        assert_eq!(vocabulary.restore("nimbus\nfiles").text, "nimbus\nfiles");
        assert!(vocabulary.restore("“NIMBUS FILES”.").preserve_initial_case);
        assert!(!vocabulary.restore("Use NIMBUSFILES.").preserve_initial_case);
    }
    #[test]
    fn fuzzy_correction_is_opt_in_long_and_unambiguous() {
        assert_eq!(
            snapshot(&["nimbus-files"], false)
                .restore("mimbusfiles")
                .text,
            "mimbusfiles"
        );
        assert_eq!(
            snapshot(&["nimbus-files"], true)
                .restore("mimbusfiles")
                .text,
            "nimbus-files"
        );
        assert_eq!(
            snapshot(&["nimbus-files", "limbus-files"], true)
                .restore("mimbusfiles")
                .text,
            "mimbusfiles"
        );
        assert_eq!(
            snapshot(&["NIMB", "Claude Code"], true)
                .restore("mimb cloud code")
                .text,
            "mimb cloud code"
        );
    }
    #[test]
    fn approximation_never_changes_numbers_or_versions() {
        let vocabulary = snapshot(&["Nimbus-Files", "Project42"], true);
        assert_eq!(
            vocabulary.restore("nimbusfiles2 project43 projec42").text,
            "nimbusfiles2 project43 Project42"
        );
    }

    #[test]
    fn urls_paths_emails_and_code_are_preserved() {
        let vocabulary = snapshot(&["nimbus-files"], true);
        let text =
            "https://host/mimbusfiles /tmp/mimbusfiles x@mimbusfiles.com `mimbusfiles` mimbusfiles";
        assert_eq!(
            vocabulary.restore(text).text,
            "https://host/mimbusfiles /tmp/mimbusfiles x@mimbusfiles.com `mimbusfiles` nimbus-files"
        );
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn canonical_spelling_wins_after_text_formatting_and_snapshots_are_immutable() {
        let settings = crate::post_processing::Preferences {
            lowercase: true,
            remove_punctuation: true,
            ..Default::default()
        };
        let vocabulary = snapshot(&["nimbus-files", "OpenRouter"], false);
        assert_eq!(
            vocabulary
                .restore(&settings.process("Use Nimbus-Files e OPENROUTER!"))
                .text,
            "use nimbus-files e OpenRouter"
        );
        let mut source = Vocabulary {
            terms: vec!["nimbus-files".into()],
            ..Default::default()
        };
        let saved = Snapshot::new(source.clone());
        source.terms[0] = "OtherProject".into();
        assert_eq!(saved.restore("nimbusfiles").text, "nimbus-files");
        assert_eq!(Snapshot::default().restore(" A\nB ").text, " A\nB ");
    }
    #[test]
    fn validation_bounds_names_and_rejects_ambiguous_canonical_forms() {
        let duplicate = Vocabulary {
            terms: vec!["nimbus-files".into(), "Nimbus Files".into()],
            ..Default::default()
        };
        assert!(duplicate.validate().is_err());
        for name in ["", "hello\nworld", "x@host", "../secret", "   "] {
            assert!(
                Vocabulary {
                    terms: vec![name.into()],
                    ..Default::default()
                }
                .validate()
                .is_err()
            );
        }
        let vocabulary = snapshot(
            &(0..100)
                .map(|i| format!("Project{i}"))
                .collect::<Vec<_>>()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            false,
        );
        let terms = vocabulary.remote_terms(50, 400);
        assert!(terms.len() <= 50);
        assert!(terms.iter().map(|name| name.len() + 1).sum::<usize>() <= 400);
    }
}
