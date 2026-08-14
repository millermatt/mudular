//! Completing what you type from what the MUD just said
//! (docs/ARCHITECTURE.md §11.3).
//!
//! A MUD client can only complete what it knows the names of, and the
//! protocols do not say: GMCP's `Room.Chars` would name what is in the room
//! but almost nothing sends it, and MSDP has no room contents at all. What
//! every MUD does do is *print* the names — "A bullywug is here." — so the
//! screen is the vocabulary, and no per-MUD parsing is needed to read it.
//!
//! Recency does the rest. The room description is the most recent thing on
//! screen, so its words outrank an hour-old `bullhorn` for free, without
//! anything here knowing what a room is. That is also why this is a plain
//! recency list rather than a set: the order *is* the ranking.

use std::collections::VecDeque;

/// How many words are remembered. A few thousand covers the last several
/// screens, which is the horizon over which "recent" means anything — a
/// name from an hour ago competing with what is in the room now is the
/// failure mode, not a feature.
const CAPACITY: usize = 2000;

/// The shortest thing worth completing. Two characters name too much of a
/// MUD's vocabulary to guess from, and this is a guess that changes what
/// gets sent.
pub const MIN_PREFIX: usize = 3;

/// Words seen recently, oldest first.
///
/// Duplicates are kept rather than deduplicated on the way in: an entry
/// pushed again is exactly how a word becomes recent again, and the newest
/// copy is the one a search from the back finds first.
#[derive(Debug, Default)]
pub struct Vocabulary {
    words: VecDeque<String>,
}

impl Vocabulary {
    /// Takes the words out of a line the MUD printed.
    pub fn learn(&mut self, line: &str) {
        for word in line.split(|c: char| !is_word_char(c)) {
            if word.chars().count() < MIN_PREFIX {
                continue;
            }
            // A number is never what someone is half-way through typing —
            // damage totals and coin counts would otherwise crowd out the
            // names, which are the whole point.
            if word.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            self.words.push_back(word.to_string());
            if self.words.len() > CAPACITY {
                self.words.pop_front();
            }
        }
    }

    /// What `prefix` would complete to — the rest of the word, not the
    /// whole of it, so the caller appends rather than rewriting what was
    /// typed. `None` where there is nothing to say.
    ///
    /// The most recent word matching the prefix decides, and it decides
    /// both ways: if the MUD's latest word *is* the prefix, the answer is
    /// no completion at all. Someone who types `bat` in a room where a bat
    /// is standing means the bat, whatever `battlemage` was doing on screen
    /// a minute ago — and in a game where that line gets attacked, guessing
    /// the other way is the expensive mistake.
    pub fn suggest(&self, prefix: &str) -> Option<String> {
        if prefix.chars().count() < MIN_PREFIX {
            return None;
        }
        for word in self.words.iter().rev() {
            let Some(rest) = remainder(word, prefix) else {
                continue;
            };
            return (!rest.is_empty()).then(|| rest.to_string());
        }
        None
    }
}

/// What a MUD's names are made of. Apostrophes and hyphens are in because
/// they show up inside single names (`Sil'raen`, `half-orc`); everything
/// else — punctuation, digits' surroundings, escape residue — separates.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '\'' || c == '-'
}

/// The tail of `word` past `prefix`, ignoring case, or `None` if `word`
/// does not start with it. An empty tail means they are the same word,
/// which is a meaningful answer rather than a miss.
fn remainder<'a>(word: &'a str, prefix: &str) -> Option<&'a str> {
    let mut rest = word.char_indices();
    for wanted in prefix.chars() {
        let (_, c) = rest.next()?;
        if !c.eq_ignore_ascii_case(&wanted) && c.to_lowercase().ne(wanted.to_lowercase()) {
            return None;
        }
    }
    Some(match rest.next() {
        Some((at, _)) => &word[at..],
        None => "",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vocabulary(lines: &[&str]) -> Vocabulary {
        let mut vocabulary = Vocabulary::default();
        for line in lines {
            vocabulary.learn(line);
        }
        vocabulary
    }

    #[test]
    fn completes_a_word_the_mud_just_printed() {
        let vocabulary = vocabulary(&["A bullywug is here."]);
        assert_eq!(vocabulary.suggest("bull"), Some("ywug".to_string()));
    }

    /// The completion is the *rest* of the word: the caller appends it, so
    /// what was typed is left exactly as typed and a name the MUD prints
    /// capitalised does not rewrite a lowercase command.
    #[test]
    fn matching_ignores_case_and_the_typed_prefix_is_left_alone() {
        let vocabulary = vocabulary(&["Bullywug the Frog Knight stands here."]);
        assert_eq!(vocabulary.suggest("bull"), Some("ywug".to_string()));
    }

    /// The whole point of ranking by recency: the room you are in beats the
    /// room you were in, with nothing here knowing what a room is.
    #[test]
    fn the_most_recent_word_wins() {
        let vocabulary = vocabulary(&["A bullhorn lies here.", "A bullywug is here."]);
        assert_eq!(vocabulary.suggest("bull"), Some("ywug".to_string()));
    }

    /// The safety rule. `kill bat` in a room with a bat must not become
    /// `kill battlemage` because a battlemage was mentioned earlier.
    #[test]
    fn a_word_the_mud_printed_whole_is_not_extended() {
        let vocabulary = vocabulary(&["A battlemage waits.", "A bat flutters here."]);
        assert_eq!(vocabulary.suggest("bat"), None);
    }

    /// ...and only until the longer name is the more recent evidence.
    #[test]
    fn the_rule_reverses_when_the_longer_name_is_newer() {
        let vocabulary = vocabulary(&["A bat flutters here.", "A battlemage waits."]);
        assert_eq!(vocabulary.suggest("bat"), Some("tlemage".to_string()));
    }

    #[test]
    fn a_prefix_too_short_to_guess_from_is_not_guessed_from() {
        let vocabulary = vocabulary(&["A bullywug is here."]);
        assert_eq!(vocabulary.suggest("bu"), None);
    }

    #[test]
    fn an_unknown_prefix_completes_to_nothing() {
        let vocabulary = vocabulary(&["A bullywug is here."]);
        assert_eq!(vocabulary.suggest("gob"), None);
    }

    /// Damage totals and coin counts would otherwise crowd out the names.
    #[test]
    fn numbers_are_not_learned() {
        let vocabulary = vocabulary(&["You hit the bullywug for 1234 damage."]);
        assert_eq!(vocabulary.suggest("123"), None);
    }

    #[test]
    fn a_name_with_an_apostrophe_is_one_word() {
        let vocabulary = vocabulary(&["Sil'raen bows to you."]);
        assert_eq!(vocabulary.suggest("sil"), Some("'raen".to_string()));
    }

    /// Punctuation is a separator, so it never ends up inside a completion
    /// and cannot be sent as part of a command.
    #[test]
    fn punctuation_does_not_join_words() {
        let vocabulary = vocabulary(&["The bullywug, wounded, flees!"]);
        assert_eq!(vocabulary.suggest("bull"), Some("ywug".to_string()));
        assert_eq!(vocabulary.suggest("wou"), Some("nded".to_string()));
    }

    /// The list is bounded, and it is the *oldest* that goes.
    #[test]
    fn the_oldest_words_are_forgotten_first() {
        let mut vocabulary = vocabulary(&["A bullywug is here."]);
        for n in 0..CAPACITY {
            vocabulary.learn(&format!("word{n:04} filler filler"));
        }
        assert_eq!(vocabulary.suggest("bull"), None);
        assert!(vocabulary.words.len() <= CAPACITY);
    }
}
