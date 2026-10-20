//! Semantic validation for immutable workflow versions.
//!
//! The wire type ([`fabro_types::WorkflowVersion`]) enforces structural
//! invariants at construction. This crate owns the expensive semantic
//! validation — graph closure, config, and template checks — behind the
//! [`ValidatedWorkflowVersion`] newtype, and the content-addressed
//! [`WorkflowVersionStore`] that only accepts and returns validated versions.

use std::collections::{BTreeSet, HashMap, VecDeque};

use fabro_config::parse::{SettingsSource, validate_settings_source};
use fabro_config::{
    EnvironmentDockerfileLayer, EnvironmentImageLayer, RunGoalLayer, SettingsLayer,
};
use fabro_dot::{GraphPosition, GraphReferenceKind, WorkflowGraph};
use fabro_template::{
    BundleTemplateStore, StaticReferenceError, TemplateDiscoveryError, TemplateSource,
    discover_static_dependency_closure, validate_static_reference,
};
use fabro_types::settings::InterpString;
use fabro_types::{
    ManifestPath, ReferenceKind, WorkflowPath, WorkflowPathParseError, WorkflowVersion,
};
use thiserror::Error;

mod closure;
mod store;
pub use closure::CollectedWorkflowClosure;
pub use store::{LoadedWorkflowVersionClosure, WorkflowVersionStore, WorkflowVersionStoreError};

#[derive(Debug, Error)]
pub enum WorkflowVersionError {
    #[error("workflow graph `{path}` is invalid")]
    GraphParse {
        path:   WorkflowPath,
        #[source]
        source: Box<fabro_dot::ParseError>,
    },
    #[error("invalid {kind} in `{path}`: `{reference}`")]
    InvalidReference {
        path:      WorkflowPath,
        kind:      ReferenceKind,
        reference: String,
        #[source]
        source:    WorkflowPathParseError,
    },
    #[error("invalid static reference in `{path}`")]
    StaticReference {
        path:   WorkflowPath,
        #[source]
        source: StaticReferenceError,
    },
    #[error("{kind} in `{path}` references missing file `{target}`")]
    MissingFile {
        path:   WorkflowPath,
        kind:   ReferenceKind,
        target: WorkflowPath,
    },
    #[error("template dependencies for `{path}` are invalid")]
    Template {
        path:   WorkflowPath,
        #[source]
        source: Box<TemplateDiscoveryError>,
    },
    #[error("workflow.toml is invalid")]
    Config {
        #[source]
        source: fabro_config::ParseError,
    },
    #[error(
        "workflow.toml selects graph `{configured}`, but the version entrypoint is `{entrypoint}`"
    )]
    ConfigEntrypointMismatch {
        configured: WorkflowPath,
        entrypoint: WorkflowPath,
    },
    #[error("workflow dependencies do not match child workflow references")]
    DependencyMismatch {
        missing: Vec<WorkflowPath>,
        unused:  Vec<WorkflowPath>,
    },
}

/// A workflow version whose graph, config, and template content passed
/// semantic validation.
///
/// This is the only door: functions that require a semantically valid
/// version take this type, and the only way to obtain one is [`Self::new`]
/// (or loading through [`WorkflowVersionStore`], which validates on read).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedWorkflowVersion(WorkflowVersion);

impl ValidatedWorkflowVersion {
    pub fn new(version: WorkflowVersion) -> Result<Self, WorkflowVersionError> {
        let mut template_roots = TemplateRoots::new();
        validate_config(&version, &mut template_roots)?;
        validate_graph_closure(&version, &mut template_roots)?;
        validate_template_closure(&version, template_roots.sources)?;
        Ok(Self(version))
    }

    #[must_use]
    pub fn version(&self) -> &WorkflowVersion {
        &self.0
    }

    #[must_use]
    pub fn into_version(self) -> WorkflowVersion {
        self.0
    }

    /// Content of the run-goal file selected by this version's
    /// `workflow.toml`, when the config declares a file-form goal.
    ///
    /// Resolution reuses the exact grammar [`Self::new`] certified, so
    /// goal-file resolution has one owner: consumers that inline the goal at
    /// run-admission time read the same bytes validation proved present.
    #[must_use]
    pub fn resolved_goal_file_content(&self) -> Option<&str> {
        let config_path = self.0.config_path();
        let source = self.0.files().get(&config_path)?;
        let layer: SettingsLayer = source.parse().expect("validated workflow.toml must parse");
        let RunGoalLayer::File { file } = layer.run.as_ref().and_then(|run| run.goal.as_ref())?
        else {
            return None;
        };
        let (_, content) = validate_config_file_reference(
            &self.0,
            &config_path,
            ReferenceKind::RunGoalFile,
            &unresolved_source(file),
        )
        .expect("validated goal-file reference must resolve");
        Some(content)
    }
}

