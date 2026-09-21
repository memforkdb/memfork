//! B4 — `memfork tools --format openai|anthropic|gemini` validates against
//! each vendor's function-calling shape, and `memfork call` round-trips every
//! tool.
//!
//! This is DESIGN §6.1's third access tier: a model with no MCP support at all.
//! The tool definitions have to be usable as-is in a request to each vendor's
//! API, and the calls they describe have to actually execute.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde_json::{json, Value as Json};

mod support;

use support::memfork_assert as memfork;

fn tools(format: &str) -> Json {
    let out = memfork()
        .args(["tools", "--format", format])
        .assert()
        .success();
    serde_json::from_slice(&out.get_output().stdout).expect("valid JSON")
}

/// Run one tool call and return the result.
fn call(tool: &str, args: Json) -> Json {
    let out = memfork()
        .args(["--ephemeral", "call", tool, &args.to_string()])
        .assert()
        .success();
    serde_json::from_slice(&out.get_output().stdout).expect("valid JSON")
}

#[test]
fn b4_the_openai_shape_is_what_the_responses_api_takes() {
    let doc = tools("openai");
    let list = doc.as_array().expect("a top-level array of tools");
    assert!(!list.is_empty());

    for tool in list {
        let obj = tool.as_object().expect("each entry is an object");
        // The Responses API takes the flat form: type, name, description and
        // parameters as siblings.
        assert_eq!(obj["type"], "function");
        assert!(obj["name"].is_string(), "{tool}");
        assert!(obj["description"].is_string(), "{tool}");
        assert_eq!(obj["parameters"]["type"], "object", "{tool}");
        assert!(
            obj.get("function").is_none(),
            "the nested Chat Completions form was emitted instead of the flat one"
        );
        // Only the four keys the API expects; a stray key is rejected by
        // strict request validation.
        let keys: Vec<&String> = obj.keys().collect();
        assert_eq!(
            keys,
            vec!["type", "name", "description", "parameters"],
            "{tool}"
        );
    }
}

#[test]
fn b4_the_anthropic_shape_is_what_the_messages_api_takes() {
    let doc = tools("anthropic");
    let list = doc.as_array().expect("a top-level array of tools");
    assert!(!list.is_empty());

    for tool in list {
        let obj = tool.as_object().expect("each entry is an object");
        assert!(obj["name"].is_string(), "{tool}");
        assert!(obj["description"].is_string(), "{tool}");
        assert_eq!(obj["input_schema"]["type"], "object", "{tool}");
        assert!(
            obj.get("type").is_none(),
            "a `type` discriminator does not belong on a tool definition here"
        );
        assert!(
            obj.get("parameters").is_none(),
            "the schema key is `input_schema`, not `parameters`"
        );
        let keys: Vec<&String> = obj.keys().collect();
        assert_eq!(keys, vec!["name", "description", "input_schema"], "{tool}");
    }
}

#[test]
fn b4_the_gemini_shape_is_a_function_declarations_object() {
    let doc = tools("gemini");
    let obj = doc.as_object().expect("a top-level object");
    let keys: Vec<&String> = obj.keys().collect();
    assert_eq!(keys, vec!["functionDeclarations"]);

    let list = doc["functionDeclarations"]
        .as_array()
        .expect("an array of declarations");
    assert!(!list.is_empty());
    for tool in list {
        let obj = tool.as_object().expect("each entry is an object");
        assert!(obj["name"].is_string(), "{tool}");
        assert!(obj["description"].is_string(), "{tool}");
        assert_eq!(obj["parameters"]["type"], "object", "{tool}");
        let keys: Vec<&String> = obj.keys().collect();
        assert_eq!(keys, vec!["name", "description", "parameters"], "{tool}");
    }
}

#[test]
fn b4_an_unknown_format_is_refused() {
    memfork()
        .args(["tools", "--format", "nonesuch"])
        .assert()
        .failure();
}

