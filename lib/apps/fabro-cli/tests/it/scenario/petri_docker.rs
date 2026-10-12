//! Petri runs on a Docker environment through a real server and its
//! worker: a non-Git workspace survives a worker restart in its retained
//! container. A deleted container fails resume; Fabro does not reconstruct it.
//!
//! An Ask Fabro session on a finished Docker run attaches to the container
//! Petri created and reads a file the workflow wrote there, its model the
//! twin.
//!
//! The runs use the built-in Docker provider on this machine's daemon.
//! Tests skip when no daemon answers, unless `FABRO_REQUIRE_SANDBOX_BACKENDS`
//! requires it. The server, detached run and crash come from `petri.rs`.

#![expect(
    clippy::disallowed_methods,
    reason = "these scenarios inspect backend availability and drive the Docker daemon with its CLI"
)]

use std::env;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use fabro_static::EnvVars;
use fabro_test::{
    TwinScenario, TwinScenarios, TwinToolCall, expect_reqwest_json, test_context, twin_openai,
};
use serde_json::json;

use super::petri::{
    RunningServer, crash, run_detached_in, run_json, wait_for_status, wait_for_success,
    wait_for_worker, write_petri_workflow,
};
use crate::support::TEST_DEV_TOKEN;

/// The server-side environment the runs select.
const ENVIRONMENT: &str = "docker";
/// The twin's model, for the Ask Fabro session.
const MODEL: &str = "gpt-5.4";

/// A server with a Docker environment beside the default local one.
async fn docker_server() -> RunningServer {
    docker_server_with("", &[]).await
}

/// `docker_server`, with `settings` appended to the server's settings and
/// `secrets` in its vault.
async fn docker_server_with(settings: &str, secrets: &[(&str, &str)]) -> RunningServer {
    let server = RunningServer::start_with(settings, secrets).await;
    let body = json!({
        "id": ENVIRONMENT,
        "provider": "docker",
        "image": { "docker": null, "dockerfile": null },
        "resources": { "cpu": null, "memory": null, "disk": null },
        "network": { "mode": "allow_all", "allow": [] },
        "lifecycle": { "preserve": false, "stop_on_terminal": true, "auto_stop": null },
        "labels": {},
        "env": {}
    });
    let response = fabro_test::test_http_client()
        .post(format!("{}/api/v1/environments", server.api_base_url))
        .bearer_auth(TEST_DEV_TOKEN)
        .json(&body)
        .send()
        .await
        .expect("the environment create sends");
    expect_reqwest_json(
        response,
        fabro_http::StatusCode::CREATED,
        "POST /api/v1/environments",
    )
    .await;
    server
}

/// The run's container on the daemon, by Petri's run label: the one the
/// run's scope lives in.
fn container_of(run_id: &str) -> Option<String> {
    let output = Command::new("docker")
        .args([
            "ps",
            "-aq",
            "--filter",
            &format!("label=petri.run={run_id}"),
        ])
        .output()
        .expect("docker ps runs");
    let ids: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    assert!(ids.len() <= 1, "one container per run: {ids:?}");
    ids.into_iter().next()
}

