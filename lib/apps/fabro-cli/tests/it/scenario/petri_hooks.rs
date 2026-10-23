//! Ordinary command hooks through a real server and worker.

#![expect(
    clippy::disallowed_methods,
    reason = "scenario writes workflow inputs and reads workspace output"
)]

use std::path::PathBuf;

use fabro_test::test_context;

use super::petri::{RunningServer, run_detached, wait_for_status, write_petri_workflow};

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
