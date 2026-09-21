//! The MCP tool surface (DESIGN §6), defined once and used everywhere.
//!
//! The same definitions drive the MCP server, `memfork tools` in each vendor's
//! function-calling format, and `memfork call`. There is no second copy to
//! drift.
//!
//! Descriptions say *when* to reach for a tool, not just what it does, because
//! that is what a model acts on. Nothing here names a model, a vendor or a
//! client: which clients are supported is data in the registry, never code.

pub mod dispatch;
pub mod handoff;
pub mod schema;
pub mod vendor;

use schema::{
    free_object, integer, no_arguments, number, number_array, object, string, string_array,
    string_enum, JsonObject,
};

/// One tool: its name, what it is for, and the shape of its arguments.
#[derive(Debug, Clone)]
pub struct ToolDef {
    /// Wire name, `[a-z0-9_]`, at most 48 characters.
    pub name: &'static str,
    /// Short human-readable label.
    pub title: &'static str,
    /// What the tool does and when to use it.
    pub description: String,
    /// JSON Schema for the arguments, in the DESIGN §6.1 subset.
    pub schema: JsonObject,
}

/// The sentence appended to every tool that takes a branch.
///
/// It tells the caller not to bother: the session remembers where it is, and
/// every result repeats it, so naming a branch is only for acting on one you
/// are not currently on.
const BRANCH_NOTE: &str = "Omit `branch` unless you mean a branch other than \
     the one you are on; every result says which that is, in `current_branch`.";

fn namespace_arg() -> (&'static str, serde_json::Value) {
    (
        "namespace",
        string(
            "Project namespace. Defaults to this session's, which the server \
             instructions name; give another only to work on a different project.",
        ),
    )
}

fn branch_arg() -> (&'static str, serde_json::Value) {
    (
        "branch",
        string(
            "Branch to act on. Defaults to the branch you are on, which every \
             result reports as `current_branch`.",
        ),
    )
}

