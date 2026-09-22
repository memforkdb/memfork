//! MCP prompts: MemFork's routines as one-step commands (DESIGN §6.14).
//!
//! Clients that show MCP prompts to the person offer these as commands —
//! resume here, hand off, review decisions, tidy memory, take the next task —
//! so an agent uses MemFork well without MemFork thinking for it. The text is
//! data, in `prompts.toml`, short and about no particular model or client.
//! The proxy and the daemon serve the same list.

use std::sync::OnceLock;

use rmcp::model::{GetPromptResult, JsonObject, Prompt, PromptArgument, PromptMessage, Role};
use serde::Deserialize;

const PROMPTS: &str = include_str!("prompts.toml");

#[derive(Debug, Deserialize)]
struct File {
    prompt: Vec<Spec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Spec {
    name: String,
    description: String,
    text: String,
    argument: Option<String>,
    argument_description: Option<String>,
    label: Option<String>,
}

fn specs() -> &'static [Spec] {
    static LOADED: OnceLock<Vec<Spec>> = OnceLock::new();
    LOADED.get_or_init(|| {
        // A test proves the shipped file parses; an empty list is the safe
        // answer if it somehow did not.
        toml::from_str::<File>(PROMPTS)
            .map(|f| f.prompt)
            .unwrap_or_default()
    })
}

/// Every prompt, in the order the file gives them.
pub fn list() -> Vec<Prompt> {
    specs()
        .iter()
        .map(|s| {
            let arguments = s.argument.as_ref().map(|name| {
                vec![PromptArgument::new(name.clone())
                    .with_description(s.argument_description.clone().unwrap_or_default())
                    .with_required(false)]
            });
            Prompt::new(s.name.clone(), Some(s.description.clone()), arguments)
        })
        .collect()
}

/// The prompt `name`, for a session working in `project`, with its argument
/// if one was given. `Err` names the prompts there are.
pub fn get(
    name: &str,
    arguments: Option<&JsonObject>,
    project: &str,
) -> Result<GetPromptResult, String> {
    let Some(spec) = specs().iter().find(|s| s.name == name) else {
        return Err(format!(
            "there is no prompt `{name}`; the prompts are: {}",
            specs()
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    };
    let mut text = spec.text.replace("{project}", project);
    let given = spec.argument.as_ref().and_then(|arg| {
        arguments
            .and_then(|a| a.get(arg))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
    });
    if let (Some(value), Some(label)) = (given, &spec.label) {
        text.push_str(&format!("\n\n{label}: {value}"));
    }
    Ok(
        GetPromptResult::new(vec![PromptMessage::new_text(Role::User, text)])
            .with_description(spec.description.clone()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_five_routines_are_there_short_and_neutral() {
        let names: Vec<String> = list().into_iter().map(|p| p.name).collect();
        assert_eq!(
            names,
            [
                "resume",
                "handoff",
                "review-decisions",
                "tidy-memory",
                "next-task"
            ]
        );
        for spec in specs() {
            assert!(spec.text.chars().count() <= 500, "{} is long", spec.name);
            let lower = spec.text.to_lowercase();
            for vendor in [
                "claude", "codex", "gemini", "cursor", "grok", "copilot", "gpt",
            ] {
                assert!(!lower.contains(vendor), "{} names {vendor}", spec.name);
            }
            // Every tool a prompt names exists.
            for word in spec
                .text
                .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            {
                if word.starts_with("memfork_") {
                    assert!(
                        crate::tools::find(word).is_some(),
                        "{} names {word}",
                        spec.name
                    );
                }
            }
        }
    }

    #[test]
    fn a_prompt_names_the_project_and_takes_its_argument() {
        let mut args = JsonObject::new();
        args.insert("note".to_owned(), serde_json::json!("the sandbox is down"));
        let got = get("handoff", Some(&args), "shop").expect("prompt");
        let text = serde_json::to_string(&got.messages).expect("json");
        assert!(text.contains("shop:decision:<topic>"), "{text}");
        assert!(
            text.contains("Make sure the handoff mentions: the sandbox is down"),
            "{text}"
        );
        assert!(get("nope", None, "shop")
            .expect_err("unknown")
            .contains("resume, handoff"));
    }
}
