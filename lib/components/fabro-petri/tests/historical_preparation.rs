//! Frozen admission from Petri 50809f9: reading old settings must never
//! recompile removed syntax or strip the command's encoded environment overlay.

#![expect(
    clippy::disallowed_methods,
    reason = "test reads frozen fixtures and command effects"
)]

mod support;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use fabro_petri::admission::{self, AdmittedGraphs};
use fabro_petri::engine::{self, Execution, RunError, RunRequest, RunStatus};
use fabro_petri::runtime::RuntimeSpec;
use fabro_petri::secrets::VaultSecrets;
use fabro_store::{PlatformRecord, test_support};
use fabro_types::{BlobHash, RunId, RunProjection, RunSpec, SecretType};
use fabro_vault::Vault;
use fabro_workflow::operations::{self, ForkedRunInput};
use petri_store::{
    Access, Digest, LogId, MemoryRunStore, Record, RunKey, RunLogs, RunStore, StoreError,
};
use support::{Silent, no_questions, run_request};

const SPEC: &str = include_str!("fixtures/historical-preparation/spec.json");
const GRAPH: &[u8] = include_bytes!("fixtures/historical-preparation/graph.json");

fn saved_spec() -> RunSpec {
    serde_json::from_str(SPEC).expect("the historical spec remains readable")
}

async fn saved_graphs(spec: &RunSpec) -> AdmittedGraphs {
    admission::load_with(
        |hash| async move {
            assert_eq!(hash, BlobHash::new(GRAPH));
            Ok(Some(bytes::Bytes::from_static(GRAPH)))
        },
        &spec.admission,
    )
    .await
    .expect("the saved graph keeps its digest")
}

async fn request(key: &str, root: &Path, store: Arc<dyn RunStore>) -> RunRequest {
    let mut vault = Vault::from_entries(HashMap::new());
    vault
        .set(
            "SETUP_TOKEN",
            "historical-fixture-token",
            SecretType::Token,
            None,
        )
        .expect("the fixture vault accepts its token");
    let mut request = run_request(
        key,
        root,
        saved_graphs(&saved_spec()).await,
        store,
        RuntimeSpec::default(),
        no_questions(Arc::new(Silent)),
    );
    request.secrets = Some(Arc::new(VaultSecrets::from_vault(&vault)));
    request
}

fn workspace(root: &Path) -> PathBuf {
    let scopes: Vec<_> = std::fs::read_dir(root.join("scopes"))
        .expect("the run retains its scopes")
        .map(|entry| entry.expect("a scope entry reads").path())
        .collect();
    assert_eq!(scopes.len(), 1);
    scopes[0].join("work")
}

#[tokio::test]
async fn historical_spec_projection_retry_and_fork_preserve_the_saved_admission() {
    let spec = saved_spec();
    let original: serde_json::Value = serde_json::from_str(SPEC).unwrap();
    assert!(original["settings"]["run"]["prepare"]["steps"].is_array());
    let projected = serde_json::to_value(&spec).unwrap();
    assert!(projected["settings"]["run"].get("prepare").is_none());
    assert_eq!(projected["admission"], original["admission"]);
    assert_eq!(projected["graph"], original["graph"]);
    let graphs = saved_graphs(&spec).await;
    assert_eq!(serde_json::to_vec(&graphs.graph).unwrap(), GRAPH);
    let json = serde_json::to_value(&graphs.graph).unwrap();
    let setup = json["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == "run_prepare_1")
        .unwrap();
    assert_eq!(
        setup["step"]["config"]["env"],
        serde_json::json!({"MODE":"step","TOKEN":{"$secret":"SETUP_TOKEN"}})
    );

    let source = RunProjection::new("Historical setup".into(), spec.clone(), chrono::Utc::now());
    let root = tempfile::tempdir().unwrap();
    let forked = operations::forked_run_record(&ForkedRunInput {
        source:         &source,
        new_run_id:     RunId::new(),
        run_dir:        root.path().join("fork"),
        checkpoint_sha: "fixture-checkpoint".into(),
        provenance:     None,
        web_url:        None,
        retried_from:   None,
    });
    assert_eq!(forked.spec.admission, spec.admission);
    assert_eq!(forked.spec.graph, spec.graph);
    saved_graphs(&forked.spec).await;

    let database = test_support::test_database();
    let retry_id = RunId::new();
    Box::pin(operations::persist_retried_run(
        &database,
        &source,
        retry_id,
        &root.path().join("retry"),
        spec.provenance.clone(),
        None,
    ))
    .await
    .unwrap();
    let records = database
        .run_summary_store()
        .platform_records()
        .read(&retry_id)
        .await
        .unwrap();
    let retry = records
        .iter()
        .find_map(|record| match &record.record {
            PlatformRecord::RunCreated(created) => Some(&created.spec),
            _ => None,
        })
        .unwrap();
    assert_eq!(retry.admission, spec.admission);
    assert_eq!(retry.graph, spec.graph);
    saved_graphs(retry).await;
}

#[tokio::test]
async fn saved_preparation_executes_again_on_a_fresh_retry() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryRunStore::new());
    for key in ["original", "retry"] {
        let run_dir = root.path().join(key);
        let outcome = engine::run(request(key, &run_dir, store.clone()).await)
            .await
            .unwrap();
        assert_eq!(outcome.status, RunStatus::Success);
        assert_eq!(
            std::fs::read_to_string(workspace(&run_dir).join("prepared")).unwrap(),
            "prepared\n"
        );
    }
}