/// Every tool, in a stable order.
///
/// Built fresh rather than kept in a static, because the schemas are owned
/// `serde_json` values; callers hold the result for as long as they need it.
pub fn all() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "memfork_put",
            title: "Store a memory",
            description: "Write a value under a key, replacing any previous value. \
                 Use this to record anything worth recalling later: a decision and \
                 its reasoning, a fact discovered, a plan, an intermediate result. \
                 Keys are colon-separated, project first: `<project>:<kind>:<id>`, \
                 for example `shop:decision:payment-provider` or `shop:task:12`, \
                 which makes them listable by prefix; the server instructions name \
                 this session's project. Store each decision with its reason under \
                 `<project>:decision:<topic>` as soon as it is made, and open work \
                 under `<project>:task:<id>`, so the next agent can pick it up. \
                 Supply `embedding` if you want the entry to be findable by \
                 memfork_search. Raise `importance` for entries that should survive \
                 longest when memory is tight. The result repeats the value it \
                 stored and says whether it replaced anything, so there is no need \
                 to read the key back to check."
                .to_owned(),
            schema: object(
                &[
                    (
                        "key",
                        string(
                            "Key to write, at most 1024 bytes. Convention: \
                             `<project>:<kind>:<id>`.",
                        ),
                    ),
                    (
                        "value",
                        string("Value to store. Opaque text; JSON by convention."),
                    ),
                    (
                        "importance",
                        number("How much this is worth keeping, 0 to 1. Defaults to 0.5."),
                    ),
                    (
                        "embedding",
                        number_array(
                            "Optional vector for similarity search. Every embedding in one \
                             database must have the same length.",
                        ),
                    ),
                    (
                        "ttl_commits",
                        integer("Expire this entry after this many commits on its branch."),
                    ),
                    (
                        "meta",
                        free_object(
                            "Optional string-to-string metadata, such as a source or a tag.",
                        ),
                    ),
                    branch_arg(),
                ],
                &["key", "value"],
            ),
        },
        ToolDef {
            name: "memfork_get",
            title: "Recall one memory",
            description: format!(
                "Read the value stored under one key. Use this before acting on \
                 something you may already have decided or discovered, rather than \
                 working it out again. Returns `found: false` if the key is not \
                 there. {BRANCH_NOTE}"
            ),
            schema: object(&[("key", string("Key to read.")), branch_arg()], &["key"]),
        },
        ToolDef {
            name: "memfork_delete",
            title: "Forget one memory",
            description: format!(
                "Remove one key. Use this when something recorded turns out to be \
                 wrong or obsolete. History is kept, so the old value is still \
                 reachable with memfork_at. {BRANCH_NOTE}"
            ),
            schema: object(&[("key", string("Key to remove.")), branch_arg()], &["key"]),
        },
        ToolDef {
            name: "memfork_list",
            title: "List memories by prefix",
            description: format!(
                "List keys and values in ascending key order, optionally limited to \
                 a prefix. Use this to see what you already know before adding to \
                 it, for example prefix `shop:decision:` to review every decision in \
                 project `shop`. {BRANCH_NOTE}"
            ),
            schema: object(
                &[
                    (
                        "prefix",
                        string("Only keys starting with this. Omit for all keys."),
                    ),
                    ("limit", integer("Stop after this many keys.")),
                    branch_arg(),
                ],
                &[],
            ),
        },
        ToolDef {
            name: "memfork_search",
            title: "Find memories by similarity",
            description: format!(
                "Find the entries whose embeddings are most similar to a query \
                 vector, by exact cosine similarity. Use this when you want \
                 whatever is relevant to a topic and do not know the key. Only \
                 entries stored with an embedding can be found this way; supply the \
                 vector yourself, since memfork does not embed text. {BRANCH_NOTE}"
            ),
            schema: object(
                &[
                    (
                        "embedding",
                        number_array("Query vector, same length as the stored ones."),
                    ),
                    ("k", integer("How many results to return. Defaults to 10.")),
                    ("prefix", string("Only consider keys starting with this.")),
                    branch_arg(),
                ],
                &["embedding"],
            ),
        },
        ToolDef {
            name: "memfork_fork",
            title: "Branch before a risky step",
            description: "Create a branch holding a copy of the current state, and switch to \
                 it. Fork before any risky or exploratory step — a refactor, a \
                 speculative plan, anything you might want to abandon. It costs the \
                 same whether memory holds ten entries or ten million, so there is \
                 no reason to hesitate. You are on the new branch as soon as this \
                 returns, so there is no separate checkout step and the next write \
                 lands on the fork without naming it. Afterwards call memfork_merge \
                 if the attempt worked, or memfork_discard if it did not. Nothing \
                 you write on the fork is visible to the branch you came from until \
                 you merge."
                .to_owned(),
            schema: object(
                &[
                    ("name", string("Name for the new branch.")),
                    (
                        "from",
                        string("Branch to fork from. Defaults to the current branch."),
                    ),
                    (
                        "at_seq",
                        integer(
                            "Fork from this point in history instead of the latest state. \
                             Use it to go back to how things were before a mistake.",
                        ),
                    ),
                ],
                &["name"],
            ),
        },
        ToolDef {
            name: "memfork_checkout",
            title: "Switch branch",
            description: "Switch to a branch you are not currently on. You do not need this \
                 after memfork_fork, which already switches, nor after \
                 memfork_discard, which moves you off the branch it deleted — both \
                 report where you ended up. Reach for this to return to a branch you \
                 left earlier; memfork_branches lists what exists."
                .to_owned(),
            schema: object(&[("name", string("Branch to switch to."))], &["name"]),
        },
        ToolDef {
            name: "memfork_merge",
            title: "Keep a branch's work",
            description: "Fold one branch's changes into another. Use this when work on a \
                 fork turned out well and should become part of the main line. A key \
                 changed on both sides to different values is a conflict: with \
                 policy `fail` (the default) nothing changes and the conflicting keys \
                 are returned, so you can look at them and decide; `ours` keeps the \
                 target's values and `theirs` takes the source's."
                .to_owned(),
            schema: object(
                &[
                    ("source", string("Branch to merge from.")),
                    (
                        "target",
                        string("Branch to merge into. Defaults to the current branch."),
                    ),
                    (
                        "policy",
                        string_enum(
                            "What to do with keys both sides changed. Defaults to `fail`.",
                            &["fail", "ours", "theirs"],
                        ),
                    ),
                ],
                &["source"],
            ),
        },
        ToolDef {
            name: "memfork_discard",
            title: "Throw a branch away",
            description: "Delete a branch and everything only it could reach. Use this when an \
                 attempt did not work out: the branch you forked from is left exactly \
                 as it was, with no trace of the attempt. The default branch cannot be \
                 discarded. If the session is on the branch being discarded, it \
                 switches back to the default branch."
                .to_owned(),
            schema: object(&[("name", string("Branch to delete."))], &["name"]),
        },
        ToolDef {
            name: "memfork_branches",
            title: "List branches",
            description: "List every branch with its current position and how many entries it \
                 holds. Use this to get your bearings — to see which attempts are \
                 still open before starting another."
                .to_owned(),
            schema: no_arguments(),
        },
        ToolDef {
            name: "memfork_log",
            title: "Review what changed",
            description: format!(
                "List a branch's history, newest first: what changed, in what order, \
                 with each point's sequence number. Use this to find the sequence \
                 number to pass to memfork_at or to memfork_fork's `at_seq` when you \
                 need to go back. {BRANCH_NOTE}"
            ),
            schema: object(
                &[
                    ("limit", integer("Stop after this many entries.")),
                    branch_arg(),
                ],
                &[],
            ),
        },
        ToolDef {
            name: "memfork_at",
            title: "Read the past",
            description: format!(
                "Read memory as it was after a past change, without altering \
                 anything now. Use this to check what you believed earlier, or what a \
                 value was before it was overwritten. Sequence 0 is the empty starting \
                 state; memfork_log shows which sequence number is which. Give `key` \
                 to read one entry, or `prefix` to list. {BRANCH_NOTE}"
            ),
            schema: object(
                &[
                    (
                        "seq",
                        integer("Point in history to read at, as shown by memfork_log."),
                    ),
                    ("key", string("Read this one key instead of listing.")),
                    (
                        "prefix",
                        string("When listing, only keys starting with this."),
                    ),
                    branch_arg(),
                ],
                &["seq"],
            ),
        },
        ToolDef {
            name: "memfork_resume",
            title: "Pick up where work left off",
            description: format!(
                "Get one short briefing on a project: the latest handoff note, the \
                 most recent decisions and the open tasks. Call this first, when you \
                 start work in a project or take over from another agent, before \
                 deciding anything, so you continue from what was already decided \
                 and done instead of starting over. The briefing is bounded in size; \
                 if more is stored it says so and which prefix to list. A project \
                 with nothing stored returns `empty: true`. {BRANCH_NOTE}"
            ),
            schema: object(&[namespace_arg(), branch_arg()], &[]),
        },
        ToolDef {
            name: "memfork_handoff",
            title: "Leave a handoff note",
            description: format!(
                "Record where the work stands so that another agent, or you in a \
                 later session, can resume it with memfork_resume. Call this before \
                 you stop, before switching to another tool or model, and whenever \
                 you are asked to pause or wrap up. Say what is done, what should \
                 happen next, what is blocking, and any open questions. Each call \
                 adds a new note; earlier ones are kept as history. {BRANCH_NOTE}"
            ),
            schema: object(
                &[
                    (
                        "summary",
                        string("Where things stand, in a sentence or two."),
                    ),
                    ("done", string_array("What was finished.")),
                    (
                        "next",
                        string_array("What should happen next, most important first."),
                    ),
                    ("blockers", string_array("What is in the way, if anything.")),
                    (
                        "questions",
                        string_array("Open questions that need someone's answer."),
                    ),
                    namespace_arg(),
                    branch_arg(),
                ],
                &["summary"],
            ),
        },
        ToolDef {
            name: "memfork_diff",
            title: "Compare two branches",
            description: "Show which keys differ between two branches, and how: added, \
                 removed or modified. Use this before merging to see what a branch \
                 would change, or to compare an attempt against the state it started \
                 from."
                .to_owned(),
            schema: object(
                &[
                    ("a", string("Left side: a branch name or a commit id.")),
                    ("b", string("Right side: a branch name or a commit id.")),
                ],
                &["a", "b"],
            ),
        },
    ]
}

