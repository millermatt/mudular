//! `when:` rule guards (docs/ARCHITECTURE.md §7.6).
//!
//! A pattern says *what* matched; a condition says *whether to act on it*.
//! The grammar is deliberately small — comparison, `and`/`or`/`not`,
//! parentheses, number and string literals, and `${...}` terms. Anything
//! with memory, arithmetic, or multi-step logic is a script's job (§7.4),
//! and this stays first-party so `${hp} < 40` means one thing in every
//! build, whatever script engine is compiled in.
//!
//! Sans-IO and allocation-light: a condition compiles once, at
//! [`super::Engine::compile`], and matching then costs one tree walk.

use std::collections::HashMap;

use regex::Captures;

/// A compiled `when:` guard.
#[derive(Debug)]
pub struct Condition {
    root: Node,
}

impl Condition {
    /// Parses a guard, or explains why it is not one. Called at compile
    /// time so a malformed condition fails at load with module context,
    /// exactly like an invalid pattern, rather than silently never firing.
    pub fn parse(src: &str) -> Result<Self, String> {
        let tokens = lex(src)?;
        if tokens.is_empty() {
            return Err("condition is empty".to_string());
        }
        let mut parser = Parser { tokens, pos: 0 };
        let root = parser.expression()?;
        if parser.pos != parser.tokens.len() {
            return Err("trailing input after the condition".to_string());
        }
        Ok(Condition { root })
    }

    /// Whether the rule may fire. Terms resolve through the same order as
    /// `send:` expansion (§7.1) — captures, variables, then server data.
    ///
    /// An undefined name makes the whole condition false, including under
    /// `not`: §7.6 specifies the *condition* as false, and a `not` that
    /// turned an unresolvable term into a reason to fire would be the
    /// unsafe reading of that. Unresolved terms therefore propagate as
    /// [`None`] rather than as a boolean.
    pub fn eval(
        &self,
        caps: Option<&Captures>,
        vars: &HashMap<String, String>,
        server_data: &HashMap<String, String>,
    ) -> bool {
        self.root.eval(caps, vars, server_data).unwrap_or(false)
    }
}

#[derive(Debug)]
enum Node {
    Cmp { lhs: Term, op: CmpOp, rhs: Term },
    And(Box<Node>, Box<Node>),
    Or(Box<Node>, Box<Node>),
    Not(Box<Node>),
}