#[tokio::test]
async fn saved_preparation_resumes_before_and_after_setup_without_duplicate_effects() {
    // The frozen linear graph's firings are start=1, setup=2, work=3. Fail the
    // durable start record before setup, then before work after setup completed.
    for firing in [2, 3] {
        let root = tempfile::tempdir().unwrap();
        let memory = Arc::new(MemoryRunStore::new());
        let failing = Arc::new(FailStart {
            inner: memory.clone(),
            firing,
            failed: Arc::new(AtomicBool::new(false)),
        });
        let result = engine::run(request("resume", root.path(), failing.clone()).await).await;
        assert!(matches!(result, Err(RunError::StoreFailed(_))));
        assert!(failing.failed.load(Ordering::SeqCst));
        if firing == 3 {
            assert_eq!(
                std::fs::read_to_string(workspace(root.path()).join("prepared")).unwrap(),
                "prepared\n"
            );
        }
        let mut resumed = request("resume", root.path(), memory.clone()).await;
        let Execution::Start(graphs) = resumed.execution else {
            unreachable!()
        };
        resumed.execution = Execution::Resume(graphs);
        let outcome = engine::run(resumed).await.unwrap();
        assert_eq!(outcome.status, RunStatus::Success);
        assert_eq!(
            std::fs::read_to_string(workspace(root.path()).join("prepared")).unwrap(),
            "prepared\n"
        );
    }
}

#[tokio::test]
async fn saved_generated_command_receives_ordinary_hooks_when_it_executes() {
    let root = tempfile::tempdir().unwrap();
    let memory = Arc::new(MemoryRunStore::new());
    let outcome = engine::run(request("hooks", root.path(), memory.clone()).await)
        .await
        .unwrap();
    assert_eq!(outcome.status, RunStatus::Success);
    let hooks = std::fs::read_to_string(workspace(root.path()).join("stage-hooks")).unwrap();
    assert_eq!(
        hooks
            .lines()
            .filter(|line| *line == "run_prepare_1")
            .count(),
        1
    );
    let mut resumed = request("hooks", root.path(), memory).await;
    let Execution::Start(graphs) = resumed.execution else {
        unreachable!()
    };
    resumed.execution = Execution::Resume(graphs);
    assert_eq!(
        engine::run(resumed).await.unwrap().status,
        RunStatus::Success
    );
    assert_eq!(
        std::fs::read_to_string(workspace(root.path()).join("stage-hooks")).unwrap(),
        hooks
    );
}

struct FailStart {
    inner:  Arc<MemoryRunStore>,
    firing: u64,
    failed: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl RunStore for FailStart {
    async fn open(&self, key: &RunKey, access: Access) -> Result<Arc<dyn RunLogs>, StoreError> {
        Ok(Arc::new(FailStartLogs {
            inner:  self.inner.open(key, access).await?,
            firing: self.firing,
            failed: self.failed.clone(),
        }))
    }
}

struct FailStartLogs {
    inner:  Arc<dyn RunLogs>,
    firing: u64,
    failed: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl RunLogs for FailStartLogs {
    fn locator(&self) -> String {
        self.inner.locator()
    }
    async fn append(&self, log: &LogId, records: &[Record]) -> Result<(), StoreError> {
        if records.iter().any(|r| {
            r.record["body"]["event"] == "step.started" && r.record["body"]["firing"] == self.firing
        }) && !self.failed.swap(true, Ordering::SeqCst)
        {
            return Err(StoreError::backend(
                self.locator(),
                "append",
                "injected crash before step start",
            ));
        }
        self.inner.append(log, records).await
    }
    async fn read(&self, log: &LogId) -> Result<Vec<Record>, StoreError> {
        self.inner.read(log).await
    }
    async fn put_blob(&self, bytes: &[u8]) -> Result<Digest, StoreError> {
        self.inner.put_blob(bytes).await
    }
    async fn get_blob(&self, digest: Digest) -> Result<Option<Vec<u8>>, StoreError> {
        self.inner.get_blob(digest).await
    }
}

#[tokio::test]
async fn saved_graph_forks_before_setup_and_executes_its_encoded_command() {
    use petri_execution::host::{ForkOptions, ForkPosition};
    use petri_execution::{host, inspect};
    use petri_runtime::RunOptions;
    use petri_runtime::ir::FiringId;

    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryRunStore::new());
    let source_dir = root.path().join("source");
    let outcome = engine::run(request("source", &source_dir, store.clone()).await)
        .await
        .unwrap();
    assert_eq!(outcome.status, RunStatus::Success);
    let logs = store
        .open(&RunKey::new("source"), Access::Read)
        .await
        .unwrap();
    let inspection = inspect::inspect_run(&*logs).await.unwrap();
    let execution = inspection
        .executions
        .iter()
        .find(|execution| execution.engine.is_some())
        .unwrap();
    let start = execution
        .engine
        .as_ref()
        .unwrap()
        .history
        .iter()
        .find(|entry| entry.node == "start")
        .unwrap();
    let fork_dir = root.path().join("fork");
    let mut options = RunOptions::new(&fork_dir);
    options.run_key = Some(RunKey::new("fork"));
    let runtime = RuntimeSpec::default()
        .runtime(true)
        .store(store.clone())
        .options(options);
    let forked = host::fork_from(
        &runtime,
        &*logs,
        ForkPosition {
            execution: execution.execution,
            firing:    FiringId(start.firing),
        },
        ForkOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        forked.graph.to_string(),
        saved_spec().admission.graph.digest
    );
    let mut resumed = request("fork", &fork_dir, store.clone()).await;
    let Execution::Start(graphs) = resumed.execution else {
        unreachable!()
    };
    resumed.execution = Execution::Resume(graphs);
    let outcome = engine::run(resumed).await.unwrap();
    assert_eq!(outcome.status, RunStatus::Success);
    assert_eq!(
        std::fs::read_to_string(workspace(&fork_dir).join("prepared")).unwrap(),
        "prepared\n"
    );
}
