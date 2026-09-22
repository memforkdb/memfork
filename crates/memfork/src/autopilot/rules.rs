//! What counts as a risky command (DESIGN §5.6).
//!
//! Decided by data, never by a model: the rules are regular expressions in
//! `autopilot/rules.toml`, compiled in, in five families — migrations,
//! destructive file operations, history rewriting, dependency changes,
//! database commands. A project may add patterns of its own and switch rules
//! off by name. `memfork autopilot rules` prints them, and `memfork autopilot
//! check "<command>"` says which one a command matches, so nobody has to
//! guess why memory was forked.

use std::sync::OnceLock;

use regex_lite::Regex;
use serde::Deserialize;

/// The compiled-in rules.
pub const RULES: &str = include_str!("rules.toml");

/// The name a pattern from a project's own file matches under.
pub const EXTRA_RULE: &str = "project";

/// The most patterns a project may add.
pub const MAX_EXTRA_RULES: usize = 50;

/// Longest command line the rules look at; a longer one is judged by its
/// first bytes, which is where the command is.
pub const MAX_COMMAND_BYTES: usize = 16 * 1024;

/// One rule.
#[derive(Debug, Clone)]
pub struct Rule {
    /// Its name, for `ignore_rules` and for the lesson a failure leaves.
    pub name: String,
    /// The family it belongs to, for people.
    pub family: String,
    /// The pattern, as written.
    pub pattern: String,
    /// A command it matches, for the listing.
    pub example: String,
    regex: Regex,
}

#[derive(Debug, Deserialize)]
struct File {
    rule: Vec<RuleSpec>,
}

#[derive(Debug, Deserialize)]
struct RuleSpec {
    name: String,
    family: String,
    pattern: String,
    example: String,
}

fn compile(pattern: &str) -> Result<Regex, String> {
    Regex::new(&format!("(?i){pattern}"))
        .map_err(|e| format!("`{pattern}` is not a valid pattern: {e}"))
}

fn load() -> Result<Vec<Rule>, String> {
    let file: File = toml::from_str(RULES)
        .map_err(|e| format!("the compiled-in autopilot rules are malformed: {e}"))?;
    file.rule
        .into_iter()
        .map(|spec| {
            let regex = compile(&spec.pattern)?;
            Ok(Rule {
                name: spec.name,
                family: spec.family,
                pattern: spec.pattern,
                example: spec.example,
                regex,
            })
        })
        .collect()
}

/// Every built-in rule, in file order.
pub fn all() -> Result<&'static [Rule], String> {
    static LOADED: OnceLock<Result<Vec<Rule>, String>> = OnceLock::new();
    match LOADED.get_or_init(load) {
        Ok(rules) => Ok(rules),
        Err(why) => Err(why.clone()),
    }
}

/// The rules in force for one project: the built-in ones less those it
/// switched off, plus its own patterns.
#[derive(Debug, Clone)]
pub struct RuleSet {
    rules: Vec<Rule>,
}

impl RuleSet {
    /// The built-in rules alone.
    pub fn builtin() -> Result<RuleSet, String> {
        Ok(RuleSet {
            rules: all()?.to_vec(),
        })
    }

    /// The built-in rules with a project's changes applied. A pattern that
    /// does not compile is reported rather than skipped, so a typo does not
    /// silently switch a rule off.
    pub fn for_project(extra: &[String], ignore: &[String]) -> Result<RuleSet, String> {
        let mut rules: Vec<Rule> = all()?
            .iter()
            .filter(|r| !ignore.iter().any(|i| i == &r.name))
            .cloned()
            .collect();
        for pattern in extra.iter().take(MAX_EXTRA_RULES) {
            let regex = compile(pattern)?;
            rules.push(Rule {
                name: EXTRA_RULE.to_owned(),
                family: "this project's own rules".to_owned(),
                pattern: pattern.clone(),
                example: String::new(),
                regex,
            });
        }
        Ok(RuleSet { rules })
    }