/// Template sources that anchor static dependency discovery, all rooted at
/// the workflow package root.
struct TemplateRoots {
    package_root: ManifestPath,
    sources:      Vec<TemplateSource>,
}

impl TemplateRoots {
    fn new() -> Self {
        Self {
            package_root: ManifestPath::from_wire(".")
                .expect("the template package root must be a valid manifest path"),
            sources:      Vec::new(),
        }
    }

    fn push(&mut self, path: &WorkflowPath, content: impl Into<String>) {
        self.sources.push(TemplateSource::new(
            manifest_path(path),
            self.package_root.clone(),
            content,
        ));
    }
}

fn validate_config(
    version: &WorkflowVersion,
    template_roots: &mut TemplateRoots,
) -> Result<(), WorkflowVersionError> {
    let config_path = version.config_path();
    let Some(source) = version.files().get(&config_path) else {
        return Ok(());
    };
    let layer = source
        .parse::<SettingsLayer>()
        .map_err(|source| WorkflowVersionError::Config { source })?;
    validate_settings_source(&layer, SettingsSource::Workflow)
        .map_err(|source| WorkflowVersionError::Config { source })?;

    if let Some(configured) = layer
        .workflow
        .as_ref()
        .and_then(|workflow| workflow.graph.as_deref())
    {
        let configured = resolve_reference(&config_path, ReferenceKind::FileInline, configured)?;
        if configured != *version.entrypoint() {
            return Err(WorkflowVersionError::ConfigEntrypointMismatch {
                configured,
                entrypoint: version.entrypoint().clone(),
            });
        }
    }

    for image in layer.environment_images() {
        validate_dockerfile(version, &config_path, image)?;
    }

    // The run engine inlines the effective goal (file contents included) into
    // the entrypoint graph and renders it under the entrypoint's template
    // source, so goal includes anchor at the entrypoint for both goal forms.
    match layer.run.as_ref().and_then(|run| run.goal.as_ref()) {
        Some(RunGoalLayer::Inline(goal)) => {
            template_roots.push(version.entrypoint(), unresolved_source(goal));
        }
        Some(RunGoalLayer::File { file }) => {
            let (_, content) = validate_config_file_reference(
                version,
                &config_path,
                ReferenceKind::RunGoalFile,
                &unresolved_source(file),
            )?;
            template_roots.push(version.entrypoint(), content);
        }
        None => {}
    }
    Ok(())
}

#[expect(
    clippy::disallowed_methods,
    reason = "workflow-version validation preserves authored template source for dependency discovery"
)]
fn unresolved_source(value: &InterpString) -> String {
    value.as_source()
}

fn validate_dockerfile(
    version: &WorkflowVersion,
    config_path: &WorkflowPath,
    image: &EnvironmentImageLayer,
) -> Result<(), WorkflowVersionError> {
    let Some(EnvironmentDockerfileLayer::Path { path }) = image.dockerfile.as_ref() else {
        return Ok(());
    };
    validate_config_file_reference(version, config_path, ReferenceKind::Dockerfile, path)
        .map(|_| ())
}

/// Validate a static file reference in `workflow.toml` and require its target
/// to exist in the version, returning the target path and its content.
fn validate_config_file_reference<'version>(
    version: &'version WorkflowVersion,
    config_path: &WorkflowPath,
    kind: ReferenceKind,
    reference: &str,
) -> Result<(WorkflowPath, &'version str), WorkflowVersionError> {
    validate_static_reference(reference, kind).map_err(|source| {
        WorkflowVersionError::StaticReference {
            path: config_path.clone(),
            source,
        }
    })?;
    let target = resolve_reference(config_path, kind, reference)?;
    let content = require_file(version, config_path, kind, target.clone())?;
    Ok((target, content))
}