/// A comparison operand. `${...}` is a term, never a textual substitution:
/// splicing a value in before parsing would let untrusted server data
/// (§13) inject operators into the client's own predicate — a value of
/// `0 or 1==1` would rewrite the condition rather than be compared by it.
#[derive(Debug)]
enum Term {
    Literal(String),
    Ref(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CmpOp {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
}

impl Node {
    fn eval(
        &self,
        caps: Option<&Captures>,
        vars: &HashMap<String, String>,
        server_data: &HashMap<String, String>,
    ) -> Option<bool> {
        match self {
            Node::Cmp { lhs, op, rhs } => {
                let lhs = lhs.resolve(caps, vars, server_data)?;
                let rhs = rhs.resolve(caps, vars, server_data)?;
                Some(op.apply(&lhs, &rhs))
            }
            Node::And(a, b) => {
                Some(a.eval(caps, vars, server_data)? && b.eval(caps, vars, server_data)?)
            }
            Node::Or(a, b) => {
                Some(a.eval(caps, vars, server_data)? || b.eval(caps, vars, server_data)?)
            }
            Node::Not(inner) => Some(!inner.eval(caps, vars, server_data)?),
        }
    }
}

impl Term {
    fn resolve(
        &self,
        caps: Option<&Captures>,
        vars: &HashMap<String, String>,
        server_data: &HashMap<String, String>,
    ) -> Option<String> {
        match self {
            Term::Literal(value) => Some(value.clone()),
            Term::Ref(name) => super::lookup(name, caps, vars, server_data),
        }
    }
}

impl CmpOp {
    /// Compare numerically when both sides parse as numbers, lexically
    /// otherwise: everything in the variable and server-data stores is a
    /// string, so `${hp} < 40` must not become a string comparison.
    fn apply(self, lhs: &str, rhs: &str) -> bool {
        match (lhs.trim().parse::<f64>(), rhs.trim().parse::<f64>()) {
            (Ok(a), Ok(b)) => match self {
                CmpOp::Lt => a < b,
                CmpOp::Le => a <= b,
                CmpOp::Gt => a > b,
                CmpOp::Ge => a >= b,
                CmpOp::Eq => a == b,
                CmpOp::Ne => a != b,
            },
            _ => match self {
                CmpOp::Lt => lhs < rhs,
                CmpOp::Le => lhs <= rhs,
                CmpOp::Gt => lhs > rhs,
                CmpOp::Ge => lhs >= rhs,
                CmpOp::Eq => lhs == rhs,
                CmpOp::Ne => lhs != rhs,
            },
        }
    }
}

#[derive(Debug, PartialEq)]
enum Token {
    Literal(String),
    Ref(String),
    Op(CmpOp),
    And,
    Or,
    Not,
    LParen,
    RParen,
}

fn lex(src: &str) -> Result<Vec<Token>, String> {
    let mut out = Vec::new();
    let mut rest = src;

    loop {
        rest = rest.trim_start();
        let Some(c) = rest.chars().next() else { break };
        match c {
            '(' => {
                out.push(Token::LParen);
                rest = &rest[1..];
            }
            ')' => {
                out.push(Token::RParen);
                rest = &rest[1..];
            }
            '\'' | '"' => {
                let body = &rest[c.len_utf8()..];
                let end = body
                    .find(c)
                    .ok_or_else(|| format!("unterminated string literal starting at `{rest}`"))?;
                out.push(Token::Literal(body[..end].to_string()));
                rest = &body[end + c.len_utf8()..];
            }
            '$' => {
                let body = rest
                    .strip_prefix("${")
                    .ok_or_else(|| "`$` must start a `${name}` term".to_string())?;
                let end = body
                    .find('}')
                    .ok_or_else(|| "unterminated `${` term".to_string())?;
                out.push(Token::Ref(body[..end].to_string()));
                rest = &body[end + 1..];
            }
            '<' | '>' | '=' | '!' => {
                let (op, len) = if let Some(op) = two_char_op(rest) {
                    (op, 2)
                } else if c == '<' {
                    (CmpOp::Lt, 1)
                } else if c == '>' {
                    (CmpOp::Gt, 1)
                } else {
                    return Err(format!(
                        "`{c}` is not an operator on its own; use ==, !=, <, <=, >, or >="
                    ));
                };
                out.push(Token::Op(op));
                rest = &rest[len..];
            }
            // No arithmetic in the grammar, so a leading `-` is always a
            // negative number rather than subtraction.
            '-' | '0'..='9' => {
                let end = rest[c.len_utf8()..]
                    .find(|ch: char| !ch.is_ascii_digit() && ch != '.')
                    .map_or(rest.len(), |offset| offset + c.len_utf8());
                out.push(Token::Literal(rest[..end].to_string()));
                rest = &rest[end..];
            }
            c if c.is_alphabetic() || c == '_' => {
                let end = rest
                    .find(|ch: char| !ch.is_alphanumeric() && ch != '_')
                    .unwrap_or(rest.len());
                out.push(match &rest[..end] {
                    "and" => Token::And,
                    "or" => Token::Or,
                    "not" => Token::Not,
                    other => {
                        return Err(format!(
                            "unknown word `{other}`; a condition takes `${{name}}` terms, \
                             quoted strings, numbers, and and/or/not"
                        ));
                    }
                });
                rest = &rest[end..];
            }
            other => return Err(format!("unexpected character `{other}`")),
        }
    }

    Ok(out)
}

fn two_char_op(rest: &str) -> Option<CmpOp> {
    match rest.get(..2)? {
        "<=" => Some(CmpOp::Le),
        ">=" => Some(CmpOp::Ge),
        "==" => Some(CmpOp::Eq),
        "!=" => Some(CmpOp::Ne),
        _ => None,
    }
}

/// Recursive descent, lowest precedence first: `or` binds looser than
/// `and`, which binds looser than `not`, which wraps a comparison or a
/// parenthesised group.
struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn expression(&mut self) -> Result<Node, String> {
        let mut node = self.conjunction()?;
        while self.eat(&Token::Or) {
            node = Node::Or(Box::new(node), Box::new(self.conjunction()?));
        }
        Ok(node)
    }

    fn conjunction(&mut self) -> Result<Node, String> {
        let mut node = self.negation()?;
        while self.eat(&Token::And) {
            node = Node::And(Box::new(node), Box::new(self.negation()?));
        }
        Ok(node)
    }

    fn negation(&mut self) -> Result<Node, String> {
        if self.eat(&Token::Not) {
            return Ok(Node::Not(Box::new(self.negation()?)));
        }
        if self.eat(&Token::LParen) {
            let node = self.expression()?;
            if !self.eat(&Token::RParen) {
                return Err("missing `)`".to_string());
            }
            return Ok(node);
        }
        self.comparison()
    }

    /// A bare term is not a condition. `when: '${combat}'` has no defined
    /// truthiness in a store where every value is a string, so it is a
    /// load-time error rather than a rule that quietly never fires.
    fn comparison(&mut self) -> Result<Node, String> {
        let lhs = self.term()?;
        let op = match self.tokens.get(self.pos) {
            Some(Token::Op(op)) => *op,
            _ => return Err("expected a comparison (==, !=, <, <=, >, >=)".to_string()),
        };
        self.pos += 1;
        let rhs = self.term()?;
        Ok(Node::Cmp { lhs, op, rhs })
    }

    fn term(&mut self) -> Result<Term, String> {
        let term = match self.tokens.get(self.pos) {
            Some(Token::Literal(value)) => Term::Literal(value.clone()),
            Some(Token::Ref(name)) => Term::Ref(name.clone()),
            _ => return Err("expected a `${name}` term, a number, or a quoted string".to_string()),
        };
        self.pos += 1;
        Ok(term)
    }

    fn eat(&mut self, token: &Token) -> bool {
        if self.tokens.get(self.pos) == Some(token) {
            self.pos += 1;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn eval(src: &str, pairs: &[(&str, &str)]) -> bool {
        Condition::parse(src).expect("condition should parse").eval(
            None,
            &vars(pairs),
            &HashMap::new(),
        )
    }

    #[test]
    fn compares_a_variable_against_a_number() {
        assert!(eval("${hp} < 40", &[("hp", "30")]));
        assert!(!eval("${hp} < 40", &[("hp", "50")]));
    }

    #[test]
    fn compares_numerically_when_both_sides_are_numbers() {
        // Lexically "9" > "40"; numerically it is not. Every store value is
        // a string, so this is the coercion that matters most (§7.6).
        assert!(eval("${hp} < 40", &[("hp", "9")]));
        assert!(eval(
            "${hp} < ${heal_at}",
            &[("hp", "9"), ("heal_at", "40")]
        ));
    }

    #[test]
    fn compares_lexically_when_either_side_is_not_a_number() {
        assert!(eval("${who} == 'tank'", &[("who", "tank")]));
        assert!(eval("${who} != 'tank'", &[("who", "cleric")]));
        assert!(eval("${who} < 'tank'", &[("who", "cleric")]));
    }

    #[test]
    fn handles_negative_and_fractional_numbers() {
        assert!(eval("${bal} < -1", &[("bal", "-20")]));
        assert!(eval("${ratio} > 0.5", &[("ratio", "0.75")]));
    }

    #[test]
    fn combines_terms_with_and_or_and_not() {
        let store = &[("hp", "30"), ("mana", "10")];
        assert!(eval("${hp} < 40 and ${mana} < 20", store));
        assert!(!eval("${hp} < 40 and ${mana} > 20", store));
        assert!(eval("${hp} > 40 or ${mana} < 20", store));
        assert!(eval("not ${hp} > 40", store));
    }

    #[test]
    fn and_binds_tighter_than_or() {
        // `false and false or true` is true only if `and` binds first.
        assert!(eval(
            "${hp} > 90 and ${hp} > 90 or ${hp} < 40",
            &[("hp", "30")]
        ));
        assert!(!eval(
            "(${hp} > 90 or ${hp} > 90) and ${hp} < 40",
            &[("hp", "30")]
        ));
    }

    #[test]
    fn an_undefined_name_makes_the_condition_false() {
        assert!(!eval("${hp} < 40", &[]));
        // Including under `or`, where the other side alone would fire...
        assert!(!eval("${hp} < 40 or ${mana} < 40", &[("mana", "10")]));
        // ...and under `not`, which must not turn "unresolvable" into
        // "fire". §7.6 makes the whole condition false, not the subterm.
        assert!(!eval("not ${hp} < 40", &[]));
    }

    #[test]
    fn a_term_is_never_spliced_in_as_text() {
        // Server data (§13) that looks like a predicate is compared as a
        // value, not parsed as operators that rewrite the guard. Spliced
        // in as text, `99 or 1==1 < 40` would be true; compared as the
        // string it is, it is not.
        assert!(!eval("${hp} < 40", &[("hp", "99 or 1==1")]));
        assert!(eval("${hp} == '99 or 1==1'", &[("hp", "99 or 1==1")]));
    }

    #[test]
    fn resolves_captures_before_variables() {
        let regex = regex::Regex::new(r"health: (?P<hp>\d+)").expect("test regex");
        let caps = regex.captures("health: 12").expect("test line matches");
        let condition = Condition::parse("${hp} < 40").expect("condition should parse");
        assert!(condition.eval(Some(&caps), &vars(&[("hp", "99")]), &HashMap::new()));
    }

    #[test]
    fn falls_back_to_server_data() {
        let condition = Condition::parse("${Char.Vitals.hp} < 40").expect("condition should parse");
        let server_data = vars(&[("Char.Vitals.hp", "12")]);
        assert!(condition.eval(None, &HashMap::new(), &server_data));
    }

    #[test]
    fn rejects_malformed_conditions_at_parse_time() {
        for src in [
            "",
            "${hp}",              // a bare term has no truthiness
            "${hp} <",            // missing operand
            "${hp} = 40",         // `=` is not an operator
            "${hp} < 40 and",     // dangling connective
            "(${hp} < 40",        // unbalanced
            "${hp} < 40)",        // unbalanced the other way
            "${hp} lt 40",        // unknown word
            "${hp < 40",          // unterminated term
            "${hp} == 'tank",     // unterminated string
            "${hp} < 40 ${mana}", // trailing input
        ] {
            assert!(
                Condition::parse(src).is_err(),
                "`{src}` should not parse as a condition"
            );
        }
    }
}
