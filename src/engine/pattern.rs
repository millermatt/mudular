//! Plain patterns: what a rule's `pattern:` means (docs/ARCHITECTURE.md §7.1).
//!
//! A plain pattern is literal text with two pieces of syntax — `{name}`
//! captures and binds `${name}`, `*` captures positionally — and it compiles
//! *to* a regex here, at load. Everything downstream of `compile` still sees
//! an ordinary `regex::Regex`, so `captured()`, `${...}` expansion,
//! `highlight:` and `when:` guards are untouched by the existence of this
//! module. `regex:` skips the translation and is handed to `regex::Regex`
//! verbatim.

use std::fmt::Write as _;

/// Where the translated pattern is allowed to match. An alias is a whole
/// command the player typed — nobody writing `hh` means "any command
/// containing hh" — while a trigger is looking for something inside a line
/// the server sent, which is usually surrounded by text the author never
/// mentioned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchor {
    /// Anchored at both ends: the pattern must be the entire input.
    Whole,
    /// Unanchored: the pattern may match anywhere in the line.
    Substring,
}

/// Translates a plain pattern into regex source, or explains why it can't.
///
/// The reason a translation failure is an error rather than a fallback to
/// literal text is that every failure here is a typo in something the author
/// meant as syntax — an unclosed `{`, an empty `{}`, a name repeated. Left
/// alone they compile to a pattern that still matches *something*, which is
/// exactly the silent wrongness plain patterns exist to remove.
pub fn translate(plain: &str, anchor: Anchor) -> Result<String, String> {
    let mut parts: Vec<Part> = Vec::new();
    let mut literal = String::new();
    let mut names: Vec<String> = Vec::new();
    let mut chars = plain.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            // A backslash makes the next character literal. Something has to:
            // `*` and `{` are syntax now, and a MUD that announces a death as
            // `*** You have died ***` would otherwise be unmatchable without
            // dropping to `regex:`.
            '\\' => match chars.next() {
                Some(c) => literal.push_str(&regex::escape(&c.to_string())),
                None => {
                    return Err(
                        r"a trailing `\` escapes nothing — write `\\` for a literal `\`".into(),
                    );
                }
            },
            '}' => return Err(r"`}` with no `{` before it — write `\}` for a literal `}`".into()),
            '{' => {
                let mut name = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(c) => name.push(c),
                        None => {
                            return Err(format!(
                                "`{{{name}` is never closed — add `}}`, or write `\\{{` for a \
                                 literal `{{`"
                            ));
                        }
                    }
                }
                validate_name(&name)?;
                if names.contains(&name) {
                    return Err(format!(
                        "`{{{name}}}` appears twice; a capture name can only be used once"
                    ));
                }
                names.push(name.clone());
                parts.push(Part::Literal(std::mem::take(&mut literal)));
                parts.push(Part::Capture(Some(name)));
            }
            '*' => {
                parts.push(Part::Literal(std::mem::take(&mut literal)));
                parts.push(Part::Capture(None));
            }
            c => literal.push_str(&regex::escape(&c.to_string())),
        }
    }
    parts.push(Part::Literal(literal));

    let last_capture = parts.iter().rposition(|p| matches!(p, Part::Capture(_)));
    let trailing = parts
        .iter()
        .skip(last_capture.map_or(usize::MAX, |i| i + 1))
        .all(|p| matches!(p, Part::Literal(text) if text.is_empty()));

    let mut out = String::with_capacity(plain.len() * 2);
    if anchor == Anchor::Whole {
        out.push('^');
    }
    for (index, part) in parts.iter().enumerate() {
        match part {
            Part::Literal(text) => out.push_str(text),
            // A capture with literal text after it stops at that text, so it
            // is non-greedy: `{who} tells you '{text}'` must not let `{who}`
            // swallow the whole line up to the last quote. A capture with
            // nothing after it has nothing to stop at, and non-greedy would
            // make it match one character — so the last one is greedy.
            Part::Capture(name) => {
                let greedy = trailing && Some(index) == last_capture;
                let repeat = match greedy {
                    true => ".+",
                    false => ".+?",
                };
                match name {
                    Some(name) => {
                        let _ = write!(out, "(?P<{name}>{repeat})");
                    }
                    None => {
                        let _ = write!(out, "({repeat})");
                    }
                }
            }
        }
    }
    if anchor == Anchor::Whole {
        out.push('$');
    }
    Ok(out)
}