#[test]
fn b4_every_tool_round_trips_through_call() {
    // Each tool is called with arguments its own schema describes, and the
    // result is checked, so this covers the dispatcher as well as the shape.
    // `memfork call` is single-shot, so every call starts from an
    // empty database and each case has to stand alone.

    let put = call(
        "memfork_put",
        json!({ "key": "note:1", "value": "remembered", "importance": 0.9 }),
    );
    assert_eq!(put["key"], "note:1");
    assert!(put["commit"].as_str().is_some_and(|c| c.len() == 64));

    // A fresh database each time, so a read of a key nothing wrote finds nothing.
    let get = call("memfork_get", json!({ "key": "note:1" }));
    assert_eq!(get["found"], false);

    let list = call("memfork_list", json!({ "prefix": "note:" }));
    assert_eq!(list["count"], 0);

    let search = call("memfork_search", json!({ "embedding": [1.0, 0.0], "k": 3 }));
    assert_eq!(search["count"], 0);

    let delete = call("memfork_delete", json!({ "key": "gone" }));
    assert_eq!(delete["deleted"], false);

    let fork = call("memfork_fork", json!({ "name": "attempt" }));
    assert_eq!(fork["name"], "attempt");
    assert_eq!(fork["current_branch"], "attempt");

    let checkout = call("memfork_checkout", json!({ "name": "main" }));
    assert_eq!(checkout["current_branch"], "main");

    let merge = call(
        "memfork_merge",
        json!({ "source": "main", "target": "main" }),
    );
    assert_eq!(merge["result"], "up_to_date");

    // `memfork_discard` is the one tool with nothing to succeed against in
    // single-shot mode: a fresh database has only the default branch, and that
    // one cannot be discarded. Its refusal is checked below, and B1 round-trips
    // a real discard over MCP, where state survives between calls.

    let branches = call("memfork_branches", json!({}));
    assert_eq!(branches["current_branch"], "main");

    let log = call("memfork_log", json!({ "limit": 5 }));
    assert_eq!(log["entries"].as_array().map(Vec::len), Some(1));

    let at = call("memfork_at", json!({ "seq": 0 }));
    assert_eq!(at["count"], 0);

    let diff = call("memfork_diff", json!({ "a": "main", "b": "main" }));
    assert_eq!(diff["count"], 0);
}

#[test]
fn b4_discarding_the_default_branch_fails_loudly() {
    // The one tool whose only interesting single-shot behaviour is a refusal.
    memfork()
        .args([
            "--ephemeral",
            "call",
            "memfork_discard",
            r#"{"name":"main"}"#,
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("cannot be discarded"));
}

#[test]
fn b4_call_reports_bad_arguments_rather_than_guessing() {
    for (tool, args, expected) in [
        ("memfork_put", r#"{}"#, "`key` is required"),
        ("memfork_put", r#"{"key":"k"}"#, "`value` is required"),
        (
            "memfork_put",
            r#"{"key":1,"value":"v"}"#,
            "`key` must be a string",
        ),
        (
            "memfork_search",
            r#"{"embedding":["a"]}"#,
            "must contain only numbers",
        ),
        ("memfork_at", r#"{"seq":-1}"#, "whole number"),
        (
            "memfork_merge",
            r#"{"source":"x","policy":"maybe"}"#,
            "policy",
        ),
        ("memfork_nope", r#"{}"#, "no tool named"),
    ] {
        memfork()
            .args(["--ephemeral", "call", tool, args])
            .assert()
            .failure()
            .stderr(predicates::str::contains(expected));
    }

    memfork()
        .args(["--ephemeral", "call", "memfork_get", "not json"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("not valid JSON"));

    memfork()
        .args(["--ephemeral", "call", "memfork_get", "[1,2]"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("must be a JSON object"));
}

#[test]
fn b4_call_with_no_arguments_defaults_to_an_empty_object() {
    let out = memfork()
        .args(["--ephemeral", "call", "memfork_branches"])
        .assert()
        .success();
    let doc: Json = serde_json::from_slice(&out.get_output().stdout).expect("valid JSON");
    assert_eq!(doc["current_branch"], "main");
}

#[test]
fn b4_the_definitions_are_stable_across_runs() {
    // A model's tool list is part of its prompt; it must not change shape or
    // order between invocations.
    for format in ["openai", "anthropic", "gemini"] {
        assert_eq!(tools(format), tools(format), "{format} is not stable");
    }
}

#[test]
fn b4_every_declared_tool_is_callable() {
    // The definitions and the dispatcher come from one registry; this proves
    // it by calling every name the definitions advertise.
    let doc = tools("anthropic");
    for tool in doc.as_array().expect("an array") {
        let name = tool["name"].as_str().expect("a name");
        let output = memfork().args(["--ephemeral", "call", name, "{}"]).assert();
        let stderr = String::from_utf8_lossy(&output.get_output().stderr).into_owned();
        assert!(
            !stderr.contains("no tool named"),
            "`{name}` is advertised but not implemented"
        );
    }
}