/// `sh -c script` inside the container's workspace.
fn docker_exec(container: &str, script: &str) -> String {
    let output = Command::new("docker")
        .args(["exec", "-w", "/workspace", container, "sh", "-c", script])
        .output()
        .expect("docker exec runs");
    assert!(
        output.status.success(),
        "docker exec failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn docker_rm(container: &str) {
    let status = Command::new("docker")
        .args(["rm", "-f", container])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("docker rm runs");
    assert!(status.success(), "the container is removed");
}

/// Remove whatever the run left on the daemon, so a failed assertion does
/// not leak a container.
fn cleanup(run_id: &str) {
    if let Some(container) = container_of(run_id) {
        docker_rm(&container);
    }
}

/// Two command stages: `one` writes a file; `two` checks it is the one
/// `one` wrote, that nothing else is in the workspace beside the
/// repository's own files, and writes another.
fn two_stage_bundle(context: &fabro_test::TestContext) -> PathBuf {
    write_petri_workflow(
        context,
        "digraph Stages {\n  graph [goal=\"Two stages\", default_max_retries=0]\n  start \
         [shape=Mdiamond]\n  exit [shape=Msquare]\n  one [shape=parallelogram, script=\"echo one > \
         one.txt\"]\n  two [shape=parallelogram, script=\"test \\\"$(cat one.txt)\\\" = one && \
         test ! -e stray.txt && echo two > two.txt\"]\n  start -> one -> two -> exit\n}\n",
    )
}

/// A non-Git Docker run keeps files only inside its container.
#[tokio::test(flavor = "multi_thread")]
async fn a_non_git_docker_run_keeps_its_workspace_in_the_container() {
    if !fabro_test::docker_available() {
        return;
    }
    let context = test_context!();
    let server = docker_server().await;
    let workspace = two_stage_bundle(&context);
    let run_id = run_detached_in(&context, &server, &workspace, ENVIRONMENT, &[
        "--auto-approve",
    ]);
    wait_for_success(&server, &run_id).await;

    assert!(server.checkpoints(&run_id).await.is_empty());
    assert!(!server.petri_run_dir(&run_id).join("snapshots").exists());
    assert!(
        !server.petri_run_dir(&run_id).join("scopes").exists(),
        "no workspace is on the host"
    );
    assert!(
        container_of(&run_id).is_some(),
        "the container is retained after the run"
    );
    cleanup(&run_id);
    server.shutdown();
}

/// A non-Git run resumes in the retained container with its existing files.
#[tokio::test(flavor = "multi_thread")]
async fn a_non_git_run_resumes_in_its_retained_container() {
    if !fabro_test::docker_available() {
        return;
    }
    let context = test_context!();
    let mut server = docker_server().await;
    let workspace = two_stage_bundle(&context);
    server.hold("record", "one");
    let run_id = run_detached_in(&context, &server, &workspace, ENVIRONMENT, &[
        "--auto-approve",
    ]);

    wait_for_status(&server, &run_id, &["running"]).await;
    let worker = wait_for_worker(&run_id);
    server.wait_until_held(&run_id, "record", "one");
    let container = container_of(&run_id).expect("the run's container exists");
    docker_exec(&container, "echo marker > marker.txt && test ! -d .git");
    crash(&mut server, worker, None);

    server.release("record", "one");
    server.launch().await;
    let resumed = wait_for_worker(&run_id);
    assert_ne!(resumed, worker);
    wait_for_success(&server, &run_id).await;

    assert_eq!(
        container_of(&run_id).as_deref(),
        Some(container.as_str()),
        "the run continued in its retained container"
    );

    assert!(server.checkpoints(&run_id).await.is_empty());
    assert!(!server.petri_run_dir(&run_id).join("snapshots").exists());
    cleanup(&run_id);
    server.shutdown();
}

/// A deleted container fails resume instead of silently creating an empty
/// replacement workspace.
#[tokio::test(flavor = "multi_thread")]
async fn a_lost_container_is_not_replaced_on_resume() {
    if !fabro_test::docker_available() {
        return;
    }
    let context = test_context!();
    let mut server = docker_server().await;
    let workspace = two_stage_bundle(&context);
    server.hold("record", "one");
    let run_id = run_detached_in(&context, &server, &workspace, ENVIRONMENT, &[
        "--auto-approve",
    ]);

    wait_for_status(&server, &run_id, &["running"]).await;
    let worker = wait_for_worker(&run_id);
    server.wait_until_held(&run_id, "record", "one");
    let container = container_of(&run_id).expect("the run's container exists");
    crash(&mut server, worker, None);
    docker_rm(&container);
    assert_eq!(container_of(&run_id), None, "the container is gone");

    server.release("record", "one");
    server.launch().await;
    assert_eq!(
        wait_for_status(&server, &run_id, &["failed", "succeeded"]).await,
        "failed"
    );
    assert_eq!(
        container_of(&run_id),
        None,
        "resume must not replace the lost container"
    );
    assert!(!server.petri_run_dir(&run_id).join("snapshots").exists());
    cleanup(&run_id);
    server.shutdown();
}

/// What the session is asked, and what the twin is told to answer once it
/// has read the file.
const QUESTION: &str = "Read hello.txt in the workspace and tell me what it says.";
const CONTENT: &str = "hello-from-petri";

/// A one-stage bundle whose command writes `hello.txt` into the workspace.
fn hello_file_bundle(context: &fabro_test::TestContext) -> PathBuf {
    write_petri_workflow(
        context,
        &format!(
            "digraph Hello {{\n  graph [goal=\"Write a file\", default_max_retries=0]\n  start \
             [shape=Mdiamond]\n  exit [shape=Msquare]\n  write [shape=parallelogram, script=\"echo \
             {CONTENT} > hello.txt\"]\n  start -> write -> exit\n}}\n"
        ),
    )
}

/// The twin's script for the session's turn.
fn turn_scenario() -> TwinScenario {
    TwinScenario::responses(MODEL).input_contains(QUESTION)
}

/// The events of a session turn's stream, in order.
fn turn_events(stream: &str) -> Vec<serde_json::Value> {
    stream
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).expect("session event data is JSON"))
        .collect()
}

