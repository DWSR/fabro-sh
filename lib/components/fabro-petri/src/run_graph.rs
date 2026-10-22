//! The display graph of a run, read off what Petri admitted.
//!
//! Petri's lowered graph carries the frontend's metadata on every node
//! (`meta`: `label`, `kind`, `edges`, `synthetic`, see the Fabro handoff's
//! "Identities a host can rely on") and the run's goal and workflow name as
//! graph params. [`run_graph`] reduces that to the [`RunGraph`] the read side
//! stores on the run spec: one node per stage the workflow declares, each
//! with its label and handler kind, and one edge per routing arm as written.
//!
//! Lowering artifacts are left out: the goal check before `exit`, the
//! synthetic fan-in before a plain join, and a duplicate branch target's
//! extra delegate. A parallel branch target stays a stage of the graph: in
//! the parent graph it is the branch delegate (`kind = parallel.branch`,
//! `synthetic: true`, `branch = {fork, target}`), so its own kind is read
//! from the child graph the branch runs, where the target keeps its
//! metadata, and the parallel node's edge to it is the delegate's `branch`.

use std::str::FromStr;

use fabro_types::{RunGraph, RunGraphEdge, RunGraphNode, StageHandler};
use petri_runtime::ir::Graph;
use serde_json::Value;

use crate::check::Admitted;

/// The graph param the Attractor lowering stores the workflow's name under.
const WORKFLOW_PARAM: &str = "attractor.workflow";
/// The graph param the run's goal is stored under.
const GOAL_PARAM: &str = "goal";

/// The display graph of the admitted workflow.
#[must_use]
pub fn run_graph(admitted: &Admitted) -> RunGraph {
    let root = &admitted.graph;
    let mut graph = RunGraph::new(param_text(root, WORKFLOW_PARAM));
    graph.goal = param_text(root, GOAL_PARAM).to_string();
    for node in &root.body.nodes {
        let meta = &node.meta;
        let name = node.name.as_str();
        let kind = if is_synthetic(meta) {
            // A branch delegate stands for the parallel node's edge to its
            // target, and for the target itself when it is named after it;
            // any other synthetic node is a lowering artifact.
            let Some((fork, target)) = branch_of(meta) else {
                continue;
            };
            graph.edges.push(RunGraphEdge {
                from: fork.to_string(),
                to:   target.to_string(),
            });
            if target != name {
                continue;
            }
            admitted
                .children
                .iter()
                .flat_map(|child| &child.body.nodes)
                .find(|candidate| candidate.name == target && !is_synthetic(&candidate.meta))
                .map_or(StageHandler::Agent, |candidate| kind_of(&candidate.meta))
        } else {
            kind_of(meta)
        };
        let label = meta
            .get("label")
            .and_then(Value::as_str)
            .filter(|label| !label.is_empty())
            .unwrap_or(name);
        graph.nodes.insert(name.to_string(), RunGraphNode {
            label: label.to_string(),
            kind,
        });
        if let Some(edges) = meta.get("edges").and_then(Value::as_object) {
            for entry in edges.values() {
                if let Some(to) = entry.get("to").and_then(Value::as_str) {
                    graph.edges.push(RunGraphEdge {
                        from: name.to_string(),
                        to:   to.to_string(),
                    });
                }
            }
        }
    }
    // The routing arms are keyed by engine edge id in the metadata; order
    // them by the nodes they join so the graph reads the same on every
    // build.
    graph
        .edges
        .sort_by(|left, right| (&left.from, &left.to).cmp(&(&right.from, &right.to)));
    graph
}

fn param_text<'a>(graph: &'a Graph, name: &str) -> &'a str {
    graph
        .params
        .get(name)
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn is_synthetic(meta: &Value) -> bool {
    meta.get("synthetic").and_then(Value::as_bool) == Some(true)
}

/// The parallel node and the target a branch delegate stands for.
fn branch_of(meta: &Value) -> Option<(&str, &str)> {
    let branch = meta.get("branch")?;
    Some((
        branch.get("fork")?.as_str()?,
        branch.get("target")?.as_str()?,
    ))
}