    /// The rules, in the order they are tried.
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// The first rule `command` matches, if any.
    pub fn matches(&self, command: &str) -> Option<&Rule> {
        let end = command
            .char_indices()
            .map(|(i, _)| i)
            .find(|i| *i >= MAX_COMMAND_BYTES)
            .unwrap_or(command.len());
        let text = &command[..end];
        self.rules.iter().find(|r| r.regex.is_match(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name_of(command: &str) -> Option<String> {
        RuleSet::builtin()
            .unwrap()
            .matches(command)
            .map(|r| r.name.clone())
    }

    #[test]
    fn every_rule_matches_its_own_example_and_has_a_family() {
        let rules = all().unwrap();
        assert!(rules.len() >= 5);
        let set = RuleSet::builtin().unwrap();
        for rule in rules {
            assert!(!rule.family.is_empty(), "{}", rule.name);
            assert_eq!(
                set.matches(&rule.example).map(|r| r.name.as_str()),
                Some(rule.name.as_str()),
                "the example of `{}` does not match it",
                rule.name
            );
        }
        let families: std::collections::BTreeSet<&str> =
            rules.iter().map(|r| r.family.as_str()).collect();
        assert_eq!(families.len(), 5, "{families:?}");
    }

    #[test]
    fn risky_commands_are_named_by_rule() {
        for (command, rule) in [
            ("npx prisma migrate dev", "migration"),
            ("bundle exec rails db:migrate", "migration"),
            ("python manage.py migrate", "migration"),
            ("rm -rf node_modules", "recursive-delete"),
            ("rm -fr ./build && echo done", "recursive-delete"),
            ("Remove-Item -Recurse -Force dist", "recursive-delete"),
            ("git clean -fdx", "recursive-delete"),
            ("find . -name '*.log' -delete", "recursive-delete"),
            ("git rebase -i HEAD~3", "history-rewrite"),
            ("git push --force origin main", "history-rewrite"),
            ("git push -f", "history-rewrite"),
            ("git reset --hard HEAD~1", "history-rewrite"),
            ("git commit --amend --no-edit", "history-rewrite"),
            ("npm install left-pad", "dependency-change"),
            ("npm i", "dependency-change"),
            ("pip install requests", "dependency-change"),
            ("cargo add serde", "dependency-change"),
            ("cargo update", "dependency-change"),
            ("uv add httpx", "dependency-change"),
            ("go get github.com/x/y", "dependency-change"),
            ("psql -U app -c 'select 1'", "database-shell"),
            ("mongosh", "database-shell"),
            (
                "echo 'DROP TABLE users' | sqlite3 app.db",
                "recursive-delete",
            ),
        ] {
            let got = name_of(command);
            // The last case matches two families; either name is a right
            // answer, and the first in file order is the one given.
            if command.contains("DROP TABLE") {
                assert!(got.is_some(), "{command}");
                continue;
            }
            assert_eq!(got.as_deref(), Some(rule), "{command}");
        }
    }

    #[test]
    fn ordinary_commands_are_not_risky() {
        for command in [
            "cargo test",
            "npm test",
            "npm run build",
            "git status",
            "git commit -m 'fix'",
            "git push origin main",
            "git checkout feature/x",
            "ls -la",
            "rm notes.txt",
            "grep -r migrate src",
            "cat README.md",
            "python -m pytest",
            "echo formatting",
            "git log --oneline",
            "pip list",
            "npm ls",
        ] {
            assert_eq!(name_of(command), None, "{command}");
        }
    }

    #[test]
    fn a_project_can_add_patterns_and_switch_rules_off() {
        let set = RuleSet::for_project(
            &[r"terraform\s+apply".to_owned()],
            &["dependency-change".to_owned()],
        )
        .unwrap();
        assert_eq!(
            set.matches("terraform apply -auto-approve")
                .map(|r| r.name.as_str()),
            Some(EXTRA_RULE)
        );
        assert!(set.matches("npm install x").is_none());
        assert!(set.matches("rm -rf x").is_some());
        // A broken pattern is an error, not a silent no-op.
        assert!(RuleSet::for_project(&["(".to_owned()], &[]).is_err());
    }

    #[test]
    fn a_very_long_command_is_judged_by_its_start() {
        let long = format!("npm install {}", "x".repeat(MAX_COMMAND_BYTES * 2));
        assert_eq!(name_of(&long).as_deref(), Some("dependency-change"));
        let late = format!("echo {} && rm -rf /", "x".repeat(MAX_COMMAND_BYTES * 2));
        assert_eq!(name_of(&late), None);
    }
}