/// The twin's request log for `namespace`: the input text of each request
/// that carried the session's question, in order. The server's other
/// requests to the twin (a run title) are left out.
async fn question_inputs(twin: &fabro_test::TwinOpenAi, namespace: &str) -> Vec<String> {
    let logs = twin.request_logs(namespace).await;
    logs["requests"]
        .as_array()
        .expect("the twin request log is an array")
        .iter()
        .map(|request| {
            request["input_text"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .filter(|input| input.contains(QUESTION))
        .collect()
}

/// Ask Fabro on a finished Docker run, through a real server: the session
/// attaches to the container Petri created (stopped at the run's end, so
/// the attach starts it again) and its tool reads a file the workflow
/// wrote inside it. Ask Fabro's tool policy is read-only: the shell tool
/// is hidden from the model and refused, so the turn reads the file with
/// the model's `read_file` tool, scripted on the twin, and the twin's
/// follow-up request carries the file's content back as the tool's answer.
#[tokio::test(flavor = "multi_thread")]
async fn an_ask_fabro_turn_reads_a_file_inside_the_runs_container() {
    if !fabro_test::docker_available() {
        return;
    }
    let context = test_context!();
    let twin = twin_openai().await;
    let namespace = format!("{}::{}", module_path!(), line!());
    let server = docker_server_with(
        &format!(
            "\n[llm.providers.openai]\nbase_url = \"{}\"\n",
            twin.base_url
        ),
        &[(EnvVars::OPENAI_API_KEY, namespace.as_str())],
    )
    .await;
    TwinScenarios::new(namespace.clone())
        .scenario(turn_scenario().tool_call(TwinToolCall::new(
            "read_file",
            json!({ "file_path": "/workspace/hello.txt" }),
        )))
        .scenario(turn_scenario().text(format!("hello.txt says: {CONTENT}")))
        .load(twin)
        .await;
    let workspace = hello_file_bundle(&context);
    let run_id = run_detached_in(&context, &server, &workspace, ENVIRONMENT, &[
        "--auto-approve",
    ]);
    wait_for_success(&server, &run_id).await;
    assert!(
        container_of(&run_id).is_some(),
        "the container is retained after the run"
    );

    let client = fabro_test::test_http_client();
    let response = client
        .post(format!(
            "{}/api/v1/runs/{run_id}/sessions",
            server.api_base_url
        ))
        .bearer_auth(TEST_DEV_TOKEN)
        .json(&json!({ "title": "Ask Fabro", "model": MODEL }))
        .send()
        .await
        .expect("the session create sends");
    let session = expect_reqwest_json(
        response,
        fabro_http::StatusCode::CREATED,
        "POST /api/v1/runs/{id}/sessions",
    )
    .await;
    let session_id = session["id"].as_str().expect("the session id");

    let response = client
        .post(format!(
            "{}/api/v1/sessions/{session_id}/turns",
            server.api_base_url
        ))
        .bearer_auth(TEST_DEV_TOKEN)
        .json(&json!({ "input": QUESTION }))
        .send()
        .await
        .expect("the turn sends");
    assert_eq!(
        response.status(),
        fabro_http::StatusCode::OK,
        "POST /api/v1/sessions/{{id}}/turns"
    );
    // The stream ends with the turn.
    let stream = response.text().await.expect("the turn's stream reads");
    let events = turn_events(&stream);

    let outcome = events
        .iter()
        .find(|event| {
            event["event"] == "run.session.turn.succeeded"
                || event["event"] == "run.session.turn.failed"
        })
        .unwrap_or_else(|| panic!("the turn ends: {events:?}"));
    assert_eq!(
        outcome["event"],
        "run.session.turn.succeeded",
        "the turn ended in the container: {outcome}\nserver stderr:\n{}",
        server.stderr_text()
    );
    let read = events
        .iter()
        .find(|event| {
            event["event"] == "run.session.tool_call.completed"
                && event["properties"]["tool_name"] == "read_file"
        })
        .unwrap_or_else(|| panic!("the read_file call completed: {events:?}"));
    assert_eq!(read["properties"]["is_error"], false, "{read}");
    assert!(
        read["properties"]["output"].to_string().contains(CONTENT),
        "the tool read the file inside the container: {read}"
    );
    // The tool-call round's assistant message carries no text; the reply
    // is the last one.
    let reply = events
        .iter()
        .rev()
        .find(|event| event["event"] == "run.session.assistant_message")
        .unwrap_or_else(|| panic!("the model replied: {events:?}"));
    assert!(
        reply["properties"]["text"]
            .as_str()
            .is_some_and(|text| text.contains(CONTENT)),
        "the reply names the file's content: {reply}"
    );

    // The twin's follow-up request carried the tool's answer.
    let inputs = question_inputs(twin, &namespace).await;
    assert_eq!(inputs.len(), 2, "{inputs:?}");
    assert!(
        inputs[1].contains(CONTENT),
        "the model read the file's content from the tool: {}",
        inputs[1]
    );

    cleanup(&run_id);
    server.shutdown();
}

/// An unavailable daemon refuses cleanup without changing a completed run,
/// even after repeated requests and server restarts. `DOCKER_HOST` is
/// overridden only on the disposable server, so other tests and the real daemon
/// are intact.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker; creates a container and restarts a disposable server"]
async fn docker_failed_cleanup_preserves_success_across_server_restarts() {
    assert!(
        fabro_test::docker_available(),
        "this explicit test requires Docker"
    );
    let context = test_context!();
    let mut server = docker_server().await;
    let workspace = hello_file_bundle(&context);
    let run_id = run_detached_in(&context, &server, &workspace, ENVIRONMENT, &[
        "--auto-approve",
    ]);
    let _cleanup = scopeguard::guard(run_id.clone(), |id| cleanup(&id));
    wait_for_success(&server, &run_id).await;
    let before = run_json(&server, &format!("runs/{run_id}/state")).await;
    let container = container_of(&run_id).expect("the completed run retains its container");
    let absent_socket = format!("unix://{}", context.temp_dir.join("absent.sock").display());
    server.kill();
    server
        .launch_with_env(&[(EnvVars::DOCKER_HOST, &absent_socket)])
        .await;
    let client = fabro_test::test_http_client();
    for _ in 0..2 {
        let response = client
            .delete(format!("{}/api/v1/runs/{run_id}", server.api_base_url))
            .bearer_auth(TEST_DEV_TOKEN)
            .send()
            .await
            .expect("the delete sends");
        fabro_test::expect_reqwest_status(
            response,
            fabro_http::StatusCode::CONFLICT,
            "DELETE /api/v1/runs/{id} with unavailable Docker",
        )
        .await;
        let after = run_json(&server, &format!("runs/{run_id}/state")).await;
        for field in ["status", "status_updated_at", "conclusion"] {
            assert_eq!(after[field], before[field], "cleanup changed {field}");
        }
        assert_eq!(container_of(&run_id).as_deref(), Some(container.as_str()));
    }
    server.kill();
    server.launch().await;
    let after = run_json(&server, &format!("runs/{run_id}/state")).await;
    for field in ["status", "status_updated_at", "conclusion"] {
        assert_eq!(after[field], before[field], "restart changed {field}");
    }
    assert_eq!(container_of(&run_id).as_deref(), Some(container.as_str()));
    let response = client
        .delete(format!("{}/api/v1/runs/{run_id}", server.api_base_url))
        .bearer_auth(TEST_DEV_TOKEN)
        .send()
        .await
        .expect("the retry sends");
    fabro_test::expect_reqwest_status(
        response,
        fabro_http::StatusCode::NO_CONTENT,
        "DELETE /api/v1/runs/{id} after restoring Docker",
    )
    .await;
    assert_eq!(
        container_of(&run_id),
        None,
        "successful retry deletes the container"
    );
    let response = client
        .get(format!("{}/api/v1/runs/{run_id}", server.api_base_url))
        .bearer_auth(TEST_DEV_TOKEN)
        .send()
        .await
        .expect("the run read sends");
    fabro_test::expect_reqwest_status(
        response,
        fabro_http::StatusCode::NOT_FOUND,
        "GET /api/v1/runs/{id} after successful deletion",
    )
    .await;
    server.shutdown();
}
