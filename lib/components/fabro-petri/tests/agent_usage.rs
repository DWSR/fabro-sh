//! An agent stage's usage, from Petri's real step metrics to the projected
//! stage: one agent stage runs on the real step registry against a scripted
//! model whose primary route answers once and then fails mid-prompt, so the
//! prompt continues on the fallback route. The finished stage keeps the
//! usage Petri reported and files each route's share under the model that
//! spent it, as `pebble.usage_by_model` breaks it down.
//!
//! Built-in Host scopes run in process without a plugin executable.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fabro_petri::projection::{Item, RunView};
use fabro_petri::runtime::RuntimeSpec;
use fabro_store::platform_records::{PlatformRecord, RunCreatedRecord, StoredPlatformRecord};
use fabro_types::{ModelRef, RunId, StageId, test_support as types_support};
use lithos_llm::catalog::{ModelId, ProviderId};
use lithos_llm::types::{ErrorKind, Usage};
use pebble_coding_agent::test_support::{
    ScriptedCall, ScriptedFailure, scripted_client, text_response, tool_call_response,
};
use petri_execution::events::{RunEvent, replay_run};
use petri_execution::host::{self, HostRun};
use petri_runtime::engine::Event;
use petri_runtime::executor::Retention;
use petri_runtime::frontend::CompileInputs;
use petri_runtime::ir::RunStatus;
use petri_runtime::{RunOptions, Runtime};
use petri_store::{Access, MemoryRunStore, RunKey, RunStore};
use serde_json::{Value, json};
use smol_str::SmolStr;
use tokio::fs;

/// One agent stage on the native backend, on the scripted catalog's
/// `test/model`.
const AGENT_WORKFLOW: &str = r#"digraph Agent {
    graph [goal="Say hello", backend="api", default_max_retries=0]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    work [shape=box, prompt="Say hello", model="model", provider="test", max_retries=0]
    start -> work -> exit
}"#;

/// The scripted catalog's `small` is the stage model's one fallback.
const AGENT_SETTINGS: &str = "_version = 1\n\n[workflow]\ngraph = \"workflow.fabro\"\n\n\
                              [run.model.fallbacks]\n\"model\" = [\"test:small\"]\n";

/// Write the agent bundle into `<root>/.fabro/workflows/agent`; the
/// workflow file.
async fn install_bundle(root: &Path) -> PathBuf {
    let bundle = root.join(".fabro").join("workflows").join("agent");
    fs::create_dir_all(&bundle)
        .await
        .expect("the bundle directory is creatable");
    fs::write(bundle.join("workflow.fabro"), AGENT_WORKFLOW)
        .await
        .expect("the workflow is writable");
    fs::write(bundle.join("workflow.toml"), AGENT_SETTINGS)
        .await
        .expect("the settings are writable");
    bundle.join("workflow.fabro")
}

fn runtime(
    run_dir: &Path,
    run_id: &str,
    model_client: lithos_llm::Client,
    store: &Arc<MemoryRunStore>,
) -> Runtime {
    let mut options = RunOptions::new(run_dir);
    options.grace = Duration::from_secs(2);
    options.retention = Retention::Never;
    options.echo = false;
    options.run_key = Some(RunKey::new(run_id));
    RuntimeSpec {
        model_client: Some(model_client),
        ..RuntimeSpec::default()
    }
    .runtime(true)
    .store(Arc::clone(store) as Arc<dyn RunStore>)
    .options(options)
}

/// Lower the bundle and run it to its end.
async fn run(rt: &Runtime, workflow: &Path) {
    let lowered = rt
        .check(workflow, None, None, &CompileInputs::new())
        .expect("the workflow file loads");
    let graph = lowered
        .graph
        .unwrap_or_else(|| panic!("the workflow lowers: {:?}", lowered.diagnostics));
    let report = host::run_configured(rt, HostRun::new(graph), |_, _| {})
        .await
        .expect("the run completes");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "errors: {:?}; history: {:#?}",
        report.state.errors(),
        report.state.history()
    );
}

