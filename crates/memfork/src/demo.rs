//! `memfork demo`: the whole product moving, on a store that is thrown away.
//!
//! A temporary directory holds a store and a small fake repository. A daemon
//! is started on that store, the Brain is opened on it, and two scripted
//! agents ([`crate::agent::FakeAgent`]) play a session through the real
//! tools: a briefing, a fact and a decision, a plan worked, a fork discarded
//! with a lesson, a handoff, a resume by another client, a fact going stale,
//! a decision disputed across branches, and a resume the next day. Every
//! light on the page is a real entry and every pulse a real event.
//!
//! Nothing is spent and nothing real is touched: the store is the demo's
//! own, no AI tool is needed, and on exit the daemon is stopped and the
//! directory removed.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;

use crate::agent::FakeAgent;
use crate::style::Style;

/// How to run the demo.
#[derive(Debug, Clone)]
pub struct Options {
    /// Print the page's address and do not open a browser.
    pub no_open: bool,
    /// No pauses between steps.
    pub fast: bool,
    /// Stop as soon as the script ends, rather than waiting for Ctrl+C.
    pub exit: bool,
    /// The two clients to stand in for, if not the registry's first two.
    pub agents: Option<String>,
}

/// The project the demo plays in.
pub const NAMESPACE: &str = "shop";

/// How long the demo's daemon waits with nothing to do before exiting on
/// its own, should the demo be killed before it can stop it.
const IDLE_SECONDS: u64 = 120;

/// The steps, in order, as the terminal narrates them.
const STEPS: [(&str, &str); 9] = [
    ("Agents are nodes too", "The first client connects and the engine serves its first briefing: empty, because nothing is stored yet."),
    ("A decision, and the fact it rests on", "The agent stores a fact tied to a file, then a decision that cites it. The edge runs from the file to the fact, and the pulse from the agent."),
    ("Coordination without an orchestrator", "It writes a plan and claims the first ready task. The plan is data in the store; the engine knows what is ready, what is blocked, and who holds what."),
    ("A dead end becomes a lesson", "The agent forks, tries storing cards for refunds, and the attempt breaks the no-card-data rule. The fork is discarded; one line survives, wired to the decision it violated."),
    ("The handoff", "Before stopping, the agent leaves a note: where things stand, what is next, what is blocking. It waits for someone to pick it up."),
    ("Another client picks it up", "The second client connects. The engine builds a briefing from the handoff, the decision, the fact and the lesson, and it claims the next task. Nobody pasted anything."),
    ("The engine notices", "A source file changed: the fact is stale, and the amber pulse runs from the file back to what depended on it. Then two branches disagree on one decision, and the engine flags it. It fixes nothing; it points."),
    ("A second session of the same client", "The next day, a fresh session of the first client resumes. Its briefing is only what changed since it last looked."),
    ("Every node is a door", "Drag the timeline to see the graph as it was. Search memory the way agents do. Click any node for its value, its sources, its history and what it is connected to. None of it changes anything."),
];

