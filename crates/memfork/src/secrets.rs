//! Keeping credentials out of shared memory (DESIGN §6.10).
//!
//! Memory is read by every tool on the machine and shown to people, so a key
//! pasted into it has leaked. Every write — through the tools, the command
//! line and plan files — is checked against the rules in `secret_rules.toml`
//! before anything is stored, and a match refuses the write.
//!
//! A refusal names the rule, the field and where in the field the match
//! starts. It never contains the matched text, and neither does anything
//! MemFork logs or publishes about it. A false positive is written anyway by
//! naming its rule (`allow_secret`, or `--allow-secret` on the command line).
//!
//! The rules are data and the checks are deterministic: the same text gives
//! the same answer on every OS.

use std::sync::OnceLock;

use regex_lite::Regex;
use serde::Deserialize;
use serde_json::{json, Value as Json};

/// The rules, as shipped.
const RULES: &str = include_str!("secret_rules.toml");

/// Shortest value the name-and-value rule counts.
const MIN_VALUE_CHARS: usize = 16;

/// Fewest kinds of character (lower, upper, digit, other) such a value needs.
const MIN_CLASSES: usize = 3;

/// Fewest different characters such a value needs.
const MIN_DISTINCT: usize = 10;

#[derive(Deserialize)]
struct RuleFile {
    rule: Vec<RuleSpec>,
}

#[derive(Deserialize)]
struct RuleSpec {
    id: String,
    description: String,
    pattern: String,
    #[serde(default)]
    value_group: bool,
}

/// One compiled rule.
#[derive(Debug)]
pub struct Rule {
    /// What a refusal names, and an override gives back.
    pub id: String,
    /// What it looks for, in words.
    pub description: String,
    pattern: Regex,
    value_group: bool,
}

/// Something that looked like a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The rule that matched.
    pub rule: String,
    /// What the rule looks for, in words.
    pub description: String,
    /// Which part of the write it was in, such as `value` or `tasks[2].detail`.
    pub field: String,
    /// The line of the field the match starts on, from 1.
    pub line: usize,
    /// The column on that line, in characters, from 1.
    pub column: usize,
}

impl Found {
    /// What to tell whoever tried to write it. Never the matched text.
    pub fn message(&self) -> String {
        format!(
            "`{field}` looks like it holds {what} (rule `{rule}`, line {line}, column {column}), \
             so nothing was stored: memory is shared with every tool on this machine and shown \
             to people, so keep credentials out of it. If it is not a secret, write it again \
             with allow_secret: \"{rule}\" (on the command line, --allow-secret {rule}).",
            field = self.field,
            what = self.description,
            rule = self.rule,
            line = self.line,
            column = self.column,
        )
    }

    /// The refusal as data, for answers and `--json`.
    pub fn to_json(&self) -> Json {
        json!({
            "rule": self.rule,
            "field": self.field,
            "line": self.line,
            "column": self.column,
        })
    }
}

/// Why a write was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// Something in it looked like a secret.
    Secret(Found),
    /// The override named a rule that does not exist.
    UnknownRule(String),
    /// The shipped rules did not load. A test keeps this from happening.
    Rules(String),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Secret(found) => write!(f, "{}", found.message()),
            Refused::UnknownRule(id) => write!(
                f,
                "`{id}` is not a secret rule; the rules are: {}",
                ids().join(", ")
            ),
            Refused::Rules(why) => write!(f, "the secret rules did not load: {why}"),
        }
    }
}

fn load() -> Result<Vec<Rule>, String> {
    let file: RuleFile = toml::from_str(RULES).map_err(|e| e.to_string())?;
    file.rule
        .into_iter()
        .map(|spec| {
            let pattern =
                Regex::new(&spec.pattern).map_err(|e| format!("rule `{}`: {e}", spec.id))?;
            Ok(Rule {
                id: spec.id,
                description: spec.description,
                pattern,
                value_group: spec.value_group,
            })
        })
        .collect()
}

fn rules() -> Result<&'static [Rule], Refused> {
    static LOADED: OnceLock<Result<Vec<Rule>, String>> = OnceLock::new();
    match LOADED.get_or_init(load) {
        Ok(rules) => Ok(rules),
        Err(why) => Err(Refused::Rules(why.clone())),
    }
}

/// Every rule id, in the order the rules are checked.
pub fn ids() -> Vec<String> {
    rules()
        .map(|r| r.iter().map(|r| r.id.clone()).collect())
        .unwrap_or_default()
}

/// Every rule, for `--help` and the docs.
pub fn all() -> Result<&'static [Rule], Refused> {
    rules()
}

/// Rules a write has been told to let through.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Allow(Vec<String>);

impl Allow {
    /// From an override: one rule id, or several separated by commas. An id
    /// that is not a rule is refused rather than ignored, so a typo cannot
    /// look like an override that worked.
    pub fn parse(raw: Option<&str>) -> Result<Allow, Refused> {
        let Some(raw) = raw else {
            return Ok(Allow::default());
        };
        let known = ids();
        let mut out = Vec::new();
        for id in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if !known.iter().any(|k| k == id) {
                return Err(Refused::UnknownRule(id.to_owned()));
            }
            out.push(id.to_owned());
        }
        Ok(Allow(out))
    }

    fn allows(&self, id: &str) -> bool {
        self.0.iter().any(|a| a == id)
    }
}

/// Whether a value counts for the name-and-value rule: long, and mixed
/// enough to look random rather than like a word or a path.
fn looks_random(value: &str) -> bool {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() < MIN_VALUE_CHARS {
        return false;
    }
    let mut classes = [false; 4];
    for c in &chars {
        let class = if c.is_ascii_lowercase() {
            0
        } else if c.is_ascii_uppercase() {
            1
        } else if c.is_ascii_digit() {
            2
        } else {
            3
        };
        classes[class] = true;
    }
    let mut distinct = chars.clone();
    distinct.sort_unstable();
    distinct.dedup();
    classes.iter().filter(|c| **c).count() >= MIN_CLASSES && distinct.len() >= MIN_DISTINCT
}

