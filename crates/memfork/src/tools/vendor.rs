//! The tool definitions in each vendor's function-calling format (DESIGN §6.1).
//!
//! Access tier 3: a model with no MCP support at all. `memfork tools --format
//! <vendor>` prints the definitions that vendor's API expects, and `memfork
//! call` executes one, so MemFork is usable from any function-calling model —
//! including a local one.
//!
//! The argument schemas are the same objects the MCP server serves, unchanged.
//! Since they are already in the conservative subset (DESIGN §6.1), no vendor
//! needs anything stripped out; the only differences are where the name and
//! schema sit in the envelope.

use serde_json::{json, Map, Value as Json};

use super::ToolDef;

/// Which vendor's function-calling format to print.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Flat `{"type": "function", "name", "description", "parameters"}`, as
    /// used by the Responses API. For the older Chat Completions shape, wrap
    /// each entry as `{"type": "function", "function": {…}}`.
    OpenAi,
    /// `{"name", "description", "input_schema"}`.
    Anthropic,
    /// `{"functionDeclarations": [{"name", "description", "parameters"}]}`.
    Gemini,
}

impl Format {
    /// Parse a `--format` value.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "openai" => Some(Format::OpenAi),
            "anthropic" => Some(Format::Anthropic),
            "gemini" => Some(Format::Gemini),
            _ => None,
        }
    }

    /// The name this format parses from.
    pub fn as_str(self) -> &'static str {
        match self {
            Format::OpenAi => "openai",
            Format::Anthropic => "anthropic",
            Format::Gemini => "gemini",
        }
    }

    /// Every accepted value, for help text and for tests.
    pub fn all() -> [Format; 3] {
        [Format::OpenAi, Format::Anthropic, Format::Gemini]
    }
}

/// Render every tool in the given format, as the value that goes into a request.
pub fn render(tools: &[ToolDef], format: Format) -> Json {
    match format {
        Format::OpenAi => Json::Array(
            tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "name": t.name,
                        "description": t.description,
                        "parameters": Json::Object(t.schema.clone()),
                    })
                })
                .collect(),
        ),

        Format::Anthropic => Json::Array(
            tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": Json::Object(t.schema.clone()),
                    })
                })
                .collect(),
        ),

        // Gemini takes one object holding every declaration, not a list of
        // objects, and calls the schema `parameters`.
        Format::Gemini => json!({
            "functionDeclarations": tools
                .iter()
                .map(|t| json!({
                    "name": t.name,
                    "description": t.description,
                    "parameters": Json::Object(t.schema.clone()),
                }))
                .collect::<Vec<_>>(),
        }),
    }
}

/// The per-tool entries of a rendering, for validation and for tests.
///
/// Flattens the vendor envelope back to a list, so a caller can check each
/// tool without caring which shape it came wrapped in.
pub fn entries(rendered: &Json, format: Format) -> Vec<Map<String, Json>> {
    let list = match format {
        Format::OpenAi | Format::Anthropic => rendered.as_array().cloned().unwrap_or_default(),
        Format::Gemini => rendered
            .get("functionDeclarations")
            .and_then(Json::as_array)
            .cloned()
            .unwrap_or_default(),
    };
    list.into_iter()
        .filter_map(|v| v.as_object().cloned())
        .collect()
}

/// The key each format puts the argument schema under.
pub fn schema_key(format: Format) -> &'static str {
    match format {
        Format::OpenAi | Format::Gemini => "parameters",
        Format::Anthropic => "input_schema",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools;

    #[test]
    fn formats_round_trip_through_their_names() {
        for f in Format::all() {
            assert_eq!(Format::parse(f.as_str()), Some(f));
        }
        assert_eq!(Format::parse("OpenAI"), Some(Format::OpenAi));
        assert_eq!(Format::parse("nope"), None);
    }

    #[test]
    fn every_format_carries_every_tool_with_its_schema() {
        let all = tools::all();
        for f in Format::all() {
            let rendered = render(&all, f);
            let entries = entries(&rendered, f);
            assert_eq!(entries.len(), all.len(), "{f:?} dropped tools");
            for (entry, tool) in entries.iter().zip(&all) {
                assert_eq!(entry["name"], tool.name, "{f:?}");
                assert_eq!(entry["description"], tool.description, "{f:?}");
                assert_eq!(
                    entry[schema_key(f)],
                    Json::Object(tool.schema.clone()),
                    "{f:?} altered the schema of {}",
                    tool.name
                );
            }
        }
    }

    #[test]
    fn the_envelope_is_the_shape_each_vendor_documents() {
        let all = tools::all();

        // OpenAI Responses API: a flat array, each entry tagged `function`.
        let openai = render(&all, Format::OpenAi);
        assert!(openai.is_array());
        assert_eq!(openai[0]["type"], "function");
        assert!(openai[0].get("name").is_some());
        assert!(
            openai[0].get("function").is_none(),
            "the Responses API takes the flat form, not the nested one"
        );

        // Anthropic: a flat array, schema under `input_schema`, no `type` tag.
        let anthropic = render(&all, Format::Anthropic);
        assert!(anthropic.is_array());
        assert!(anthropic[0].get("type").is_none());
        assert!(anthropic[0].get("input_schema").is_some());

        // Gemini: one object wrapping the declarations.
        let gemini = render(&all, Format::Gemini);
        assert!(gemini.is_object());
        assert!(gemini["functionDeclarations"].is_array());
    }
}