fn validate_graph_closure(
    version: &WorkflowVersion,
    template_roots: &mut TemplateRoots,
) -> Result<(), WorkflowVersionError> {
    let mut queue = VecDeque::from([version.entrypoint().clone()]);
    let mut visited = BTreeSet::new();
    let mut child_workflows = BTreeSet::new();

    while let Some(path) = queue.pop_front() {
        if !visited.insert(path.clone()) {
            continue;
        }
        let source =
            version
                .files()
                .get(&path)
                .ok_or_else(|| WorkflowVersionError::MissingFile {
                    path:   path.clone(),
                    kind:   ReferenceKind::Import,
                    target: path.clone(),
                })?;
        let graph = WorkflowGraph::parse(path.as_str(), source).map_err(|source| {
            WorkflowVersionError::GraphParse {
                path:   path.clone(),
                source: Box::new(source),
            }
        })?;
        let position = if &path == version.entrypoint() {
            GraphPosition::Entrypoint
        } else {
            GraphPosition::Imported
        };
        let references =
            graph
                .references(position)
                .map_err(|source| WorkflowVersionError::StaticReference {
                    path: path.clone(),
                    source,
                })?;

        for reference in references {
            match reference.kind {
                GraphReferenceKind::GoalFile { reference } => {
                    let target = resolve_reference(&path, ReferenceKind::GraphGoalFile, reference)?;
                    let content =
                        require_file(version, &path, ReferenceKind::GraphGoalFile, target.clone())?;
                    template_roots.push(&target, content);
                }
                GraphReferenceKind::GoalInline { content }
                | GraphReferenceKind::InlinePrompt { content }
                | GraphReferenceKind::ModelStylesheetInline { content } => {
                    template_roots.push(&path, content);
                }
                GraphReferenceKind::Import { reference } => {
                    let target = resolve_reference(&path, ReferenceKind::Import, reference)?;
                    require_file(version, &path, ReferenceKind::Import, target.clone())?;
                    queue.push_back(target);
                }
                GraphReferenceKind::ChildWorkflow { reference } => {
                    let target = resolve_reference(&path, ReferenceKind::ChildWorkflow, reference)?;
                    child_workflows.insert(target);
                }
                GraphReferenceKind::FileInline { key, reference } => {
                    let target = resolve_reference(&path, ReferenceKind::FileInline, reference)?;
                    let content =
                        require_file(version, &path, ReferenceKind::FileInline, target.clone())?;
                    if key == "prompt" {
                        template_roots.push(&target, content);
                    }
                }
            }
        }
    }

    let configured = version
        .workflow_dependencies()
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    if child_workflows != configured {
        return Err(WorkflowVersionError::DependencyMismatch {
            missing: child_workflows.difference(&configured).cloned().collect(),
            unused:  configured.difference(&child_workflows).cloned().collect(),
        });
    }
    Ok(())
}

fn validate_template_closure(
    version: &WorkflowVersion,
    roots: Vec<TemplateSource>,
) -> Result<(), WorkflowVersionError> {
    discover_static_dependency_closure(roots, &template_store(version)).map_err(|source| {
        WorkflowVersionError::Template {
            path:   template_discovery_path(&source),
            source: Box::new(source),
        }
    })?;
    Ok(())
}

fn template_discovery_path(error: &TemplateDiscoveryError) -> WorkflowPath {
    WorkflowPath::new(error.source_path().to_string())
        .expect("template paths sourced from a workflow version must be valid")
}

fn template_store(version: &WorkflowVersion) -> BundleTemplateStore {
    BundleTemplateStore::new(
        version
            .files()
            .iter()
            .map(|(path, content)| (manifest_path(path), content.clone()))
            .collect::<HashMap<_, _>>(),
    )
}

fn resolve_reference(
    path: &WorkflowPath,
    kind: ReferenceKind,
    reference: &str,
) -> Result<WorkflowPath, WorkflowVersionError> {
    path.resolve_reference(reference)
        .map_err(|source| WorkflowVersionError::InvalidReference {
            path: path.clone(),
            kind,
            reference: reference.to_owned(),
            source,
        })
}

fn require_file<'version>(
    version: &'version WorkflowVersion,
    path: &WorkflowPath,
    kind: ReferenceKind,
    target: WorkflowPath,
) -> Result<&'version str, WorkflowVersionError> {
    version
        .files()
        .get(&target)
        .map(String::as_str)
        .ok_or_else(|| WorkflowVersionError::MissingFile {
            path: path.clone(),
            kind,
            target,
        })
}