/// Line and column, from 1, of byte offset `at` in `text`.
fn position(text: &str, at: usize) -> (usize, usize) {
    let before = &text[..at.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, column)
}

/// Check one field of a write. The first rule, in file order, that matches
/// anywhere in it and is not allowed refuses it.
pub fn check(field: &str, text: &str, allow: &Allow) -> Result<(), Refused> {
    for rule in rules()? {
        if allow.allows(&rule.id) {
            continue;
        }
        let at = if rule.value_group {
            rule.pattern
                .captures_iter(text)
                .filter_map(|c| c.get(1))
                .find(|m| looks_random(m.as_str()))
                .map(|m| m.start())
        } else {
            rule.pattern.find(text).map(|m| m.start())
        };
        if let Some(at) = at {
            let (line, column) = position(text, at);
            return Err(Refused::Secret(Found {
                rule: rule.id.clone(),
                description: rule.description.clone(),
                field: field.to_owned(),
                line,
                column,
            }));
        }
    }
    Ok(())
}

/// Check several fields, in order.
pub fn check_all<'a>(
    fields: impl IntoIterator<Item = (String, &'a str)>,
    allow: &Allow,
) -> Result<(), Refused> {
    for (field, text) in fields {
        check(&field, text, allow)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(text: &str) -> Option<String> {
        match check("value", text, &Allow::default()) {
            Ok(()) => None,
            Err(Refused::Secret(f)) => Some(f.rule),
            Err(other) => panic!("{other}"),
        }
    }

    // Built at run time so this file never holds anything shaped like a key.
    fn fake(prefix: &str, body: &str, n: usize) -> String {
        format!("{prefix}{}", body.repeat(n))
    }

    #[test]
    fn the_shipped_rules_load() {
        let rules = all().expect("rules load");
        assert!(rules.len() >= 12);
        let mut ids: Vec<&str> = rules.iter().map(|r| r.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), rules.len(), "two rules share an id");
    }

    #[test]
    fn each_rule_catches_its_shape() {
        let cases = [
            (
                format!("-----BEGIN {}PRIVATE KEY-----\nabc", "RSA "),
                "private-key",
            ),
            (fake("AKIA", "ABCD2345", 2), "aws-access-key"),
            (fake("ghp_", "aB3dE6gH9j", 4), "github-token"),
            (fake("glpat-", "aB3dE6gH9j", 2), "gitlab-token"),
            (fake("xoxb-", "12345-abcde", 2), "slack-token"),
            (
                fake(&format!("sk_{}_", ["li", "ve"].concat()), "aB3dE6gH", 3),
                "stripe-key",
            ),
            (
                fake("AIza", "aB3dE6gH9jK", 4)[..39].to_owned(),
                "google-api-key",
            ),
            (fake("npm_", "aB3dE6gH9jKl", 3), "npm-token"),
            (fake("pypi-AgE", "aB3dE6gH9j", 6), "pypi-token"),
            (fake("sk-", "aB3dE6gH9j", 4), "sk-api-key"),
            (
                format!(
                    "{}.{}.{}",
                    fake("eyJ", "hbGciOiJ", 2),
                    fake("eyJ", "zdWIiOiI", 2),
                    "sIgnAtUrE_x9"
                ),
                "jwt",
            ),
            (
                format!("password = \"{}\"", "Zq8!rT2#vL5@nW9x"),
                "assigned-secret",
            ),
        ];
        for (text, rule) in cases {
            assert_eq!(refused(&text).as_deref(), Some(rule), "{rule}");
        }
    }

    #[test]
    fn ordinary_writing_passes() {
        for text in [
            "Hosted checkout: no card data on our servers.",
            "the token budget is 4096 bytes",
            "password: see the team vault",
            "secret = \"aaaaaaaaaaaaaaaaaaaa\"",
            "api_key = \"read-it-from-the-environment\"",
            "src/auth/login.rs handles sessions",
            "sk-short",
            "commit 58e8aed8d4569934d42c2cbcfc5d6be41ccbcc6073bbf50dca27b368a53c449e",
        ] {
            assert_eq!(refused(text), None, "{text}");
        }
    }

    #[test]
    fn a_refusal_says_where_and_never_what() {
        let key = fake("ghp_", "aB3dE6gH9j", 4);
        let text = format!("line one\nuse {key} here");
        let Err(Refused::Secret(found)) = check("tasks[1].detail", &text, &Allow::default()) else {
            panic!("not refused");
        };
        assert_eq!((found.line, found.column), (2, 5));
        assert_eq!(found.field, "tasks[1].detail");
        let message = found.message();
        assert!(
            !message.contains(&key) && !message.contains("aB3dE6"),
            "{message}"
        );
        assert!(
            message.contains("allow_secret: \"github-token\""),
            "{message}"
        );
        assert!(!found.to_json().to_string().contains("aB3dE6"));
    }

    #[test]
    fn an_override_names_the_rule_and_only_that_rule() {
        let text = fake("ghp_", "aB3dE6gH9j", 4);
        let allow = Allow::parse(Some("github-token")).expect("parsed");
        assert!(check("value", &text, &allow).is_ok());
        let other = Allow::parse(Some("jwt, slack-token")).expect("parsed");
        assert!(check("value", &text, &other).is_err());
        assert_eq!(
            Allow::parse(Some("github-tokn")),
            Err(Refused::UnknownRule("github-tokn".to_owned()))
        );
    }
}
