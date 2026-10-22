//! Explicit setup commands through a real server and worker, including the
//! intentionally broader workflow-wide scope of migrated secrets.

#![expect(
    clippy::disallowed_methods,
    reason = "scenario writes workflow inputs and reads workspace output"
)]

use std::path::PathBuf;

use fabro_test::test_context;

use super::petri::{
    RunningServer, run_detached, run_json, settled_stream, wait_for_status, write_petri_workflow,
};

fn run_workspace(server: &RunningServer, run_id: &str) -> PathBuf {
    let scopes = server.petri_run_dir(run_id).join("scopes");
    let mut workspaces: Vec<PathBuf> = std::fs::read_dir(scopes)
        .expect("the run has retained scopes")
        .map(|entry| entry.expect("a scope reads").path().join("work"))
        .collect();
    assert_eq!(workspaces.len(), 1, "one workspace: {workspaces:?}");
    workspaces.remove(0)
}

#[tokio::test]
async fn explicit_setup_orders_commands_and_shares_redacted_workflow_secrets() {
    let context = test_context!();
    let secret = "fixture-setup-token-72c9e1";
    let server = RunningServer::start_with("", &[("SETUP_TOKEN", secret)]).await;
    let workspace = write_petri_workflow(
        &context,
        r#"digraph Setup {
        start [shape=Mdiamond]
        quoted [shape=parallelogram, script="printf '%s' 'a space, '\"'\"'quote'\"'\"'; $HOME' > quoted-argument", timeout="5m", on_failure="exit", max_retries=0]
        install [shape=parallelogram, script="test -n \"$SETUP_TOKEN\" && printf 'setup token=%s\n' \"$SETUP_TOKEN\" && touch installed", timeout="5m", on_failure="exit", max_retries=0]
        database [shape=parallelogram, script="test -f installed && touch prepared", timeout="5m", on_failure="exit", max_retries=0]
        work [shape=parallelogram, script="test -f prepared && test -n \"$SETUP_TOKEN\" && touch finished"]
        exit [shape=Msquare]
        start -> quoted -> install -> database -> work -> exit
    }"#,
    );
    std::fs::write(
        workspace.join("workflow.toml"),
        r#"_version = 1
[workflow]
graph = "workflow.fabro"
[run.environment.env]
SETUP_TOKEN = "{{ secrets.SETUP_TOKEN }}"
"#,
    )
    .unwrap();

    let run_id = run_detached(&context, &server, &workspace);
    assert_eq!(
        wait_for_status(&server, &run_id, &["succeeded", "failed"]).await,
        "succeeded"
    );
    let workspace = run_workspace(&server, &run_id);
    assert!(workspace.join("finished").exists());
    assert_eq!(
        std::fs::read_to_string(workspace.join("quoted-argument")).unwrap(),
        "a space, 'quote'; $HOME"
    );
    let state = run_json(&server, &format!("runs/{run_id}/state")).await;
    for node in ["quoted", "install", "database", "work"] {
        assert_eq!(state["spec"]["graph"]["nodes"][node]["kind"], "command");
    }
    assert!(state["spec"]["settings"]["run"].get("prepare").is_none());
    let stream = settled_stream(&server, &run_id).await;
    let serialized = serde_json::to_string(&stream).unwrap();
    assert!(
        !serialized.contains(secret),
        "the stream must redact the workflow secret"
    );
    assert!(
        serialized.contains("setup token=***"),
        "the redacted output must remain visible"
    );
    for node in ["quoted", "install", "database", "work"] {
        assert!(
            stream
                .iter()
                .any(|item| item["item"]["subject"]["node"]["name"] == node
                    && item["item"]["derived"]["event"] == "visit.completed"),
            "missing events for {node}"
        );
    }
    server.shutdown();
}

#[tokio::test]
async fn failed_setup_stops_downstream_work_without_retrying_shell_exit() {
    let context = test_context!();
    let server = RunningServer::start().await;
    let workspace = write_petri_workflow(
        &context,
        r#"digraph SetupFailure {
        graph [default_max_retries=3]
        start [shape=Mdiamond]
        setup [shape=parallelogram, script="echo attempt >> attempts; exit 1", timeout="5m", on_failure="exit"]
        work [shape=parallelogram, script="touch should-not-exist"]
        exit [shape=Msquare]
        start -> setup -> work -> exit
    }"#,
    );
    let run_id = run_detached(&context, &server, &workspace);
    assert_eq!(
        wait_for_status(&server, &run_id, &["succeeded", "failed"]).await,
        "failed"
    );
    let workspace = run_workspace(&server, &run_id);
    assert_eq!(
        std::fs::read_to_string(workspace.join("attempts")).unwrap(),
        "attempt\n"
    );
    assert!(!workspace.join("should-not-exist").exists());
    server.shutdown();
}

#[tokio::test]
async fn a_command_with_the_former_preparation_prefix_receives_ordinary_hooks() {
    let context = test_context!();
    let server = RunningServer::start().await;
    let workspace = write_petri_workflow(
        &context,
        r#"digraph Hooked {
        start [shape=Mdiamond]
        run_prepare_1 [shape=parallelogram, script="touch prepared", timeout="5m", on_failure="exit"]
        exit [shape=Msquare]
        start -> run_prepare_1 -> exit
    }"#,
    );
    std::fs::write(
        workspace.join("workflow.toml"),
        r#"_version = 1
[workflow]
graph = "workflow.fabro"
[[run.hooks]]
event = "stage_start"
matcher = "^run_prepare_1$"
script = "echo hook >> hooks"
"#,
    )
    .unwrap();
    let run_id = run_detached(&context, &server, &workspace);
    assert_eq!(
        wait_for_status(&server, &run_id, &["succeeded", "failed"]).await,
        "succeeded"
    );
    let workspace = run_workspace(&server, &run_id);
    assert!(workspace.join("prepared").exists());
    assert_eq!(
        std::fs::read_to_string(workspace.join("hooks")).unwrap(),
        "hook\n"
    );
    server.shutdown();
}