/// The agent's finished step metrics, as Petri recorded them.
fn agent_metrics(events: &[RunEvent]) -> BTreeMap<SmolStr, Value> {
    events
        .iter()
        .find_map(|event| match event.engine() {
            Some(Event::StepFinished { outcome, .. })
                if outcome.metrics.custom.contains_key("pebble.usage") =>
            {
                Some(outcome.metrics.custom.clone())
            }
            _ => None,
        })
        .expect("the agent's step finished with its usage")
}

fn usage(value: &Value) -> Usage {
    serde_json::from_value(value.clone()).expect("a usage")
}

fn model(provider: &str, model: &str) -> ModelRef {
    ModelRef::new(ProviderId::new(provider), ModelId::new(model))
}

#[tokio::test]
async fn a_fallback_mid_prompt_keeps_each_routes_usage_under_its_model() {
    let root = tempfile::tempdir().expect("a temp dir");
    let workflow = install_bundle(root.path()).await;
    let run_id = RunId::new();
    let (client, provider) = scripted_client(vec![
        ScriptedCall::response(tool_call_response(
            "shell",
            "work",
            json!({"command": "true"}),
        )),
        ScriptedCall::Failure(ScriptedFailure::retryable(
            ErrorKind::Server,
            "primary down",
        )),
        ScriptedCall::response(text_response("Hello from small.")),
    ]);
    let store = Arc::new(MemoryRunStore::new());
    let rt = runtime(
        &root.path().join("run"),
        &run_id.to_string(),
        client,
        &store,
    );

    run(&rt, &workflow).await;

    let requested: Vec<String> = provider
        .requests()
        .iter()
        .map(|request| request.model().to_owned())
        .collect();
    assert_eq!(requested, ["test/model", "test/model", "test/small"]);

    // The run's events, folded as the projector folds them: the run's
    // `run.created` record first, then Petri's events in record order.
    let logs = store
        .open(&RunKey::new(run_id.to_string()), Access::Read)
        .await
        .expect("the run opens");
    let events = replay_run(&*logs).await.expect("the record replays");
    let mut spec = types_support::test_run_spec();
    spec.run_id = run_id;
    let created = StoredPlatformRecord {
        seq:         1,
        recorded_at: 0,
        record:      PlatformRecord::RunCreated(RunCreatedRecord {
            spec,
            title: Some("Say hello".to_string()),
            parent_id: None,
            retried_from: None,
            web_url: None,
        }),
        position:    None,
    };
    let mut view = RunView::new();
    view.fold(&Item::Platform(&created), 1);
    for (index, event) in events.iter().enumerate() {
        view.fold(&Item::Petri(event), index as u64 + 2);
    }
    let projection = view.projection().expect("the run has a projection");
    let stage = projection
        .stage(&StageId::new("work", 1))
        .expect("the agent stage is projected");

    // Petri's breakdown, one entry per route in first-use order.
    let metrics = agent_metrics(&events);
    let routes = metrics["pebble.usage_by_model"]
        .as_array()
        .expect("Petri breaks the root's usage down by route");
    assert_eq!(routes.len(), 2, "{metrics:?}");
    let primary = usage(&routes[0]["usage"]);
    let fallback = usage(&routes[1]["usage"]);
    assert!(primary.total_tokens() > 0, "{metrics:?}");
    assert!(fallback.total_tokens() > 0, "{metrics:?}");

    // The stage keeps every token Petri reported, and each route's share
    // stays under the model that spent it.
    assert_eq!(stage.usage, usage(&metrics["pebble.usage"]));
    assert_eq!(stage.usage, primary.saturating_add(fallback));
    let rows: Vec<(ModelRef, Usage)> = stage
        .usage_by_model
        .iter()
        .map(|row| (row.model.clone(), row.usage))
        .collect();
    assert_eq!(rows, [
        (model("test", "model"), primary),
        (model("test", "small"), fallback),
    ]);
}
