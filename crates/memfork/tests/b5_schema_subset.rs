//! B5 — every tool schema stays inside the subset every vendor accepts.
//!
//! DESIGN §6.1 pins tool input schemas to `type`, `properties`, `required`,
//! `description`, `enum` and `items`, caps the tool count at 16 and tool names
//! at 48 characters of `[a-z0-9_]`. Some clients silently sanitise anything
//! else; some reject the tool outright. Either way the failure appears at the
//! far end, in someone else's product, so it is checked here.
//!
//! The check runs against what the binary actually emits — the same JSON an
//! MCP client and a function-calling API would receive — rather than against
//! the Rust values behind it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;

use serde_json::Value as Json;

mod support;

/// The only schema keywords DESIGN §6.1 permits.
const ALLOWED_KEYWORDS: &[&str] = &[
    "type",
    "properties",
    "required",
    "description",
    "enum",
    "items",
];

/// The only `type` values DESIGN §6.1 permits.
const ALLOWED_TYPES: &[&str] = &["object", "string", "number", "integer", "boolean", "array"];

/// Keywords that are specifically known to break a client somewhere.
const FORBIDDEN_KEYWORDS: &[&str] = &[
    "$ref",
    "$schema",
    "$defs",
    "definitions",
    "oneOf",
    "anyOf",
    "allOf",
    "not",
    "format",
    "pattern",
    "additionalProperties",
    "patternProperties",
    "const",
    "default",
    "minimum",
    "maximum",
    "nullable",
];

const MAX_TOOLS: usize = 16;
const MAX_NAME_LEN: usize = 48;

use support::memfork_assert as memfork;

/// The tool definitions as the binary prints them for a vendor.
fn rendered(format: &str) -> Json {
    let out = memfork()
        .args(["tools", "--format", format])
        .assert()
        .success();
    serde_json::from_slice(&out.get_output().stdout).expect("valid JSON")
}

