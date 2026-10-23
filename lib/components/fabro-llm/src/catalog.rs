//! Catalog construction and the agent-profile reading that is Fabro's own.
//!
//! Layer order is fixed: lithos built-ins, then the operator's `[llm]`
//! overlay. Which providers are on, which model a selector names, and which
//! model to pick for a job are lithos questions, answered by
//! [`Catalog`] and [`CatalogProvider`] (`enabled_providers`,
//! `offerings_matching`, `default_offering_for`, and the rest). What stays
//! here is the coding harness a model expects, read from the shared
//! `metadata.agent` namespace that Pebble reads too.

use fabro_config::LlmLayer;
use fabro_static::EnvVars;
use fabro_types::AgentProfileKind;
pub use lithos_llm::catalog::Offering;
use lithos_llm::catalog::{
    Catalog, CatalogBuilder, CatalogError, CatalogModel, CatalogProvider, CodecId, Metadata,
    adapter_ids, codec_ids,
};
use serde::Deserialize;
use toml::{Table, Value};

/// The metadata namespace agent harnesses read.
const AGENT_METADATA_NAMESPACE: &str = "agent";

/// Builds the effective catalog.
///
/// `env_lookup` supplies `OPENAI_BASE_URL`, the one environment override
/// Fabro honors: it repoints the `openai` provider so test doubles and
/// gateways can stand in for the real API without editing settings.
pub fn build_catalog(
    overlay: &LlmLayer,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<Catalog, CatalogError> {
    let base_url_overlay = env_lookup(EnvVars::OPENAI_BASE_URL).map(|base_url| {
        LlmLayer::from_providers([(
            "openai".into(),
            Table::from_iter([(
                "base_url".into(),
                Value::String(base_url.trim_end_matches("/v1").to_string()),
            )]),
        )])
    });
    let build_base = || {
        let mut builder = Catalog::builder().with_builtin();
        if !overlay.is_empty() {
            builder = add_catalog_layer(builder, "settings [llm]", overlay)?;
        }
        if let Some(overlay) = &base_url_overlay {
            builder = add_catalog_layer(builder, "OPENAI_BASE_URL", overlay)?;
        }
        Ok::<_, CatalogError>(builder)
    };
    let catalog = build_base()?.build()?;
    // Pebble needs provider-level profile metadata. Fill only missing profiles
    // after the operator overlay has selected the provider's wire protocol.
    let implied = implied_agent_profiles(&catalog);
    if implied.is_empty() {
        return Ok(catalog);
    }
    add_catalog_layer(build_base()?, "implied agent profiles", &implied)?.build()
}

fn add_catalog_layer(
    builder: CatalogBuilder,
    name: &str,
    overlay: &LlmLayer,
) -> Result<CatalogBuilder, CatalogError> {
    // The fixed schema declaration is outside the settings overlay. All
    // dynamic keys and values are serialized together by the TOML library.
    let document = format!("schema_version = 1\n{}", overlay.to_overlay_toml());
    builder.toml_layer(name, &document)
}

/// Metadata for providers without an explicit harness profile.
fn implied_agent_profiles(catalog: &Catalog) -> LlmLayer {
    LlmLayer::from_providers(catalog.providers().filter_map(|provider| {
        if agent_metadata(provider.metadata()).profile.is_some() {
            return None;
        }
        let profile = Value::try_from(adapter_agent_profile(provider))
            .expect("agent profiles serialize as strings");
        let agent = Table::from_iter([("profile".into(), profile)]);
        let metadata = Table::from_iter([("agent".into(), Value::Table(agent))]);
        let fields = Table::from_iter([("metadata".into(), Value::Table(metadata))]);
        Some((provider.id().to_string(), fields))
    }))
}

/// The profile a provider's wire protocol implies, for a provider whose
/// catalog entry does not name one. The protocol is the provider's first
/// codec, the one the client sends a generation call on; the `bedrock`
/// adapter speaks Converse to Anthropic-shaped models.
fn adapter_agent_profile(provider: &CatalogProvider) -> AgentProfileKind {
    if provider.adapter().as_str() == adapter_ids::BEDROCK {
        return AgentProfileKind::Anthropic;
    }
    match provider.codecs().first().map(CodecId::as_str) {
        Some(codec_ids::ANTHROPIC_MESSAGES) => AgentProfileKind::Anthropic,
        Some(codec_ids::GEMINI_GENERATE) => AgentProfileKind::Gemini,
        _ => AgentProfileKind::OpenAi,
    }
}

/// The catalog with no operator overlay: the lithos built-ins.
///
/// Used where no settings file is in play, such as the standalone hook
/// runner. Servers and the CLI build from the operator's `[llm]` overlay
/// with [`build_catalog`] instead.
#[must_use]
pub fn default_catalog() -> Catalog {
    build_catalog(&LlmLayer::default(), &|_| None).expect("the built-in catalog always builds")
}

/// The `metadata.agent` namespace on a catalog entry. Malformed metadata
/// falls back to the defaults; the lithos built-ins are validated in lithos.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct AgentMetadata {
    profile:              Option<AgentProfileKind>,
    reasoning_by_default: Option<bool>,
}