fn manifest_path(path: &WorkflowPath) -> ManifestPath {
    ManifestPath::from_wire(path.as_str())
        .expect("validated workflow paths must also be valid manifest paths")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use fabro_template::{TemplateDiscoveryError, TemplateLoadError};
    use fabro_types::{BlobHash, ReferenceKind, WorkflowPath, WorkflowVersion, WorkflowVersionId};

    use super::{ValidatedWorkflowVersion, WorkflowVersionError};

    fn path(value: &str) -> WorkflowPath {
        value.parse().unwrap()
    }

    fn dependency_id(value: &[u8]) -> WorkflowVersionId {
        BlobHash::new(value).into()
    }

    fn version_with(
        files: impl IntoIterator<Item = (&'static str, &'static str)>,
        dependencies: impl IntoIterator<Item = (&'static str, WorkflowVersionId)>,
    ) -> Result<ValidatedWorkflowVersion, WorkflowVersionError> {
        ValidatedWorkflowVersion::new(
            WorkflowVersion::new(
                path("workflow.fabro"),
                files
                    .into_iter()
                    .map(|(path_value, content)| (path(path_value), content.to_owned()))
                    .collect(),
                dependencies
                    .into_iter()
                    .map(|(path_value, id)| (path(path_value), id))
                    .collect(),
            )
            .expect("test fixtures must be structurally valid"),
        )
    }

    fn version_with_config(
        config: impl Into<String>,
        extra_files: impl IntoIterator<Item = (&'static str, &'static str)>,
    ) -> Result<ValidatedWorkflowVersion, WorkflowVersionError> {
        let mut files = extra_files
            .into_iter()
            .map(|(path_value, content)| (path(path_value), content.to_owned()))
            .collect::<BTreeMap<_, _>>();
        files.insert(path("workflow.fabro"), "digraph W {}".to_owned());
        files.insert(path("workflow.toml"), config.into());
        ValidatedWorkflowVersion::new(
            WorkflowVersion::new(path("workflow.fabro"), files, BTreeMap::default())
                .expect("test fixtures must be structurally valid"),
        )
    }

    fn hook_documentation_toml(markdown: &str) -> Vec<&str> {
        let snippets: Vec<_> = markdown
            .split("```toml")
            .skip(1)
            .map(|fence| {
                let (_, body) = fence.split_once('\n').expect("TOML fence has a body");
                body.split_once("```").expect("TOML fence is closed").0
            })
            .collect();
        assert!(
            !snippets.is_empty(),
            "hook documentation must contain TOML examples"
        );
        snippets
    }

    #[test]
    fn hook_documentation_examples_register_and_resolve() {
        use std::collections::{BTreeMap, BTreeSet};

        use fabro_config::{RunLayer, SettingsLayer, WorkflowSettingsBuilder};
        use fabro_types::settings::InterpString;
        use fabro_types::settings::run::HookType;

        let guide = include_str!("../../../../docs/public/agents/hooks.mdx");
        let reference = include_str!("../../../../docs/public/execution/run-configuration.mdx");
        let reference = reference
            .split_once("### `[[run.hooks]]`")
            .expect("hook reference section exists")
            .1
            .split_once("\n## ")
            .expect("hook reference section has an end")
            .0;
        let catalog: SettingsLayer = "[environments.default]\nprovider = \"local\"\n"
            .parse()
            .unwrap();
        let mut transports = BTreeMap::<_, usize>::new();
        let mut timeouts = BTreeSet::new();
        let mut saw_argv = false;
        let mut saw_headers = false;
        for (document, markdown) in [("guide", guide), ("reference", reference)] {
            for (index, snippet) in hook_documentation_toml(markdown).into_iter().enumerate() {
                version_with_config(snippet, []).unwrap_or_else(|error| {
                    panic!(
                        "{document} TOML example {} must register: {error:?}",
                        index + 1
                    )
                });
                let layer: SettingsLayer = snippet.parse().unwrap();
                let entries = &layer.run.as_ref().expect("example has run settings").hooks;
                assert!(!entries.is_empty(), "example must define hooks");
                let settings = WorkflowSettingsBuilder::new()
                    .server_manifest_defaults(RunLayer::default(), catalog.environments.clone())
                    .workflow_toml(snippet)
                    .unwrap()
                    .build()
                    .unwrap_or_else(|error| {
                        panic!(
                            "{document} TOML example {} must resolve: {error:?}",
                            index + 1
                        )
                    });
                assert_eq!(settings.run.hooks.len(), entries.len());
                for (entry, hook) in entries.iter().zip(&settings.run.hooks) {
                    match hook
                        .resolved_hook_type()
                        .as_deref()
                        .expect("hook has a transport")
                    {
                        HookType::Command { command } => {
                            *transports.entry("command").or_default() += 1;
                            if let Some(script) = &entry.script {
                                assert_eq!(command, script);
                            } else {
                                assert_eq!(entry.command.as_ref().unwrap().len(), 2);
                                assert_eq!(command, &InterpString::parse("cargo fmt"));
                                saw_argv = true;
                            }
                        }
                        HookType::Http { url, headers, .. } => {
                            *transports.entry("http").or_default() += 1;
                            assert_eq!(Some(url), entry.url.as_ref());
                            if !entry.headers.is_empty() {
                                assert_eq!(headers.as_ref().unwrap(), &entry.headers);
                                saw_headers = true;
                            }
                        }
                        HookType::Prompt { prompt, .. } => {
                            *transports.entry("prompt").or_default() += 1;
                            assert!(entry.agent.is_none());
                            assert_eq!(Some(prompt), entry.prompt.as_ref());
                        }
                        HookType::Agent {
                            prompt,
                            max_tool_rounds,
                            ..
                        } => {
                            *transports.entry("agent").or_default() += 1;
                            assert!(entry.agent.is_some());
                            assert_eq!(Some(prompt), entry.prompt.as_ref());
                            assert_eq!(max_tool_rounds, &entry.max_tool_rounds);
                        }
                    }
                    if let Some(timeout) = hook.timeout_ms {
                        timeouts.insert(timeout);
                    }
                }
            }
        }
        assert_eq!(
            transports,
            [("agent", 2), ("command", 9), ("http", 2), ("prompt", 3)].into()
        );
        assert_eq!(timeouts, [15_000, 30_000, 120_000].into());
        assert!(saw_argv, "guide must demonstrate vector-valued command");
        assert!(saw_headers, "guide must demonstrate nested HTTP headers");
    }

    #[test]
    fn hook_documentation_obsolete_fields_remain_rejected() {
        for source in [
            "[[hooks]]\nevent = \"stage_start\"\nscript = \"true\"\n",
            "[[run.hooks]]\nevent = \"stage_start\"\ntype = \"prompt\"\nprompt = \"Check\"\n",
            "[[run.hooks]]\nevent = \"run_complete\"\ntype = \"http\"\nurl = \"https://example.com\"\n",
            "[[run.hooks]]\nevent = \"stage_start\"\ncommand = \"true\"\n",
            "[[run.hooks]]\nevent = \"stage_start\"\nscript = \"true\"\ntimeout_ms = 30000\n",
        ] {
            assert!(matches!(
                version_with_config(source, []),
                Err(WorkflowVersionError::Config { .. })
            ));
        }
    }

    fn version_with_goal_file(
        reference: &str,
    ) -> Result<ValidatedWorkflowVersion, WorkflowVersionError> {
        let reference = serde_json::to_string(reference).unwrap();
        let config = format!("_version = 1\n[run.goal]\nfile = {reference}\n");
        version_with_config(config, [])
    }

    fn version_with_inline_goal(
        goal: &str,
        extra_files: impl IntoIterator<Item = (&'static str, &'static str)>,
    ) -> Result<ValidatedWorkflowVersion, WorkflowVersionError> {
        let goal = serde_json::to_string(goal).unwrap();
        version_with_config(format!("_version = 1\n[run]\ngoal = {goal}\n"), extra_files)
    }

    #[test]
    fn validates_imports_templates_file_refs_and_dependencies() {
        let version = version_with(
            [
                (
                    "workflow.fabro",
                    r#"digraph W {
                        graph [goal="@prompts/goal.md"]
                        start [shape=Mdiamond]
                        imported [import="graphs/imported.fabro"]
                        child [stack.child_workflow="children/check.fabro"]
                        exit [shape=Msquare]
                        start -> imported -> child -> exit
                    }"#,
                ),
                (
                    "graphs/imported.fabro",
                    r#"digraph I { step [prompt="{% include \"../prompts/partial.md\" %}"] }"#,
                ),
                ("prompts/goal.md", "{% include \"partial.md\" %}"),
                ("prompts/partial.md", "Do the work"),
            ],
            [("children/check.fabro", dependency_id(b"child"))],
        )
        .unwrap();

        assert_eq!(version.version().workflow_dependencies().len(), 1);
    }

    #[test]
    fn rejects_missing_and_unused_dependencies() {
        let error = version_with(
            [(
                "workflow.fabro",
                r#"digraph W { child [stack.child_workflow="child.fabro"] }"#,
            )],
            [("unused.fabro", dependency_id(b"unused"))],
        )
        .unwrap_err();

        let WorkflowVersionError::DependencyMismatch { missing, unused } = error else {
            panic!("expected dependency mismatch");
        };
        assert_eq!(missing, vec![path("child.fabro")]);
        assert_eq!(unused, vec![path("unused.fabro")]);
    }

    #[test]
    fn rejects_config_entrypoint_and_missing_dockerfile() {
        let error = version_with(
            [
                (
                    "workflow.fabro",
                    "digraph W { start [shape=Mdiamond] exit [shape=Msquare] start -> exit }",
                ),
                (
                    "workflow.toml",
                    "_version = 1\n[workflow]\ngraph = \"other.fabro\"\n",
                ),
            ],
            [],
        )
        .unwrap_err();
        assert!(matches!(
            error,
            WorkflowVersionError::ConfigEntrypointMismatch { .. }
        ));

        let missing_dockerfile = version_with(
            [
                ("workflow.fabro", "digraph W {}"),
                (
                    "workflow.toml",
                    "_version = 1\n[run.environment.image]\ndockerfile = { path = \"docker/Dockerfile\" }\n",
                ),
            ],
            [],
        )
        .unwrap_err();
        assert!(matches!(
            missing_dockerfile,
            WorkflowVersionError::MissingFile { .. }
        ));

        let invalid_config = version_with(
            [
                ("workflow.fabro", "digraph W {}"),
                ("workflow.toml", "not valid toml = ["),
            ],
            [],
        )
        .unwrap_err();
        assert!(matches!(
            invalid_config,
            WorkflowVersionError::Config { .. }
        ));
    }

    #[test]
    fn rejects_missing_workflow_goal_file() {
        let error = version_with_goal_file("prompts/goal.md").unwrap_err();

        assert!(matches!(
            error,
            WorkflowVersionError::MissingFile {
                path: source_path,
                kind,
                target,
            }
                if source_path == path("workflow.toml")
                    && kind == ReferenceKind::RunGoalFile
                    && target == path("prompts/goal.md")
        ));
    }

    #[test]
    fn accepts_inline_workflow_goal_with_static_template_closure() {
        let version = version_with_inline_goal(
            r#"Review {{ vars.target }} with {{ inputs.mode }} after {{ goal }}. {% include "prompts/shared.md" %}"#,
            [("prompts/shared.md", "Use {{ vars.detail }}")],
        )
        .unwrap();

        assert_eq!(version.version().files().len(), 3);
    }

    #[test]
    fn accepts_file_workflow_goal_with_transitive_template_closure() {
        // The goal file's own includes anchor at the entrypoint's directory
        // (the package root here), not at the goal file's directory; loaded
        // dependencies then anchor at their own directories as usual.
        let version =
            version_with_config("_version = 1\n[run.goal]\nfile = \"prompts/goal.md\"\n", [
                ("prompts/goal.md", r#"{% include "prompts/partial.md" %}"#),
                ("prompts/partial.md", r#"{% include "nested/detail.md" %}"#),
                ("prompts/nested/detail.md", "Use {{ vars.detail }}"),
            ])
            .unwrap();

        assert_eq!(version.version().files().len(), 5);
    }

    #[test]
    fn rejects_broken_transitive_includes_under_a_workflow_goal_file() {
        // Guards the root push for file goals: without it the goal file is
        // never parsed and the broken include below is silently accepted.
        let error =
            version_with_config("_version = 1\n[run.goal]\nfile = \"prompts/goal.md\"\n", [
                ("prompts/goal.md", r#"{% include "prompts/partial.md" %}"#),
                ("prompts/partial.md", r#"{% include "missing.md" %}"#),
            ])
            .unwrap_err();

        assert!(matches!(
            error,
            WorkflowVersionError::Template { path: source_path, source }
                if source_path == path("prompts/partial.md")
                    && matches!(
                        source.as_ref(),
                        TemplateDiscoveryError::Missing { reference, .. } if reference == "missing.md"
                    )
        ));
    }

    #[test]
    fn anchors_workflow_goal_includes_at_the_entrypoint() {
        let version_with_entrypoint = |goal_include_target: &'static str| {
            ValidatedWorkflowVersion::new(
                WorkflowVersion::new(
                    path("graphs/main.fabro"),
                    BTreeMap::from([
                        (path("graphs/main.fabro"), "digraph W {}".to_owned()),
                        (
                            path("graphs/workflow.toml"),
                            "_version = 1\n[run]\ngoal = \"{% include \\\"shared.md\\\" %}\"\n"
                                .to_owned(),
                        ),
                        (path(goal_include_target), "shared".to_owned()),
                    ]),
                    BTreeMap::default(),
                )
                .expect("test fixtures must be structurally valid"),
            )
        };

        // The include resolves beside the entrypoint graph, matching where
        // the run engine renders the inlined goal.
        version_with_entrypoint("graphs/shared.md").unwrap();

        let error = version_with_entrypoint("shared.md").unwrap_err();
        assert!(matches!(
            error,
            WorkflowVersionError::Template { path: source_path, source }
                if source_path == path("graphs/main.fabro")
                    && matches!(
                        source.as_ref(),
                        TemplateDiscoveryError::Missing { reference, .. } if reference == "shared.md"
                    )
        ));
    }

    #[test]
    fn discovers_workflow_config_beside_a_nested_entrypoint() {
        let version = ValidatedWorkflowVersion::new(
            WorkflowVersion::new(
                path(".fabro/workflows/demo/workflow.fabro"),
                BTreeMap::from([
                    (
                        path(".fabro/workflows/demo/workflow.fabro"),
                        "digraph W {}".to_owned(),
                    ),
                    (
                        path(".fabro/workflows/demo/workflow.toml"),
                        "_version = 1\n[workflow]\ngraph = \"workflow.fabro\"\n[run.goal]\nfile = \"goal.md\"\n"
                            .to_owned(),
                    ),
                    (
                        path(".fabro/workflows/demo/goal.md"),
                        "Ship the nested workflow".to_owned(),
                    ),
                ]),
                BTreeMap::new(),
            )
            .expect("nested workflow version should be structurally valid"),
        )
        .expect("entrypoint-adjacent workflow config should validate");

        assert_eq!(
            version.resolved_goal_file_content(),
            Some("Ship the nested workflow")
        );
    }

    #[test]
    fn rejects_non_static_or_nonportable_workflow_goal_file_references() {
        for reference in ["{{ vars.NAME }}", "{% include \"goal.md\" %}"] {
            let error = version_with_goal_file(reference).unwrap_err();
            let WorkflowVersionError::StaticReference {
                path: source_path,
                source,
            } = error
            else {
                panic!("expected static-reference error for {reference:?}");
            };
            assert_eq!(source_path, path("workflow.toml"));
            assert_eq!(source.kind(), ReferenceKind::RunGoalFile);
        }

        for reference in [
            "",
            "/absolute.md",
            "../outside.md",
            "~/goal.md",
            "C:/goal.md",
            "prompts\\goal.md",
            "prompts//goal.md",
            "prompts/",
            "prompts/goal\n.md",
        ] {
            let error = version_with_goal_file(reference).unwrap_err();
            assert!(
                matches!(
                    &error,
                    WorkflowVersionError::InvalidReference {
                        path: source_path,
                        kind: ReferenceKind::RunGoalFile,
                        ..
                    } if *source_path == path("workflow.toml")
                ),
                "expected invalid-reference error for {reference:?}, got {error:?}"
            );
        }
    }

    #[test]
    fn rejects_invalid_workflow_goal_template_closure() {
        let missing = version_with_inline_goal(r#"{% include "missing.md" %}"#, []).unwrap_err();
        let WorkflowVersionError::Template {
            path: source_path,
            source,
        } = missing
        else {
            panic!("expected missing template dependency");
        };
        assert_eq!(source_path, path("workflow.fabro"));
        assert!(matches!(
            source.as_ref(),
            TemplateDiscoveryError::Missing { parent, reference }
                if parent.to_string() == "workflow.fabro" && reference == "missing.md"
        ));

        let dynamic = version_with_inline_goal(r"{% include inputs.partial %}", []).unwrap_err();
        let WorkflowVersionError::Template { source, .. } = dynamic else {
            panic!("expected dynamic template dependency");
        };
        assert!(matches!(
            source.as_ref(),
            TemplateDiscoveryError::Dynamic { parent }
                if parent.to_string() == "workflow.fabro"
        ));

        let escaping =
            version_with_inline_goal(r#"{% include "../outside.md" %}"#, []).unwrap_err();
        let WorkflowVersionError::Template { source, .. } = escaping else {
            panic!("expected escaping template dependency");
        };
        assert!(matches!(
            source.as_ref(),
            TemplateDiscoveryError::Load {
                source: TemplateLoadError::EscapesRoot { parent, .. },
                ..
            } if parent.to_string() == "workflow.fabro"
        ));
    }

    #[test]
    fn validates_root_model_stylesheet_template_closure() {
        let version = version_with(
            [
                (
                    "workflow.fabro",
                    r#"digraph W {
                        graph [model_stylesheet="{% include 'styles/base.css' %}"]
                    }"#,
                ),
                ("styles/base.css", "{% include 'nested.css' %}"),
                ("styles/nested.css", "* { reasoning_effort: low; }"),
            ],
            [],
        )
        .unwrap();

        assert_eq!(version.version().files().len(), 3);

        for template in [
            "{% include 'missing.css' %}",
            "{% include inputs.stylesheet %}",
            "{% include '../outside.css' %}",
        ] {
            let graph = format!(r#"digraph W {{ graph [model_stylesheet="{template}"] }}"#);
            let error = ValidatedWorkflowVersion::new(
                WorkflowVersion::new(
                    path("workflow.fabro"),
                    BTreeMap::from([(path("workflow.fabro"), graph)]),
                    BTreeMap::default(),
                )
                .unwrap(),
            )
            .unwrap_err();
            assert!(
                matches!(error, WorkflowVersionError::Template { .. }),
                "template: {template}; error: {error:?}"
            );
        }
    }

    #[test]
    fn ignores_imported_model_stylesheet_template_closure() {
        let version = version_with(
            [
                (
                    "workflow.fabro",
                    r#"digraph W { imported [import="child.fabro"] }"#,
                ),
                (
                    "child.fabro",
                    r#"digraph I {
                        graph [model_stylesheet="{% include 'missing.css' %}"]
                    }"#,
                ),
            ],
            [],
        )
        .unwrap();

        assert_eq!(version.version().files().len(), 2);
    }

    #[test]
    fn validates_all_inline_graph_roots_that_share_the_graph_path() {
        let error = version_with(
            [(
                "workflow.fabro",
                r#"digraph W {
                    graph [goal="valid"]
                    step [prompt="{% include inputs.partial %}"]
                }"#,
            )],
            [],
        )
        .unwrap_err();

        assert!(matches!(
            error,
            WorkflowVersionError::Template {
                source,
                ..
            } if matches!(source.as_ref(), TemplateDiscoveryError::Dynamic { .. })
        ));
    }

    #[test]
    fn validates_graph_files_included_from_goal_templates() {
        // The graph file's inline prompt anchors a template root at the graph
        // path; that root must not shadow the raw graph content when a goal
        // template includes the graph file itself.
        let error = version_with(
            [
                (
                    "workflow.fabro",
                    r#"digraph W {
                        graph [goal="@goal.md"]
                        step [prompt="hello", note="{% include 'missing.md' %}"]
                    }"#,
                ),
                ("goal.md", r#"{% include "workflow.fabro" %}"#),
            ],
            [],
        )
        .unwrap_err();

        assert!(matches!(
            error,
            WorkflowVersionError::Template { path: source_path, source }
                if source_path == path("workflow.fabro")
                    && matches!(
                        source.as_ref(),
                        TemplateDiscoveryError::Missing { reference, .. } if reference == "missing.md"
                    )
        ));
    }

    #[test]
    fn accepts_root_config_and_all_dockerfile_path_sources() {
        let version = version_with(
            [
                ("workflow.fabro", "digraph W {}"),
                (
                    "workflow.toml",
                    r#"_version = 1
[workflow]
graph = "workflow.fabro"

[environments.cloud]
provider = "daytona"

[environments.cloud.image]
dockerfile = { path = "docker/named.Dockerfile" }

[run.environment.image]
dockerfile = { path = "docker/run.Dockerfile" }
"#,
                ),
                ("docker/named.Dockerfile", "FROM alpine\n"),
                ("docker/run.Dockerfile", "FROM ubuntu\n"),
            ],
            [],
        )
        .unwrap();

        assert_eq!(version.version().entrypoint(), &path("workflow.fabro"));
    }

    #[test]
    fn rejects_server_managed_environment_cwd_in_workflow_config() {
        let error = version_with(
            [
                ("workflow.fabro", "digraph W {}"),
                (
                    "workflow.toml",
                    "_version = 1\n[environments.local]\nprovider = \"local\"\ncwd = \"/tmp\"\n",
                ),
            ],
            [],
        )
        .unwrap_err();

        assert!(matches!(error, WorkflowVersionError::Config { .. }));
        assert!(error.to_string().contains("workflow.toml is invalid"));
    }

    #[test]
    fn rejects_escaping_and_dynamic_template_references() {
        let escaping = version_with(
            [(
                "workflow.fabro",
                r#"digraph W { imported [import="../outside.fabro"] }"#,
            )],
            [],
        )
        .unwrap_err();
        assert!(matches!(
            escaping,
            WorkflowVersionError::InvalidReference { .. }
        ));

        let dynamic = version_with(
            [(
                "workflow.fabro",
                r#"digraph W { step [prompt="{% include template_name %}"] }"#,
            )],
            [],
        )
        .unwrap_err();
        assert!(matches!(dynamic, WorkflowVersionError::Template { .. }));
    }
}
