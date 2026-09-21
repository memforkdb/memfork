//! JSON Schema builders, and the conservative subset every vendor accepts.
//!
//! DESIGN §6.1 pins tool input schemas to the intersection of what MCP clients
//! and function-calling APIs all handle: `type`, `properties`, `required`,
//! `description`, `enum` and `items`, and nothing else. No `$ref`, no
//! `oneOf`/`anyOf`/`allOf`, no `format` or `pattern` — some clients sanitise
//! those and some reject the tool outright.
//!
//! Schemas are therefore written by hand here rather than derived from Rust
//! types. A derive macro would emit `$schema`, `format: "uint64"` for integers
//! and `anyOf` for `Option<T>`, all of which are outside the subset. Writing
//! them out makes the constraint structural instead of something a test has to
//! catch after the fact — though [`validate_subset`] checks it anyway, because
//! a hand-written schema can still drift.

use serde_json::{json, Map, Value};

/// A JSON object, as `serde_json` models it.
pub type JsonObject = Map<String, Value>;

/// The only schema keywords a tool may use (DESIGN §6.1).
pub const ALLOWED_KEYWORDS: &[&str] = &[
    "type",
    "properties",
    "required",
    "description",
    "enum",
    "items",
];

/// The only `type` values a tool may use.
pub const ALLOWED_TYPES: &[&str] = &["object", "string", "number", "integer", "boolean", "array"];

/// Tool count ceiling (DESIGN §6.1). A long tool list crowds a model's context
/// and some clients truncate it.
pub const MAX_TOOLS: usize = 16;

/// Tool name ceiling (DESIGN §6.1).
pub const MAX_NAME_LEN: usize = 48;

/// An object schema with the given properties, in the order written, and the
/// given required keys.
pub fn object(properties: &[(&str, Value)], required: &[&str]) -> JsonObject {
    let mut props = Map::new();
    for (name, schema) in properties {
        props.insert((*name).to_owned(), schema.clone());
    }
    let mut out = Map::new();
    out.insert("type".to_owned(), json!("object"));
    out.insert("properties".to_owned(), Value::Object(props));
    out.insert(
        "required".to_owned(),
        Value::Array(required.iter().map(|r| json!(r)).collect()),
    );
    out
}

/// An object schema taking no arguments at all.
pub fn no_arguments() -> JsonObject {
    object(&[], &[])
}