fn agent_metadata(metadata: &Metadata) -> AgentMetadata {
    metadata
        .namespace::<AgentMetadata>(AGENT_METADATA_NAMESPACE)
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// Whether requests to `offering` reason when no effort is requested.
///
/// The catalog can state it outright under `metadata.agent`. Otherwise a
/// model that supports reasoning and takes named effort levels reasons by
/// default, while one that needs an explicit thinking budget does not.
#[must_use]
pub fn reasons_by_default(offering: &Offering<'_>) -> bool {
    agent_metadata(offering.model.metadata())
        .reasoning_by_default
        .or(agent_metadata(offering.provider.metadata()).reasoning_by_default)
        .unwrap_or_else(|| {
            offering.model.capabilities().reasoning().is_supported()
                && offering.model.protocol_options().reasoning_effort_levels
        })
}

fn model_agent_profile(provider: &CatalogProvider, model: &CatalogModel) -> AgentProfileKind {
    agent_metadata(model.metadata())
        .profile
        .unwrap_or_else(|| provider_agent_profile(provider))
}

/// The agent profile a provider's models run under unless a model row says
/// otherwise: the provider's `metadata.agent.profile`, which
/// [`build_catalog`] fills in for a provider that declared none.
fn provider_agent_profile(provider: &CatalogProvider) -> AgentProfileKind {
    agent_metadata(provider.metadata())
        .profile
        .unwrap_or_else(|| adapter_agent_profile(provider))
}

/// The agent profile for a route on an enabled provider. Unknown
/// (passthrough) models take the provider default; a disabled or unknown
/// provider has none.
#[must_use]
pub fn agent_profile(
    catalog: &Catalog,
    provider_selector: &str,
    model_selector: Option<&str>,
) -> Option<AgentProfileKind> {
    let provider = catalog.enabled_provider(provider_selector)?;
    Some(
        match model_selector.and_then(|selector| provider.model(selector)) {
            Some(model) => model_agent_profile(provider, model),
            None => provider_agent_profile(provider),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_catalog;

    #[test]
    fn operator_overlay_applies_last() {
        let overlay = LlmLayer(
            toml::from_str(
                r"
[providers.openai]
priority = 500
enabled = false
",
            )
            .unwrap(),
        );
        let catalog = build_catalog(&overlay, &|_| None).unwrap();
        assert!(catalog.enabled_provider("openai").is_none());
        assert_eq!(
            catalog.provider("openai").unwrap().priority(),
            500,
            "overlay values win over the built-ins"
        );
    }

    #[test]
    fn openai_base_url_env_repoints_the_openai_provider() {
        let catalog = build_catalog(&LlmLayer::default(), &|name| {
            (name == EnvVars::OPENAI_BASE_URL).then(|| "http://127.0.0.1:1234/v1".to_string())
        })
        .unwrap();
        assert_eq!(
            catalog.provider("openai").unwrap().base_url(),
            "http://127.0.0.1:1234"
        );
    }

    #[test]
    fn agent_profiles_follow_the_model_then_the_provider() {
        let catalog = test_catalog();
        assert_eq!(
            agent_profile(&catalog, "openai", Some("gpt-5.6-sol")),
            Some(AgentProfileKind::Gpt56)
        );
        assert_eq!(
            agent_profile(&catalog, "moonshot", None),
            Some(AgentProfileKind::Kimi),
            "a passthrough model on Moonshot takes the provider's Kimi profile"
        );
        assert_eq!(
            agent_profile(&catalog, "deepseek", None),
            Some(AgentProfileKind::OpenAi)
        );
        assert_eq!(
            agent_profile(&catalog, "bedrock-openai", None),
            None,
            "disabled providers have no profile to offer"
        );
        assert_eq!(
            agent_profile(&catalog, "moonshot", Some("kimi-k3")),
            Some(AgentProfileKind::Kimi)
        );
        assert_eq!(
            agent_profile(&catalog, "openai", Some("gpt-6-astra")),
            Some(AgentProfileKind::Gpt6)
        );
        assert_eq!(
            agent_profile(&catalog, "anthropic", Some("claude-sonnet-4.5")),
            Some(AgentProfileKind::Anthropic)
        );
    }

    #[test]
    fn reasoning_by_default_reads_agent_metadata_then_capabilities() {
        let catalog = test_catalog();
        let moonshot = catalog.enabled_provider("moonshot").unwrap();
        let kimi = moonshot.offering("kimi-k2.5").unwrap();
        assert!(reasons_by_default(&kimi), "the catalog row says so");
        let anthropic = catalog.enabled_provider("anthropic").unwrap();
        let sonnet = anthropic.offering("claude-sonnet-4.5").unwrap();
        assert!(
            !reasons_by_default(&sonnet),
            "a thinking-budget model reasons only when asked"
        );
    }

    /// An operator-defined provider with no `metadata.agent.profile` gets the
    /// one its adapter implies, so the coding agent can build on it.
    #[test]
    fn operator_providers_without_a_profile_get_the_adapter_implied_one() {
        let overlay = LlmLayer(
            toml::from_str(
                r#"
[providers.acme]
display_name = "Acme"
base_url = "https://api.acme.test/v1"
auth = { type = "bearer" }
default_model = "acme-llama"

[providers.acme.models.acme-llama]
display_name = "Acme Llama"
api_model = "acme-llama"
limits = { context_tokens = 131072, max_output_tokens = 8192 }
capabilities = { text = true, tools = true }
"#,
            )
            .unwrap(),
        );
        let catalog = build_catalog(&overlay, &|_| None).unwrap();
        let acme = catalog.provider("acme").unwrap();
        assert_eq!(
            agent_metadata(acme.metadata()).profile,
            Some(AgentProfileKind::OpenAi)
        );
        assert_eq!(
            agent_profile(&catalog, "acme", Some("acme-llama")),
            Some(AgentProfileKind::OpenAi)
        );
    }

    #[test]
    fn dotted_provider_profiles_preserve_identity_metadata_and_model_overrides() {
        let source = r#"
[providers."my.gw"]
display_name = "Gateway"
base_url = "http://127.0.0.1:1234/v1"
auth = { type = "bearer" }
[providers."my.gw".metadata.agent]
reasoning_by_default = false
[providers."my.gw".models."model.v1"]
display_name = "Model"
api_model = "wire-model"
[providers."my.gw".models."model.v1".metadata.agent]
profile = "anthropic"
[providers.my]
display_name = "Sibling"
base_url = "http://127.0.0.1:5678"
auth = { type = "none" }
[providers.my.metadata.agent]
profile = "gemini"
"#;
        for enabled in [true, false] {
            let mut overlay = LlmLayer(toml::from_str(source).unwrap());
            overlay.0["providers"]["my.gw"]
                .as_table_mut()
                .unwrap()
                .insert("enabled".into(), Value::Boolean(enabled));
            let catalog = build_catalog(&overlay, &|name| {
                (name == EnvVars::OPENAI_BASE_URL).then(|| "http://127.0.0.1:4321/v1".into())
            })
            .unwrap();
            let gateway = catalog.provider("my.gw").unwrap();
            assert_eq!(gateway.id().as_str(), "my.gw");
            assert_eq!(gateway.base_url(), "http://127.0.0.1:1234/v1");
            assert_eq!(
                agent_metadata(gateway.metadata()).profile,
                Some(AgentProfileKind::OpenAi)
            );
            assert_eq!(
                agent_metadata(gateway.metadata()).reasoning_by_default,
                Some(false)
            );
            assert_eq!(gateway.model("model.v1").unwrap().api_model(), "wire-model");
            assert_eq!(
                model_agent_profile(gateway, gateway.model("model.v1").unwrap()),
                AgentProfileKind::Anthropic
            );
            assert_eq!(
                fabro_auth::expected_secret_name(gateway).as_deref(),
                Some("MY_GW_API_KEY")
            );
            assert_eq!(
                agent_profile(&catalog, "my", None),
                Some(AgentProfileKind::Gemini)
            );
            assert_eq!(catalog.enabled_provider("my.gw").is_some(), enabled);
            assert_eq!(
                catalog.provider("openai").unwrap().base_url(),
                "http://127.0.0.1:4321"
            );
        }
    }

    #[test]
    fn provider_names_remain_literal_with_inferred_and_explicit_profiles() {
        for id in [
            "my.gw", "my.gw.v2", "my-gw", "my_gw", ".gateway", "gateway.",
        ] {
            for explicit in [None, Some("openai"), Some("gemini")] {
                let mut fields: Table = toml::from_str(
                    r#"
display_name = "Gateway"
base_url = "http://127.0.0.1:1234"
auth = { type = "none" }
"#,
                )
                .unwrap();
                if let Some(profile) = explicit {
                    fields.insert(
                        "metadata".into(),
                        Value::Table(Table::from_iter([(
                            "agent".into(),
                            Value::Table(Table::from_iter([(
                                "profile".into(),
                                Value::String(profile.into()),
                            )])),
                        )])),
                    );
                }
                let overlay = LlmLayer::from_providers([(id.into(), fields)]);
                let catalog = build_catalog(&overlay, &|_| None).unwrap();
                assert_eq!(catalog.provider(id).unwrap().id().as_str(), id);
                assert_eq!(
                    agent_profile(&catalog, id, None),
                    Some(if explicit == Some("gemini") {
                        AgentProfileKind::Gemini
                    } else {
                        AgentProfileKind::OpenAi
                    })
                );
                assert!(catalog.provider("my").is_err());
            }
        }
    }

    /// The implied profile follows the provider's first codec, so a host that
    /// speaks Anthropic Messages gets the Anthropic harness.
    #[test]
    fn the_implied_profile_follows_the_first_codec() {
        let overlay = LlmLayer(
            toml::from_str(
                r#"
[providers.acme]
display_name = "Acme"
codecs = ["anthropic-messages"]
base_url = "https://api.acme.test"
auth = { type = "header", name = "x-api-key" }
"#,
            )
            .unwrap(),
        );
        let catalog = build_catalog(&overlay, &|_| None).unwrap();
        assert_eq!(
            agent_profile(&catalog, "acme", None),
            Some(AgentProfileKind::Anthropic)
        );
    }
}
