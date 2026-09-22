//! MCP prompts: MemFork's routines offered as one-step commands, answered by
//! the proxy without starting a daemon.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::process::Stdio;

use rmcp::model::{ClientConfig, GetPromptRequestParams, Implementation};
use rmcp::service::ServiceExt;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use support::Sandbox;

#[tokio::test(flavor = "multi_thread")]
async fn a_client_is_offered_the_routines_and_gets_them_for_its_project() {
    let sandbox = Sandbox::new();
    let repo = sandbox.root().join("shop");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let mut std_cmd = sandbox.command();
    std_cmd.current_dir(&repo);
    let transport =
        TokioChildProcess::new(tokio::process::Command::from(std_cmd).configure(|cmd| {
            cmd.arg("mcp");
            cmd.stderr(Stdio::null());
        }))
        .unwrap();
    let client = ClientConfig::new(Default::default(), Implementation::new("any-client", "1.0"))
        .serve(transport)
        .await
        .unwrap();

    let offered = client.peer_info().unwrap().capabilities.prompts.is_some();
    assert!(offered, "the server does not say it has prompts");
    let prompts = client.list_all_prompts().await.unwrap();
    let names: Vec<&str> = prompts.iter().map(|p| p.name.as_str()).collect();
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
    assert!(
        sandbox.owner().is_none(),
        "listing prompts started a daemon"
    );

    let mut args = serde_json::Map::new();
    args.insert("task".to_owned(), serde_json::json!("add refunds"));
    let mut request = GetPromptRequestParams::new("resume");
    request.arguments = Some(args);
    let got = client.get_prompt(request).await.unwrap();
    let text = serde_json::to_string(&got).unwrap();
    assert!(
        text.contains("memfork_resume") && text.contains("Task: add refunds"),
        "{text}"
    );
    let review = client
        .get_prompt(GetPromptRequestParams::new("review-decisions"))
        .await
        .unwrap();
    assert!(serde_json::to_string(&review)
        .unwrap()
        .contains("`shop:decision:`"));
    assert!(client
        .get_prompt(GetPromptRequestParams::new("nope"))
        .await
        .is_err());
    client.cancel().await.unwrap();
}