enum Part {
    Literal(String),
    Capture(Option<String>),
}

/// Escapes text so a plain pattern matches it verbatim — what picking a line
/// out of the scrollback into a new trigger needs (§10.2). Three characters
/// carry meaning; the rest of the line stays as the player saw it, which is
/// the difference between a prefilled pattern they can read and the
/// regex-escaped one they used to get.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if matches!(ch, '\\' | '*' | '{' | '}') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// The regex crate's own rule for a group name, restated as advice: a name
/// it rejects would otherwise surface as a regex error about source the
/// author never wrote.
fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("`{}` needs a name — write `*` for a capture without one".into());
    }
    let mut chars = name.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    if !first_ok || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!(
            "`{{{name}}}` is not a usable name: letters, digits and `_` only, not starting with \
             a digit"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;

    fn trigger(plain: &str) -> Regex {
        Regex::new(&translate(plain, Anchor::Substring).expect("translates")).expect("compiles")
    }

    fn alias(plain: &str) -> Regex {
        Regex::new(&translate(plain, Anchor::Whole).expect("translates")).expect("compiles")
    }

    /// The point of the whole feature: `.` means `.`, and an author who
    /// forgets a backslash they never had to write cannot be quietly wrong.
    #[test]
    fn a_dot_is_a_dot_and_not_any_character() {
        let re = trigger("Bob has arrived.");
        assert!(re.is_match("Bob has arrived."));
        assert!(!re.is_match("Bob has arrived!"));
    }

    #[test]
    fn regex_metacharacters_are_literal_text() {
        let re = trigger("(2 gold) [tagged] $50 a+b|c ^x");
        assert!(re.is_match("You see (2 gold) [tagged] $50 a+b|c ^x here."));
    }

    #[test]
    fn a_named_capture_binds_the_placeholder_of_the_same_name() {
        let caps = trigger("{who} has arrived.")
            .captures("Bob has arrived.")
            .expect("matches");
        assert_eq!(&caps["who"], "Bob");
    }

    #[test]
    fn a_star_captures_positionally() {
        let caps = trigger("* tells you '*'")
            .captures("Bob tells you 'hello there'")
            .expect("matches");
        assert_eq!(caps.get(1).map(|m| m.as_str()), Some("Bob"));
        assert_eq!(caps.get(2).map(|m| m.as_str()), Some("hello there"));
    }

    /// The greediness question #80 asked to pin down before it became
    /// folklore: a capture stops at the literal that follows it.
    #[test]
    fn a_capture_stops_at_the_text_that_follows_it() {
        let caps = trigger("{who} tells you '{text}'")
            .captures("Bob tells you 'hi' and 'bye'")
            .expect("matches");
        assert_eq!(&caps["who"], "Bob");
        assert_eq!(&caps["text"], "hi");
    }

    /// ...and a capture with nothing after it takes the rest of the line,
    /// rather than the one character non-greedy would settle for.
    #[test]
    fn a_trailing_capture_takes_the_rest_of_the_line() {
        let caps = alias("gh {message}")
            .captures("gh on my way")
            .expect("matches");
        assert_eq!(&caps["message"], "on my way");
    }

    #[test]
    fn a_trigger_matches_inside_a_longer_line() {
        assert!(trigger("is DEAD").is_match("The orc is DEAD!! R.I.P."));
    }

    /// An alias is a whole command: `hh` is not "any command containing hh",
    /// which is what makes the anchoring differ from a trigger's.
    #[test]
    fn an_alias_must_match_the_whole_command() {
        let re = alias("hh");
        assert!(re.is_match("hh"));
        assert!(!re.is_match("shh"));
        assert!(!re.is_match("hh now"));
    }

    #[test]
    fn a_backslash_makes_syntax_literal() {
        let re = trigger(r"\{gossip\} {who}: \*hi\*");
        let caps = re.captures("{gossip} Bob: *hi*").expect("matches");
        assert_eq!(&caps["who"], "Bob");
    }

    /// The line MUDs really do send, and the reason plain patterns need an
    /// escape at all.
    #[test]
    fn a_line_of_asterisks_can_be_matched_literally() {
        let re = trigger(&escape("*** You have died ***"));
        assert!(re.is_match("*** You have died ***"));
        assert!(!re.is_match("*** You have won ***"));
    }

    #[test]
    fn escaping_a_line_makes_it_match_itself_and_stay_readable() {
        let line = r"Bob {the *smith*} says: 100% \o/";
        let escaped = escape(line);
        assert!(trigger(&escaped).is_match(line));
        assert!(
            escaped.contains("Bob") && escaped.contains("says: 100%"),
            "only syntax is escaped, so the line stays legible: {escaped}"
        );
    }

    #[test]
    fn a_trailing_backslash_escapes_nothing_and_says_so() {
        let err = translate(r"50% off\", Anchor::Substring).expect_err("rejected");
        assert!(err.contains("escapes nothing"), "{err}");
    }

    #[test]
    fn an_unclosed_brace_is_an_error_not_a_literal() {
        let err = translate("{who has arrived", Anchor::Substring).expect_err("rejected");
        assert!(err.contains("never closed"), "{err}");
    }

    #[test]
    fn a_stray_closing_brace_is_an_error() {
        let err = translate("100} gold", Anchor::Substring).expect_err("rejected");
        assert!(err.contains("no `{`"), "{err}");
    }

    #[test]
    fn an_empty_capture_name_points_at_the_star() {
        let err = translate("{} gold", Anchor::Substring).expect_err("rejected");
        assert!(err.contains('*'), "{err}");
    }

    #[test]
    fn a_name_the_regex_engine_would_reject_is_rejected_here_instead() {
        let err = translate("{who is here} gold", Anchor::Substring).expect_err("rejected");
        assert!(err.contains("not a usable name"), "{err}");
        let digit = translate("{1st}", Anchor::Substring).expect_err("rejected");
        assert!(digit.contains("not a usable name"), "{digit}");
    }

    /// Two groups of one name is a regex compile error several layers away
    /// from the line the author wrote; catching it here names the cause.
    #[test]
    fn a_repeated_capture_name_is_rejected_where_it_was_written() {
        let err = translate("{who} hits {who}", Anchor::Substring).expect_err("rejected");
        assert!(err.contains("twice"), "{err}");
    }

    /// Whatever the translation produces has to be a pattern the regex crate
    /// will actually take, for any input at all — a translated pattern that
    /// fails to compile would report regex source the author never wrote.
    #[test]
    fn every_translation_that_succeeds_compiles() {
        for plain in [
            "",
            "*",
            "{a}",
            "**",
            "{a}{b}",
            r"\d+ gold",
            "[",
            "(?P<x>",
            "a.b*c+d?e|f",
            r"\\",
            r"\{",
            "* has arrived.",
        ] {
            for anchor in [Anchor::Substring, Anchor::Whole] {
                let source = translate(plain, anchor)
                    .unwrap_or_else(|err| panic!("{plain:?} did not translate: {err}"));
                Regex::new(&source)
                    .unwrap_or_else(|err| panic!("{plain:?} became invalid regex: {err}"));
            }
        }
    }
}