/// Run the demo to the end, or until Ctrl+C.
pub fn run(out: &mut impl Write, options: &Options, style: &Style) -> Result<(), String> {
    use crate::policy::{self, Feature};
    if !policy::allows(Feature::Brain) {
        return Err(policy::refusal(Feature::Brain));
    }
    let (first, second) = match &options.agents {
        Some(list) => {
            let mut parts = list.split(',').map(str::trim).filter(|s| !s.is_empty());
            let a = parts
                .next()
                .ok_or_else(|| "--agents needs two names, separated by a comma".to_owned())?;
            let b = parts
                .next()
                .ok_or_else(|| "--agents needs two names, separated by a comma".to_owned())?;
            (a.to_owned(), b.to_owned())
        }
        None => crate::agent::default_pair(),
    };

    // Everything the demo makes lives here, and goes when it ends.
    let tmp = tempfile::Builder::new()
        .prefix("memfork-demo-")
        .tempdir()
        .map_err(|e| format!("cannot make a temporary directory: {e}"))?;
    let store = tmp.path().join("store");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join("src").join("auth"))
        .and_then(|()| std::fs::create_dir_all(&store))
        .map_err(|e| format!("cannot set up the demo's directory: {e}"))?;
    let login = repo.join("src").join("auth").join("login.rs");
    std::fs::write(
        &login,
        "pub fn login(user: &str) -> Session {\n    // the entry point\n}\n",
    )
    .map_err(|e| format!("cannot write the demo's source file: {e}"))?;

    writeln!(
        out,
        "{}",
        style.dim(&format!(
            "a throwaway store in {}; your own memory is not touched",
            crate::style::path(tmp.path())
        ))
    )
    .map_err(|e| e.to_string())?;
    let endpoint = crate::daemon::ensure(&store, &crate::launch::resolve(), IDLE_SECONDS)
        .map_err(|e| e.to_string())?;
    let url = crate::brain::url(&endpoint)
        .ok_or_else(|| "the daemon published no read token".to_owned())?;
    writeln!(out, "{url}").map_err(|e| e.to_string())?;
    if options.no_open {
        writeln!(
            out,
            "{}",
            style.dim("open that address in a browser, then watch it here")
        )
        .map_err(|e| e.to_string())?;
    } else {
        crate::brain::open_in_browser(&url)?;
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the async runtime: {e}"))?;
    let outcome = runtime.block_on(async {
        let script = play(
            out, options, style, &endpoint, &repo, &login, &first, &second,
        );
        tokio::select! {
            result = script => result.map(|()| false),
            _ = tokio::signal::ctrl_c() => Ok(true),
        }
    });
    let interrupted = match outcome {
        Ok(interrupted) => interrupted,
        Err(e) => {
            let _ = crate::daemon::stop(&store);
            return Err(e);
        }
    };
    if !interrupted && !options.exit {
        writeln!(
            out,
            "\n{}",
            style.dim("the page stays open on this store; press Ctrl+C to finish and remove it")
        )
        .map_err(|e| e.to_string())?;
        runtime.block_on(async {
            let _ = tokio::signal::ctrl_c().await;
        });
    }
    writeln!(
        out,
        "\n{}",
        style.dim("stopping the demo's daemon and removing its store")
    )
    .map_err(|e| e.to_string())?;
    let _ = crate::daemon::stop(&store);
    tmp.close()
        .map_err(|e| format!("the demo's directory could not be removed: {e}"))?;
    writeln!(out, "{}", style.dim("done; nothing was kept")).map_err(|e| e.to_string())?;
    Ok(())
}

/// One step's heading and words.
fn narrate(out: &mut impl Write, style: &Style, index: usize) -> Result<(), String> {
    let (title, words) = STEPS[index];
    writeln!(
        out,
        "\n{} {}\n{}",
        style.dim(&format!("{}/{}", index + 1, STEPS.len())),
        style.strong(crate::style::palette::PRIMARY, title),
        words
    )
    .map_err(|e| e.to_string())
}

