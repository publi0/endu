//! Deterministic, opt-in formatting. One snapshot belongs to each dictation.

use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};
use unicode_properties::{GeneralCategoryGroup, UnicodeGeneralCategory};
use unicode_segmentation::UnicodeSegmentation;

static PREFERENCES: AtomicU8 = AtomicU8::new(0);

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Preferences {
    pub lowercase: bool,
    pub lowercase_initial: bool,
    pub remove_punctuation: bool,
    pub remove_ellipses: bool,
    pub remove_final_period: bool,
    pub collapse_spaces: bool,
    pub single_line: bool,
}

impl Preferences {
    pub fn current() -> Self {
        Self::decode(PREFERENCES.load(Ordering::Acquire))
    }

    pub fn apply_runtime(self) {
        PREFERENCES.store(self.encode(), Ordering::Release);
    }

    fn encode(self) -> u8 {
        u8::from(self.lowercase)
            | (u8::from(self.lowercase_initial) << 1)
            | (u8::from(self.remove_punctuation) << 2)
            | (u8::from(self.remove_ellipses) << 3)
            | (u8::from(self.remove_final_period) << 4)
            | (u8::from(self.collapse_spaces) << 5)
            | (u8::from(self.single_line) << 6)
    }

    fn decode(bits: u8) -> Self {
        Self {
            lowercase: bits & 1 != 0,
            lowercase_initial: bits & 2 != 0,
            remove_punctuation: bits & 4 != 0,
            remove_ellipses: bits & 8 != 0,
            remove_final_period: bits & 16 != 0,
            collapse_spaces: bits & 32 != 0,
            single_line: bits & 64 != 0,
        }
    }

    pub fn controls_initial_case(self) -> bool {
        self.lowercase || self.lowercase_initial
    }

    pub fn process(self, text: &str) -> String {
        let mut text = if self.remove_punctuation {
            remove_runs(text, |character| {
                character.general_category_group() == GeneralCategoryGroup::Punctuation
            })
        } else if self.remove_ellipses {
            remove_ellipses(text)
        } else {
            text.to_owned()
        };
        if self.remove_final_period && !self.remove_punctuation {
            remove_final_period(&mut text);
        }
        if self.single_line {
            text = text
                .split(is_line_break)
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
        }
        if self.collapse_spaces {
            let mut space = false;
            text = text
                .chars()
                .filter_map(|character| {
                    if character.is_whitespace() && !is_line_break(character) {
                        let append = !space;
                        space = true;
                        append.then_some(' ')
                    } else {
                        space = false;
                        Some(character)
                    }
                })
                .collect();
        }
        if self.lowercase {
            text = text.to_lowercase();
        } else if self.lowercase_initial
            && let Some((index, character)) = text.char_indices().find(|(_, c)| c.is_alphabetic())
        {
            text.replace_range(
                index..index + character.len_utf8(),
                &character.to_lowercase().collect::<String>(),
            );
        }
        text
    }
}

fn is_line_break(character: char) -> bool {
    matches!(
        character,
        '\r' | '\n' | '\u{0085}' | '\u{2028}' | '\u{2029}'
    )
}

// Avoid joining words when the removed punctuation was their only separator.
fn remove_runs(text: &str, removed: impl Fn(char) -> bool) -> String {
    let mut result = String::with_capacity(text.len());
    let mut removed_since_text = false;
    for grapheme in text.graphemes(true) {
        let character = grapheme.chars().next().expect("nonempty grapheme");
        if removed(character) && !grapheme.ends_with('\u{20e3}') {
            removed_since_text = true;
        } else {
            if removed_since_text
                && word_character(character)
                && result.chars().next_back().is_some_and(word_character)
            {
                result.push(' ');
            }
            result.push_str(grapheme);
            removed_since_text = false;
        }
    }
    result
}

fn word_character(character: char) -> bool {
    character.is_alphanumeric()
        || matches!(
            character.general_category_group(),
            GeneralCategoryGroup::Mark | GeneralCategoryGroup::Symbol
        )
}

fn remove_ellipses(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut removed = false;
    while let Some(character) = chars.next() {
        if character == '…' {
            removed = true;
            continue;
        }
        if character == '.' {
            let after_first = chars.clone();
            let mut count = 1;
            loop {
                let mut probe = chars.clone();
                while probe
                    .peek()
                    .is_some_and(|c| c.is_whitespace() && !is_line_break(*c))
                {
                    probe.next();
                }
                if probe.next() != Some('.') {
                    break;
                }
                chars = probe;
                count += 1;
            }
            if count >= 3 {
                removed = true;
                continue;
            }
            chars = after_first;
            result.push('.');
            removed = false;
            continue;
        }
        if removed
            && word_character(character)
            && result.chars().next_back().is_some_and(word_character)
        {
            result.push(' ');
        }
        result.push(character);
        removed = false;
    }
    result
}