/// Look one tool up by name.
pub fn find(name: &str) -> Option<ToolDef> {
    all().into_iter().find(|t| t.name == name)
}

/// Every tool name, in registry order.
pub fn names() -> Vec<&'static str> {
    all().iter().map(|t| t.name).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_matches_the_spec() {
        // DESIGN §6 lists exactly these fifteen tools.
        assert_eq!(
            names(),
            vec![
                "memfork_put",
                "memfork_get",
                "memfork_delete",
                "memfork_list",
                "memfork_search",
                "memfork_fork",
                "memfork_checkout",
                "memfork_merge",
                "memfork_discard",
                "memfork_branches",
                "memfork_log",
                "memfork_at",
                "memfork_resume",
                "memfork_handoff",
                "memfork_diff",
            ]
        );
    }

    #[test]
    fn descriptions_say_when_to_use_the_tool() {
        // DESIGN §6: "Tool descriptions must tell the model WHEN to use each
        // tool." A when-clause can be phrased as advice ("Use this to…") or as
        // an instruction ("Fork before any risky step"), so look for either.
        const WHEN_MARKERS: &[&str] = &["use this", " use ", "before ", "when ", "after "];
        for tool in all() {
            let lower = tool.description.to_ascii_lowercase();
            assert!(
                WHEN_MARKERS.iter().any(|m| lower.contains(m)),
                "`{}` does not say when to use it",
                tool.name
            );
            assert!(
                tool.description.len() > 80,
                "`{}` has a description too short to be useful",
                tool.name
            );
        }
    }

    #[test]
    fn descriptions_steer_away_from_redundant_calls() {
        // A model made three calls where one would do: it oriented itself, it
        // wrote, and then it read the write back. The descriptions have to say
        // that neither extra call is needed.
        let by_name = |name: &str| {
            all()
                .into_iter()
                .find(|t| t.name == name)
                .unwrap_or_else(|| panic!("{name} is in the registry"))
        };

        let fork = by_name("memfork_fork").description.to_ascii_lowercase();
        assert!(
            fork.contains("no separate checkout"),
            "memfork_fork does not say it already switches"
        );

        let put = by_name("memfork_put").description.to_ascii_lowercase();
        assert!(
            put.contains("no need to read the key back"),
            "memfork_put does not say the result confirms the write"
        );

        let checkout = by_name("memfork_checkout").description.to_ascii_lowercase();
        assert!(
            checkout.contains("do not need this after memfork_fork"),
            "memfork_checkout does not say when it is unnecessary"
        );

        // And every branch argument points at the field that answers it.
        for tool in all() {
            let Some(props) = tool.schema.get("properties").and_then(|p| p.as_object()) else {
                continue;
            };
            if let Some(branch) = props.get("branch") {
                let text = branch["description"].as_str().unwrap_or_default();
                assert!(
                    text.contains("current_branch"),
                    "{}'s `branch` argument does not mention where to read it from",
                    tool.name
                );
            }
        }
    }

    #[test]
    fn resume_and_handoff_say_when_in_the_work_to_call_them() {
        let by_name = |name: &str| {
            find(name)
                .unwrap_or_else(|| panic!("{name} is in the registry"))
                .description
                .to_ascii_lowercase()
        };
        let resume = by_name("memfork_resume");
        assert!(resume.contains("call this first"), "{resume}");
        assert!(resume.contains("start work"), "{resume}");
        let handoff = by_name("memfork_handoff");
        assert!(handoff.contains("before you stop"), "{handoff}");
        assert!(handoff.contains("before switching"), "{handoff}");
    }

    #[test]
    fn the_key_convention_is_colons_everywhere() {
        // One separator, the one prefix listing is built on. A slash anywhere
        // in a key example would teach a second convention.
        for tool in all() {
            for fragment in tool.description.split('`').skip(1).step_by(2) {
                if fragment.contains(':') {
                    assert!(
                        !fragment.contains('/') || fragment.starts_with("file:"),
                        "`{}` shows a key with a slash: {fragment}",
                        tool.name
                    );
                }
            }
        }
    }

    #[test]
    fn within_the_tool_limit() {
        // DESIGN §6.1: some clients take no more than sixteen tools per server.
        assert!(all().len() <= 16, "{} tools", all().len());
    }

    #[test]
    fn nothing_names_a_vendor_or_a_client() {
        // Vendor neutral, including in tool descriptions: no model or client
        // is named, and none is assumed.
        const VENDORS: &[&str] = &[
            "claude",
            "anthropic",
            "openai",
            "gpt",
            "chatgpt",
            "gemini",
            "google",
            "grok",
            "xai",
            "cursor",
            "codex",
            "copilot",
            "llama",
            "mistral",
            "ollama",
        ];
        for tool in all() {
            let text =
                format!("{} {} {}", tool.name, tool.title, tool.description).to_ascii_lowercase();
            for vendor in VENDORS {
                assert!(
                    !text.contains(vendor),
                    "`{}` mentions `{vendor}`",
                    tool.name
                );
            }
        }
    }
}