/// A string property.
pub fn string(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

/// A string property restricted to a fixed set of values.
pub fn string_enum(description: &str, values: &[&str]) -> Value {
    json!({ "type": "string", "description": description, "enum": values })
}

/// A fractional number property.
pub fn number(description: &str) -> Value {
    json!({ "type": "number", "description": description })
}

/// A whole number property.
pub fn integer(description: &str) -> Value {
    json!({ "type": "integer", "description": description })
}

/// An array of fractional numbers, e.g. an embedding.
pub fn number_array(description: &str) -> Value {
    json!({
        "type": "array",
        "description": description,
        "items": { "type": "number" }
    })
}

/// An array of strings.
pub fn string_array(description: &str) -> Value {
    json!({
        "type": "array",
        "description": description,
        "items": { "type": "string" }
    })
}

/// A free-form object of string values.
///
/// The subset has no `additionalProperties`, so an object with caller-chosen
/// keys can only be described by its type and its description. That is enough:
/// the description says what belongs in it.
pub fn free_object(description: &str) -> Value {
    json!({ "type": "object", "description": description })
}

/// Check a schema against the DESIGN §6.1 subset, returning every violation.
///
/// Recurses through `properties` and `items`, so a forbidden keyword nested
/// three levels down is still found.
pub fn validate_subset(schema: &JsonObject) -> Vec<String> {
    let mut problems = Vec::new();
    check_object(schema, "", &mut problems);
    problems
}

fn check_object(schema: &JsonObject, path: &str, problems: &mut Vec<String>) {
    let at = |p: &str| {
        if p.is_empty() {
            "(root)".to_owned()
        } else {
            p.to_owned()
        }
    };

    for key in schema.keys() {
        if !ALLOWED_KEYWORDS.contains(&key.as_str()) {
            problems.push(format!(
                "{}: `{key}` is not one of the permitted keywords {ALLOWED_KEYWORDS:?}",
                at(path)
            ));
        }
    }

    match schema.get("type") {
        None => problems.push(format!("{}: no `type`", at(path))),
        Some(Value::String(t)) => {
            if !ALLOWED_TYPES.contains(&t.as_str()) {
                problems.push(format!(
                    "{}: type `{t}` is not one of {ALLOWED_TYPES:?}",
                    at(path)
                ));
            }
        }
        // A union type such as `["string", "null"]` is exactly what the subset
        // exists to keep out: several clients reject it.
        Some(other) => problems.push(format!(
            "{}: `type` must be a single string, found {other}",
            at(path)
        )),
    }

    if let Some(Value::Array(values)) = schema.get("enum") {
        if values.is_empty() {
            problems.push(format!("{}: `enum` is empty", at(path)));
        }
        for v in values {
            if !v.is_string() {
                problems.push(format!(
                    "{}: `enum` values must be strings, found {v}",
                    at(path)
                ));
            }
        }
    }

    if let Some(required) = schema.get("required") {
        match required {
            Value::Array(keys) => {
                let props = schema.get("properties").and_then(Value::as_object);
                for k in keys {
                    match (k.as_str(), props) {
                        (Some(name), Some(props)) if !props.contains_key(name) => problems.push(
                            format!("{}: `{name}` is required but is not a property", at(path)),
                        ),
                        (None, _) => problems
                            .push(format!("{}: `required` entries must be strings", at(path))),
                        _ => {}
                    }
                }
            }
            _ => problems.push(format!("{}: `required` must be an array", at(path))),
        }
    }

    if let Some(props) = schema.get("properties") {
        match props.as_object() {
            None => problems.push(format!("{}: `properties` must be an object", at(path))),
            Some(props) => {
                for (name, sub) in props {
                    let sub_path = if path.is_empty() {
                        name.clone()
                    } else {
                        format!("{path}.{name}")
                    };
                    match sub.as_object() {
                        Some(sub) => check_object(sub, &sub_path, problems),
                        None => {
                            problems.push(format!("{sub_path}: property schema must be an object"))
                        }
                    }
                }
            }
        }
    }

    if let Some(items) = schema.get("items") {
        let sub_path = format!("{}[]", at(path));
        match items.as_object() {
            Some(items) => check_object(items, &sub_path, problems),
            None => problems.push(format!(
                "{sub_path}: `items` must be a single schema object"
            )),
        }
    }
}

/// Whether a tool name meets DESIGN §6.1: `[a-z0-9_]` only, at most 48 characters.
pub fn validate_name(name: &str) -> Vec<String> {
    let mut problems = Vec::new();
    if name.is_empty() {
        problems.push("tool name is empty".to_owned());
    }
    if name.len() > MAX_NAME_LEN {
        problems.push(format!(
            "tool name `{name}` is {} characters, maximum is {MAX_NAME_LEN}",
            name.len()
        ));
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_'))
    {
        problems.push(format!(
            "tool name `{name}` contains `{bad}`; only [a-z0-9_] is allowed"
        ));
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_conservative_schema_passes() {
        let schema = object(
            &[
                ("key", string("a key")),
                ("policy", string_enum("how", &["fail", "ours"])),
                ("embedding", number_array("a vector")),
                ("meta", free_object("pairs")),
            ],
            &["key"],
        );
        assert!(validate_subset(&schema).is_empty());
    }

    #[test]
    fn forbidden_keywords_are_caught() {
        let mut schema = object(&[("k", string("k"))], &["k"]);
        schema.insert("$schema".to_owned(), json!("https://json-schema.org/"));
        schema.insert("additionalProperties".to_owned(), json!(false));
        let problems = validate_subset(&schema);
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("$schema")));
        assert!(problems.iter().any(|p| p.contains("additionalProperties")));
    }

    #[test]
    fn forbidden_keywords_nested_in_a_property_are_caught() {
        let schema = object(
            &[("k", json!({ "type": "string", "format": "uuid" }))],
            &["k"],
        );
        let problems = validate_subset(&schema);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].starts_with("k:"), "{problems:?}");
        assert!(problems[0].contains("format"));
    }

    #[test]
    fn forbidden_keywords_nested_in_items_are_caught() {
        let schema = object(
            &[(
                "xs",
                json!({
                    "type": "array",
                    "items": { "type": "string", "pattern": "^a" }
                }),
            )],
            &["xs"],
        );
        let problems = validate_subset(&schema);
        assert!(
            problems.iter().any(|p| p.contains("pattern")),
            "{problems:?}"
        );
    }

    #[test]
    fn a_nullable_union_type_is_caught() {
        let schema = object(&[("k", json!({ "type": ["string", "null"] }))], &[]);
        let problems = validate_subset(&schema);
        assert!(
            problems.iter().any(|p| p.contains("single string")),
            "{problems:?}"
        );
    }

    #[test]
    fn requiring_an_undeclared_property_is_caught() {
        let schema = object(&[("a", string("a"))], &["b"]);
        let problems = validate_subset(&schema);
        assert!(
            problems.iter().any(|p| p.contains("`b` is required")),
            "{problems:?}"
        );
    }

    #[test]
    fn names_are_checked() {
        assert!(validate_name("memfork_put").is_empty());
        assert!(!validate_name("memfork-put").is_empty());
        assert!(!validate_name("MemforkPut").is_empty());
        assert!(!validate_name(&"a".repeat(MAX_NAME_LEN + 1)).is_empty());
        assert!(!validate_name("").is_empty());
    }
}
