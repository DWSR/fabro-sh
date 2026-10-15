//! Skills discovered through Fabro's runtime expand only authored native input.

mod support;

use std::sync::{Arc, Mutex};

use fabro_llm::test_support::{self, ScriptedAdapter};
use fabro_petri::check::Launch;
use fabro_petri::engine::{self, RunStatus};
use fabro_petri::runtime::RuntimeSpec;
use lithos_llm::Client;
use lithos_llm::catalog::ProviderId;
use lithos_llm::middleware::{Call, Observer, ObserverMiddleware};
use lithos_llm::types::{ContentPart, Request};
use petri_store::MemoryRunStore;
use support::{Silent, no_questions, run_request};
use tokio::fs;

const MODEL: &str = "gpt-5.4";
const TEMPLATE: &str = "COMMIT TEMPLATE SENTINEL";
const PRIOR: &str = "smoke-tested via a /tmp build; reviewer mentioned /commit";

struct Requests(Arc<Mutex<Vec<Request>>>);

impl Observer for Requests {
    fn on_start(&self, call: &Call) {
        self.0
            .lock()
            .expect("request capture lock")
            .push(call.request().clone());
    }
}

async fn run_native(workflow: &str, responses: &[&str]) -> Vec<Request> {
    let root = tempfile::tempdir().expect("temporary runtime home");
    let home = root.path().join("fabro-home");
    let skill = home.join("skills/commit");
    fs::create_dir_all(&skill).await.expect("skill directory");
    // No placeholder: expansion must replace only the authored input.
    fs::write(
        skill.join("SKILL.md"),
        format!("---\nname: commit\ndescription: Test commit instructions\n---\n{TEMPLATE}\n"),
    )
    .await
    .expect("skill template");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let client = Client::builder()
        .catalog(test_support::test_catalog())
        .adapter(
            ProviderId::new("openai"),
            ScriptedAdapter::new(
                responses
                    .iter()
                    .map(|text| test_support::text_response("openai", MODEL, text))
                    .collect(),
            ),
        )
        .middleware(ObserverMiddleware::new(Requests(Arc::clone(&requests))))
        .build()
        .expect("scripted model client")
        .client;
    let runtime = RuntimeSpec {
        model_client: Some(client),
        fabro_home: Some(home),
        ..RuntimeSpec::default()
    };
    let graphs = support::admit(
        &[
            ("workflow.fabro", workflow),
            ("workflow.toml", support::SETTINGS),
        ],
        Launch {
            default_model: Some(format!("openai/{MODEL}")),
            ..Launch::default()
        },
        &runtime,
    );
    let outcome = engine::run(run_request(
        "skills",
        &root.path().join("run"),
        graphs,
        Arc::new(MemoryRunStore::new()),
        runtime,
        no_questions(Arc::new(Silent)),
    ))
    .await
    .expect("workflow run settles");
    assert_eq!(outcome.status, RunStatus::Success, "{outcome:?}");
    let requests = requests.lock().expect("request capture lock").clone();
    for request in &requests {
        for message in request.messages() {
            assert!(
                message
                    .content()
                    .iter()
                    .all(|part| !matches!(part, ContentPart::Text { text } if text.is_empty())),
                "absent segments must not become invalid empty text blocks: {message:?}"
            );
        }
    }
    requests
}

fn user_text(request: &Request) -> String {
    request
        .messages()
        .last()
        .expect("user input message")
        .content()
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

#[tokio::test]
async fn authored_skill_commands_preserve_workflow_context_and_output_requirements() {
    for (authored, expands) in [
        ("  /commit authored arguments /tmp /commit", true),
        ("Address the feedback about /commit and /tmp", false),
    ] {
        let workflow = format!(
            r#"digraph Skills {{
                graph [backend="api", goal="Preserve workflow context", default_max_retries=0]
                start [shape=Mdiamond]
                prior [prompt="Report the previous result"]
                task [prompt="{authored}", fidelity="summary:high", output_schema="routing", output_retries=0]
                exit [shape=Msquare]
                start -> prior -> task -> exit
            }}"#
        );
        let requests = run_native(&workflow, &[PRIOR, r#"{"outcome":"succeeded"}"#]).await;
        assert_eq!(requests.len(), 2);
        let sent = user_text(&requests[1]);
        assert!(sent.contains("Preserve workflow context"), "{sent}");
        assert!(sent.contains(PRIOR), "{sent}");
        assert!(sent.contains("Fabro final-output contract"), "{sent}");
        assert_eq!(sent.contains(TEMPLATE), expands, "{sent}");
        if expands {
            assert!(!sent.contains(authored), "{sent}");
            assert!(sent.find(PRIOR) < sent.find(TEMPLATE), "{sent}");
            assert!(
                sent.find(TEMPLATE) < sent.find("Fabro final-output contract"),
                "{sent}"
            );
        } else {
            assert!(sent.contains(authored), "{sent}");
        }
    }
}

#[tokio::test]
async fn absent_native_prompt_segments_do_not_emit_empty_model_text_blocks() {
    for (fidelity, authored) in [
        ("full", "Say done"),
        ("compact", "Say done"),
        ("compact", ""),
    ] {
        let workflow = format!(
            r#"digraph Segments {{
                graph [backend="api", goal="Keep the nonempty preamble", default_max_retries=0]
                start [shape=Mdiamond]
                task [prompt="{authored}", fidelity="{fidelity}"]
                exit [shape=Msquare]
                start -> task -> exit
            }}"#
        );
        let requests = run_native(&workflow, &["done"]).await;
        assert_eq!(requests.len(), 1);
        let sent = user_text(&requests[0]);
        if fidelity == "full" {
            assert_eq!(sent, authored);
        } else {
            assert!(sent.contains("Keep the nonempty preamble"), "{sent}");
            assert!(sent.contains(authored), "{sent}");
        }
    }
}