/// Flatten a vendor rendering to (name, schema) pairs.
fn tools_of(format: &str) -> Vec<(String, Json)> {
    let doc = rendered(format);
    let (list, schema_key) = match format {
        "openai" => (doc.as_array().cloned().unwrap_or_default(), "parameters"),
        "anthropic" => (doc.as_array().cloned().unwrap_or_default(), "input_schema"),
        "gemini" => (
            doc["functionDeclarations"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
            "parameters",
        ),
        other => panic!("unknown format {other}"),
    };
    list.into_iter()
        .map(|t| {
            (
                t["name"].as_str().expect("a name").to_owned(),
                t[schema_key].clone(),
            )
        })
        .collect()
}

/// Walk a schema, collecting every violation with the path it was found at.
fn violations(schema: &Json, path: &str, problems: &mut Vec<String>) {
    let at = if path.is_empty() { "(root)" } else { path };
    let Some(obj) = schema.as_object() else {
        problems.push(format!("{at}: schema is not an object"));
        return;
    };

    for key in obj.keys() {
        if !ALLOWED_KEYWORDS.contains(&key.as_str()) {
            problems.push(format!("{at}: forbidden keyword `{key}`"));
        }
    }
    // Named explicitly as well, so a failure says which known-bad construct it
    // is rather than only that something unexpected appeared.
    for bad in FORBIDDEN_KEYWORDS {
        if obj.contains_key(*bad) {
            problems.push(format!("{at}: `{bad}` is not accepted by every vendor"));
        }
    }

    match obj.get("type") {
        None => problems.push(format!("{at}: no `type`")),
        Some(Json::String(t)) if ALLOWED_TYPES.contains(&t.as_str()) => {}
        Some(Json::String(t)) => problems.push(format!("{at}: type `{t}` is not permitted")),
        Some(other) => problems.push(format!(
            "{at}: `type` must be a single string, found {other} \
             (a union type is rejected by several clients)"
        )),
    }

    if let Some(props) = obj.get("properties") {
        match props.as_object() {
            None => problems.push(format!("{at}: `properties` is not an object")),
            Some(props) => {
                for (name, sub) in props {
                    let sub_path = if path.is_empty() {
                        name.clone()
                    } else {
                        format!("{path}.{name}")
                    };
                    violations(sub, &sub_path, problems);
                }
            }
        }
    }

    if let Some(items) = obj.get("items") {
        violations(items, &format!("{at}[]"), problems);
    }

    if let Some(values) = obj.get("enum") {
        match values.as_array() {
            None => problems.push(format!("{at}: `enum` is not an array")),
            Some(values) if values.is_empty() => problems.push(format!("{at}: `enum` is empty")),
            Some(values) => {
                for v in values {
                    if !v.is_string() {
                        problems.push(format!("{at}: `enum` value {v} is not a string"));
                    }
                }
            }
        }
    }

    if let Some(required) = obj.get("required") {
        let props = obj.get("properties").and_then(Json::as_object);
        match required.as_array() {
            None => problems.push(format!("{at}: `required` is not an array")),
            Some(keys) => {
                for k in keys {
                    match k.as_str() {
                        None => {
                            problems.push(format!("{at}: `required` entry {k} is not a string"))
                        }
                        Some(name) => {
                            if !props.is_some_and(|p| p.contains_key(name)) {
                                problems
                                    .push(format!("{at}: `{name}` is required but not declared"));
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn b5_every_schema_stays_inside_the_subset() {
    let mut problems = Vec::new();
    for format in ["openai", "anthropic", "gemini"] {
        for (name, schema) in tools_of(format) {
            let mut tool_problems = Vec::new();
            violations(&schema, "", &mut tool_problems);
            problems.extend(
                tool_problems
                    .into_iter()
                    .map(|p| format!("{format}/{name}: {p}")),
            );
        }
    }
    assert!(
        problems.is_empty(),
        "tool schemas left the DESIGN §6.1 subset:\n  {}",
        problems.join("\n  ")
    );
}

#[test]
fn b5_tool_names_and_count_are_within_the_limits() {
    let tools = tools_of("anthropic");
    assert!(
        tools.len() <= MAX_TOOLS,
        "{} tools, the ceiling is {MAX_TOOLS}",
        tools.len()
    );
    for (name, _) in &tools {
        assert!(
            name.len() <= MAX_NAME_LEN,
            "`{name}` is {} characters, the ceiling is {MAX_NAME_LEN}",
            name.len()
        );
        assert!(
            name.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "`{name}` is not [a-z0-9_]"
        );
    }
    let unique: BTreeSet<&String> = tools.iter().map(|(n, _)| n).collect();
    assert_eq!(unique.len(), tools.len(), "duplicate tool names");
}

#[test]
fn b5_every_vendor_sees_the_same_tools_and_the_same_schemas() {
    // One registry, three envelopes: a difference here means a vendor path is
    // rewriting schemas, which is how a subset violation would sneak in.
    let openai = tools_of("openai");
    let anthropic = tools_of("anthropic");
    let gemini = tools_of("gemini");

    assert_eq!(openai, anthropic);
    assert_eq!(openai, gemini);
}

#[test]
fn b5_the_mcp_server_serves_exactly_these_schemas() {
    // The schemas an MCP client receives must be the same ones the vendor
    // formats carry, or the subset check would only cover half the surface.
    // `memfork call` shares the registry, so a mismatch shows up as a tool
    // that exists in one place and not the other.
    let names: BTreeSet<String> = tools_of("anthropic").into_iter().map(|(n, _)| n).collect();
    for name in &names {
        let out = memfork().args(["call", name, "{}"]).assert();
        let output = out.get_output();
        let text = String::from_utf8_lossy(&output.stderr);
        assert!(
            !text.contains("no tool named"),
            "`{name}` is in the vendor output but not in the dispatcher"
        );
    }
}

#[test]
fn b5_the_validator_actually_catches_a_violation() {
    // A check that can never fail is not a check. Each of these is a real
    // construct a schema generator would emit.
    for (bad, why) in [
        (
            serde_json::json!({ "type": "object", "$schema": "https://json-schema.org/" }),
            "$schema",
        ),
        (
            serde_json::json!({ "type": "object", "additionalProperties": false }),
            "additionalProperties",
        ),
        (
            serde_json::json!({ "type": ["string", "null"] }),
            "single string",
        ),
        (
            serde_json::json!({
                "type": "object",
                "properties": { "id": { "type": "string", "format": "uuid" } }
            }),
            "format",
        ),
        (
            serde_json::json!({
                "type": "object",
                "properties": {
                    "xs": { "type": "array", "items": { "type": "string", "pattern": "^a" } }
                }
            }),
            "pattern",
        ),
        (
            serde_json::json!({ "type": "object", "properties": {}, "required": ["ghost"] }),
            "ghost",
        ),
    ] {
        let mut problems = Vec::new();
        violations(&bad, "", &mut problems);
        assert!(
            problems.iter().any(|p| p.contains(why)),
            "the validator missed `{why}` in {bad}: {problems:?}"
        );
    }
}