fn remove_final_period(text: &mut String) {
    let Some((index, character)) = text
        .char_indices()
        .rev()
        .find(|(_, character)| !trailing_closer(*character))
    else {
        return;
    };
    if matches!(character, '.' | '。' | '．')
        && !text[..index]
            .trim_end_matches(trailing_closer)
            .ends_with(['.', '。', '．'])
    {
        text.remove(index);
    }
}

fn trailing_closer(character: char) -> bool {
    character.is_whitespace()
        || matches!(
            character,
            '"' | '\'' | '”' | '’' | '»' | '›' | ')' | ']' | '}' | '」' | '』' | '】'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_preferences_preserve_every_byte_and_old_settings_default_off() {
        let text = "  “Olá, João...”\r\n\tNASA custa € 3,50! 👩🏽‍💻  ";
        assert_eq!(Preferences::default().process(text), text);
        assert_eq!(
            serde_json::from_str::<Preferences>("{}").unwrap(),
            Preferences::default()
        );
        for bits in 0..128 {
            assert_eq!(Preferences::decode(bits).encode(), bits);
        }
    }

    #[test]
    fn casing_handles_accents_quotes_and_multicharacter_lowercase() {
        let initial = Preferences {
            lowercase_initial: true,
            ..Preferences::default()
        };
        assert_eq!(
            initial.process("“Olá, João e NASA.”"),
            "“olá, João e NASA.”"
        );
        assert_eq!(initial.process("İSTANBUL"), "i\u{307}STANBUL");
        assert_eq!(
            Preferences {
                lowercase: true,
                ..initial
            }
            .process("ÁRVORE, João!"),
            "árvore, joão!"
        );
    }

    #[test]
    fn punctuation_keeps_unicode_letters_symbols_and_word_boundaries() {
        let preferences = Preferences {
            remove_punctuation: true,
            ..Preferences::default()
        };
        assert_eq!(
            preferences.process("“Olá,João!” — café; árvore。"),
            "Olá João  café árvore"
        );
        assert_eq!(
            preferences.process("e-mail 3,50 C++ € 👩🏽‍💻"),
            "e mail 3 50 C++ € 👩🏽‍💻"
        );
        assert_eq!(preferences.process("...?!"), "");
        assert_eq!(
            preferences.process("cafe\u{301},mundo #️⃣"),
            "cafe\u{301} mundo #️⃣"
        );
    }

    #[test]
    fn ellipses_and_terminal_period_are_independent() {
        let ellipses = Preferences {
            remove_ellipses: true,
            ..Preferences::default()
        };
        assert_eq!(
            ellipses.process("Olá...João… Tudo bem."),
            "Olá João Tudo bem."
        );
        assert_eq!(ellipses.process("3.14 e dois.."), "3.14 e dois..");
        assert_eq!(ellipses.process("Olá. . . João."), "Olá João.");
        let period = Preferences {
            remove_final_period: true,
            ..Preferences::default()
        };
        assert_eq!(period.process("“Olá. Tudo bem.”  "), "“Olá. Tudo bem”  ");
        assert_eq!(period.process("Espere..."), "Espere...");
        assert_eq!(period.process("Tudo bem?"), "Tudo bem?");
        assert_eq!(period.process("1.25"), "1.25");
    }

    #[test]
    fn whitespace_options_preserve_or_join_paragraphs_explicitly() {
        let spaces = Preferences {
            collapse_spaces: true,
            ..Preferences::default()
        };
        assert_eq!(
            spaces.process("Olá\t  João\r\n\r\nTudo  bem"),
            "Olá João\r\n\r\nTudo bem"
        );
        let line = Preferences {
            single_line: true,
            ..Preferences::default()
        };
        assert_eq!(
            line.process("Olá  João\r\n\r\n Tudo\u{2028}bem"),
            "Olá  João Tudo bem"
        );
    }

    #[test]
    fn every_combination_is_idempotent() {
        for bits in 0..128 {
            let preferences = Preferences::decode(bits);
            for input in [
                "“Olá...  João.”\nTudo BEM.",
                "ÁRVORE, café; NASA?!",
                "...",
                "",
                "(Oi...)",
                "3.14",
                "İSTANBUL",
                "Olá\t\t🙂 mundo…",
            ] {
                let once = preferences.process(input);
                assert_eq!(
                    preferences.process(&once),
                    once,
                    "bits={bits}, input={input:?}"
                );
            }
        }
    }
}