/// The node's handler kind from its `meta.kind`: the Attractor names are the
/// stage handler names, and anything else runs as an agent, as Fabro's
/// handler resolution has it.
fn kind_of(meta: &Value) -> StageHandler {
    let kind = meta.get("kind").and_then(Value::as_str);
    kind.and_then(|kind| StageHandler::from_str(kind).ok())
        .unwrap_or_else(|| StageHandler::from_handler_type(kind))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::check::{self, Bundle, CheckRequest};

    fn admit(files: &[(&str, &str)]) -> Admitted {
        let request = CheckRequest {
            bundle: Bundle {
                files:        files
                    .iter()
                    .map(|(path, text)| ((*path).to_string(), (*text).to_string()))
                    .collect::<BTreeMap<_, _>>(),
                entrypoint:   "workflow.fabro".to_string(),
                project_toml: None,
            },
            ..CheckRequest::default()
        };
        check::check(&request).unwrap_or_else(|err| panic!("the workflow should admit: {err:?}"))
    }

    fn kinds(graph: &RunGraph) -> Vec<(&str, StageHandler)> {
        graph
            .nodes
            .iter()
            .map(|(id, node)| (id.as_str(), node.kind))
            .collect()
    }

    fn edges(graph: &RunGraph) -> Vec<(&str, &str)> {
        graph
            .edges
            .iter()
            .map(|edge| (edge.from.as_str(), edge.to.as_str()))
            .collect()
    }

    #[test]
    fn a_linear_workflow_keeps_its_name_goal_stages_and_edges() {
        let admitted = admit(&[(
            "workflow.fabro",
            r#"digraph Ship {
                graph [goal="Ship the feature"]
                start [shape=Mdiamond, label="Start"]
                plan [label="Plan rollout", prompt="Plan"]
                deploy [shape=parallelogram, script="make deploy"]
                gate [shape=hexagon, label="Ship?"]
                exit [shape=Msquare]
                start -> plan -> deploy -> gate
                gate -> exit [label="[Y] Yes"]
                gate -> plan [label="[N] No"]
            }"#,
        )]);

        let graph = run_graph(&admitted);

        assert_eq!(graph.name, "Ship");
        assert_eq!(graph.goal(), "Ship the feature");
        assert_eq!(kinds(&graph), vec![
            ("deploy", StageHandler::Command),
            ("exit", StageHandler::Exit),
            ("gate", StageHandler::Human),
            ("plan", StageHandler::Agent),
            ("start", StageHandler::Start),
        ]);
        assert_eq!(graph.node("plan").unwrap().label, "Plan rollout");
        assert_eq!(graph.node("deploy").unwrap().label, "deploy");
        assert_eq!(edges(&graph), vec![
            ("deploy", "gate"),
            ("gate", "exit"),
            ("gate", "plan"),
            ("plan", "deploy"),
            ("start", "plan"),
        ]);
        assert!(graph.is_boundary("start"));
        assert!(graph.is_boundary("exit"));
    }

    #[test]
    fn lowering_artifacts_are_left_out_and_branch_targets_keep_their_kind() {
        let admitted = admit(&[(
            "workflow.fabro",
            r#"digraph Parallel {
                graph [goal="Fan out"]
                start [shape=Mdiamond]
                fan_out [shape=component]
                lint [shape=parallelogram, script="make lint"]
                test [prompt="Run the tests", goal_gate=true, retry_target="lint"]
                join [shape=tripleoctagon]
                exit [shape=Msquare]
                start -> fan_out
                fan_out -> lint
                fan_out -> test
                lint -> join
                test -> join
                join -> exit
            }"#,
        )]);

        let graph = run_graph(&admitted);

        assert_eq!(kinds(&graph), vec![
            ("exit", StageHandler::Exit),
            ("fan_out", StageHandler::Parallel),
            ("join", StageHandler::ParallelFanIn),
            ("lint", StageHandler::Command),
            ("start", StageHandler::Start),
            ("test", StageHandler::Agent),
        ]);
        assert_eq!(edges(&graph), vec![
            ("fan_out", "lint"),
            ("fan_out", "test"),
            ("join", "exit"),
            ("lint", "join"),
            ("start", "fan_out"),
            ("test", "join"),
        ]);
    }

    #[test]
    fn explicit_setup_commands_are_stages_of_the_graph_they_run_in() {
        let admitted = admit(&[
            (
                "workflow.fabro",
                r#"digraph Prepared {
                    start [shape=Mdiamond]
                    setup [shape=parallelogram, script="make setup", timeout="5m", on_failure="exit"]
                    work [prompt="Work"]
                    exit [shape=Msquare]
                    start -> setup -> work -> exit
                }"#,
            ),
            (
                "workflow.toml",
                "_version = 1\n[run]\ngoal = \"Settings goal\"\n",
            ),
        ]);

        let graph = run_graph(&admitted);

        assert_eq!(graph.goal(), "Settings goal");
        assert_eq!(
            graph.node("setup").map(|node| node.kind),
            Some(StageHandler::Command)
        );
        assert_eq!(edges(&graph), vec![
            ("setup", "work"),
            ("start", "setup"),
            ("work", "exit"),
        ]);
    }
}