async fn pause(options: &Options, ms: u64) {
    if !options.fast {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
}

/// The script itself.
#[allow(clippy::too_many_arguments)]
async fn play(
    out: &mut impl Write,
    options: &Options,
    style: &Style,
    endpoint: &crate::persist::Endpoint,
    repo: &Path,
    login: &PathBuf,
    first: &str,
    second: &str,
) -> Result<(), String> {
    let put = |key: &str, value: &str| json!({ "key": key, "value": value });

    // 1. The first client, and an empty briefing.
    narrate(out, style, 0)?;
    let a = FakeAgent::connect(endpoint, first, NAMESPACE, repo).await?;
    a.call("memfork_resume", json!({})).await?;
    pause(options, 1600).await;

    // 2. A fact with a source, and a decision that cites it.
    narrate(out, style, 1)?;
    a.call(
        "memfork_put",
        json!({
            "key": "shop:fact:auth-entry",
            "value": "auth lives in src/auth, entry point login.rs",
            "sources": ["src/auth/login.rs"],
        }),
    )
    .await?;
    pause(options, 1200).await;
    a.call(
        "memfork_put",
        put(
            "shop:decision:payments",
            "hosted checkout: no card data on our servers; rests on fact:auth-entry",
        ),
    )
    .await?;
    pause(options, 1600).await;

    // 3. A plan, a claim, a task done.
    narrate(out, style, 2)?;
    a.call(
        "memfork_task",
        json!({ "action": "plan", "tasks": [
            { "id": "schema", "title": "design the order schema" },
            { "id": "pay", "title": "decide payments" },
            { "id": "checkout", "title": "checkout page", "depends_on": ["schema"] },
            { "id": "refunds", "title": "refunds", "depends_on": ["pay", "checkout"] },
            { "id": "tax", "title": "EU tax", "depends_on": ["refunds"] },
            { "id": "emails", "title": "order emails", "depends_on": ["refunds"] },
        ]}),
    )
    .await?;
    pause(options, 800).await;
    a.call("memfork_task", json!({ "action": "claim", "id": "schema" }))
        .await?;
    pause(options, 1400).await;
    a.call("memfork_task", json!({ "action": "done", "id": "schema" }))
        .await?;
    a.call("memfork_task", json!({ "action": "claim", "id": "pay" }))
        .await?;
    a.call("memfork_task", json!({ "action": "done", "id": "pay" }))
        .await?;
    pause(options, 1600).await;

    // 4. A fork, a bad idea, a discard with a lesson.
    narrate(out, style, 3)?;
    a.call("memfork_fork", json!({ "name": "try-refunds" }))
        .await?;
    pause(options, 700).await;
    a.call(
        "memfork_put",
        put(
            "shop:note:refund-cards",
            "store card numbers so refunds can be replayed",
        ),
    )
    .await?;
    pause(options, 700).await;
    a.call(
        "memfork_discard",
        json!({
            "name": "try-refunds",
            "lesson": "storing cards for refunds breaks decision:payments: no card data on our servers",
        }),
    )
    .await?;
    pause(options, 1600).await;

    // 5. The handoff, and the first client goes.
    narrate(out, style, 4)?;
    a.call(
        "memfork_handoff",
        json!({
            "summary": "checkout works, refunds next; blocked on a sandbox account",
            "done": ["order schema", "payments decided"],
            "next": ["refunds"],
            "blockers": ["a sandbox account for the payment provider"],
        }),
    )
    .await?;
    pause(options, 1000).await;
    a.disconnect().await;
    pause(options, 1200).await;

    // 6. The second client resumes, and picks up.
    narrate(out, style, 5)?;
    let b = FakeAgent::connect(endpoint, second, NAMESPACE, repo).await?;
    pause(options, 500).await;
    b.call("memfork_resume", json!({})).await?;
    pause(options, 1800).await;
    b.call(
        "memfork_task",
        json!({ "action": "claim", "id": "checkout" }),
    )
    .await?;
    b.call(
        "memfork_task",
        json!({ "action": "done", "id": "checkout" }),
    )
    .await?;
    pause(options, 600).await;
    b.call(
        "memfork_task",
        json!({ "action": "claim", "id": "refunds" }),
    )
    .await?;
    pause(options, 1600).await;

    // 7. A file changes; a decision is disputed.
    narrate(out, style, 6)?;
    std::fs::write(login, "pub fn login(user: &str, otp: &str) -> Session {\n    // the entry point, now with a second factor\n}\n")
        .map_err(|e| format!("cannot change the demo's source file: {e}"))?;
    b.call("memfork_get", json!({ "key": "shop:fact:auth-entry" }))
        .await?;
    pause(options, 1600).await;
    b.call("memfork_fork", json!({ "name": "try-eu" })).await?;
    b.call(
        "memfork_put",
        put(
            "shop:decision:payments",
            "a European provider with its own checkout page",
        ),
    )
    .await?;
    b.call("memfork_checkout", json!({ "name": "main" }))
        .await?;
    pause(options, 1800).await;

    // 8. The same client, another day: only what changed.
    narrate(out, style, 7)?;
    let again = FakeAgent::connect(endpoint, first, NAMESPACE, repo).await?;
    pause(options, 600).await;
    again.call("memfork_resume", json!({})).await?;
    pause(options, 1600).await;
    again.disconnect().await;
    b.disconnect().await;

    // 9. Over to the person.
    narrate(out, style, 8)?;
    Ok(())
}
