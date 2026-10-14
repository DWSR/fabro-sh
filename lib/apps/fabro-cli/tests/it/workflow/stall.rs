//! The real server/worker enforces the budget and serves the stall reason.

use std::time::Duration;

use super::{find_run_dir, fixture, read_conclusion, run_stream_items};

#[fabro_macros::e2e_test()]
fn local_quiet_workflow_stalls_through_the_worker_and_api() {
    let context = fabro_test::test_context!();
    context
        .run_cmd()
        .args(["--auto-approve", "--environment", "local"])
        .arg(fixture("stall.fabro"))
        .timeout(Duration::from_secs(30))
        .assert()
        .failure();
    let run_dir = find_run_dir(&context);
    let conclusion = read_conclusion(&run_dir);
    assert_eq!(conclusion["status"], "failed", "{conclusion:#}");
    assert_eq!(conclusion["failure"]["reason"], "cancelled");
    let message = conclusion["failure"]["detail"]["message"].as_str().unwrap();
    assert!(message.contains("stall timeout: 1000 ms"), "{message}");
    let items = run_stream_items(&run_dir);
    let stall = items
        .iter()
        .find(|item| item.name() == Some("run.stalled"))
        .expect("the API stream reports why the worker cancelled");
    let value = serde_json::to_value(stall).unwrap();
    assert_eq!(
        value["item"]["derived"]["stall_timeout_ms"], 1000,
        "{value:#}"
    );
    assert!(value["item"]["derived"]["idle_ms"].as_u64().unwrap() >= 1000);
}
