//! Model selection over ACP.
//!
//! A seat can pin the model its harness runs ([`crate::home::AgentSlotConfig::model`]). ACP has no
//! way to state a model when the session is created — `session/new` params are `cwd` + `mcpServers`
//! only — so a pinned model is applied to the session **after** it exists and **before** the first
//! prompt.
//!
//! Two dialects are in the field, and which one an adapter speaks is a property of the adapter
//! BUILD rather than the vendor, so it is read off that adapter's own `session/new` response
//! instead of a table keyed by name:
//!
//! - **config option** (preferred) — the response carries a `configOptions` entry in the `model`
//!   category; the model is set with `session/set_config_option`, whose response returns every
//!   option with its current value.
//! - **legacy models** — the response carries a `models` object; the model is set with
//!   `session/set_model`.
//!
//! ★ Only the config-option dialect can be VERIFIED. `session/set_model` answers `{}` on every
//! adapter measured (codex-acp 1.1.2, cursor-agent 2026.07.09), so there is nothing to read back:
//! the legacy path can only be checked before the fact. That asymmetry is why the config-option
//! dialect is always preferred when an adapter advertises both.
//!
//! ★ The offered list is the authority on what a model name may be, and a pinned model is
//! validated against it BEFORE anything is sent. This is not belt-and-braces: `claude-agent-acp`
//! 0.45.1 resolves an unrecognised value through an alias matcher and, measured against the real
//! adapter, **accepts a garbage model without error and reports the session as `default`**. An
//! adapter's acceptance is therefore not evidence that a model was honoured, and neither is the
//! mere presence of a model in the read-back. Sending only a value the adapter itself advertised
//! makes the read-back an exact comparison — no alias table to keep, and no fuzzy match to be
//! silently redirected by.
//!
//! Exact-or-nothing, never nearest-match, mirrors the harness rule in
//! [`crate::seller_agents`]: quietly running something other than what was asked for is the one
//! outcome this module exists to prevent.

use serde_json::{Value, json};

use crate::driver::{DriverError, SessionId};

/// The `configOptions` category that marks the model selector. Optional and UX-only per the ACP
/// schema, so [`ModelSupport::read`] falls back to the conventional id.
const MODEL_CATEGORY: &str = "model";
/// The conventional `configOptions` id for the model selector — `claude-agent-acp`, `codex-acp` and
/// `cursor-agent` all use it. Only consulted when no option declares the category.
const MODEL_CONFIG_ID: &str = "model";

/// How a harness accepts a model, as advertised by its own `session/new` response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelSupport {
    /// `session/set_config_option`. The verifiable dialect: the set response echoes current values.
    ConfigOption {
        /// The advertised option id to address (`configId`), never assumed.
        config_id: String,
        /// Exactly the values this adapter said it accepts.
        offered: Vec<String>,
    },
    /// `session/set_model`. ⚠ Unverifiable after the fact — the response carries no state.
    LegacyModels {
        /// Exactly the model ids this adapter said it accepts.
        offered: Vec<String>,
    },
    /// The harness advertised no model selector at all. A pinned model cannot be honoured, and a
    /// seat that pins one must fail rather than run on the default.
    Unsupported,
}

impl ModelSupport {
    /// Classify a `session/new` (or `session/load`) result.
    ///
    /// Prefers the config-option dialect whenever it is advertised, because it is the only one
    /// whose application can be confirmed.
    pub fn read(session_result: &Value) -> Self {
        if let Some(option) = model_config_option(session_result.get("configOptions")) {
            if let Some(config_id) = option.get("id").and_then(Value::as_str) {
                return Self::ConfigOption {
                    config_id: config_id.to_owned(),
                    offered: offered_values(option.get("options"), "value"),
                };
            }
        }
        if let Some(models) = session_result.get("models") {
            return Self::LegacyModels {
                offered: offered_values(models.get("availableModels"), "modelId"),
            };
        }
        Self::Unsupported
    }

    /// The dialect name, for operator-facing messages.
    fn dialect(&self) -> &'static str {
        match self {
            Self::ConfigOption { .. } => "session/set_config_option",
            Self::LegacyModels { .. } => "session/set_model",
            Self::Unsupported => "none",
        }
    }

    /// What this harness said it accepts. Empty for [`Self::Unsupported`].
    fn offered(&self) -> &[String] {
        match self {
            Self::ConfigOption { offered, .. } | Self::LegacyModels { offered } => offered,
            Self::Unsupported => &[],
        }
    }

    /// The JSON-RPC call that pins `model` on `session_id`, or the reason it cannot be pinned.
    ///
    /// Refuses before sending anything when the harness advertises no selector, or when `model` is
    /// not one of the values it advertised — the error names the adapter and lists what it does
    /// accept, so the operator can fix the config from the message alone.
    pub fn request(
        &self,
        adapter: &str,
        session_id: &SessionId,
        model: &str,
    ) -> Result<ModelRequest, DriverError> {
        if matches!(self, Self::Unsupported) {
            return Err(DriverError::Other(format!(
                "harness {adapter:?} does not support model selection: its session advertised \
                 neither a model config option nor a models list, so the configured model \
                 {model:?} cannot be applied. Remove the model from this agent's config, or run a \
                 harness that accepts one."
            )));
        }
        if !self.offered().iter().any(|value| value == model) {
            return Err(DriverError::Other(format!(
                "harness {adapter:?} does not offer model {model:?} (via {dialect}); it accepts: \
                 [{offered}]. The configured model must be one of these EXACTLY — a near match is \
                 never substituted, because an adapter that resolves an unknown name to its \
                 default reports success while running a different model.",
                dialect = self.dialect(),
                offered = self.offered().join(", "),
            )));
        }
        Ok(match self {
            Self::ConfigOption { config_id, .. } => ModelRequest {
                method: "session/set_config_option",
                params: json!({
                    "sessionId": session_id,
                    "configId": config_id,
                    "value": model,
                }),
                config_id: Some(config_id.clone()),
                requested: model.to_owned(),
            },
            Self::LegacyModels { .. } => ModelRequest {
                method: "session/set_model",
                params: json!({
                    "sessionId": session_id,
                    "modelId": model,
                }),
                config_id: None,
                requested: model.to_owned(),
            },
            Self::Unsupported => unreachable!("refused above"),
        })
    }
}

/// A pending model change: the call to make, and what must hold afterwards.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRequest {
    /// JSON-RPC method to call.
    pub method: &'static str,
    /// JSON-RPC params.
    pub params: Value,
    /// The option to re-read on the response. `None` ⇒ the legacy dialect, which returns no state.
    config_id: Option<String>,
    /// The value sent — always one the adapter advertised, so the read-back compares exactly.
    requested: String,
}

impl ModelRequest {
    /// Confirm from the set response that the session is now on the requested model.
    ///
    /// On the config-option dialect this is a real check: the response carries the model option's
    /// `currentValue`, which must equal what was sent. A response that omits the option, or
    /// reports a different value, is a FAILURE — an adapter that accepts a value and then runs
    /// another one is the whole reason this is checked rather than assumed.
    ///
    /// On the legacy dialect there is nothing to read: the response is `{}` on every adapter
    /// measured. Acceptance without a JSON-RPC error is all that dialect can offer, and it is
    /// weaker on purpose rather than by oversight.
    pub fn confirm(&self, adapter: &str, response: &Value) -> Result<ModelOutcome, DriverError> {
        let Some(config_id) = &self.config_id else {
            return Ok(ModelOutcome {
                model: self.requested.clone(),
                confirmed: false,
            });
        };
        let current = model_config_option(response.get("configOptions"))
            .or_else(|| {
                // Fall back to the id we addressed: an adapter may answer without categories.
                response
                    .get("configOptions")
                    .and_then(Value::as_array)
                    .and_then(|options| {
                        options.iter().find(|option| {
                            option.get("id").and_then(Value::as_str) == Some(config_id.as_str())
                        })
                    })
            })
            .and_then(|option| option.get("currentValue"))
            .and_then(Value::as_str);
        match current {
            Some(current) if current == self.requested => Ok(ModelOutcome {
                model: self.requested.clone(),
                confirmed: true,
            }),
            Some(other) => Err(DriverError::Other(format!(
                "harness {adapter:?} accepted model {requested:?} but reports the session is on \
                 {other:?} — refusing to prompt a model that was not the one configured",
                requested = self.requested,
            ))),
            None => Err(DriverError::Other(format!(
                "harness {adapter:?} accepted model {requested:?} but its \
                 session/set_config_option response carried no current value for config \
                 {config_id:?}, so the model in effect is unknown — refusing to prompt on it",
                requested = self.requested,
            ))),
        }
    }
}

/// The model a session ended up on, and whether the harness confirmed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelOutcome {
    /// The model requested and accepted.
    pub model: String,
    /// `true` when the harness echoed it back as current; `false` on the legacy dialect, which
    /// cannot report state. Never `true` on an unread response.
    pub confirmed: bool,
}

/// The model selector among `configOptions`: by category first (the spec's semantic marker), then
/// by the conventional id, since the schema makes category optional and UX-only. Never matched on
/// display names — a `name` is free text and guessing from it would pick a neighbouring selector.
fn model_config_option(config_options: Option<&Value>) -> Option<&Value> {
    let options = config_options?.as_array()?;
    options
        .iter()
        .find(|option| option.get("category").and_then(Value::as_str) == Some(MODEL_CATEGORY))
        .or_else(|| {
            options
                .iter()
                .find(|option| option.get("id").and_then(Value::as_str) == Some(MODEL_CONFIG_ID))
        })
}

/// The advertised values under `key`, in the order the adapter listed them.
fn offered_values(list: Option<&Value>, key: &str) -> Vec<String> {
    list.and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.get(key).and_then(Value::as_str))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `session/new` result shaped like claude-agent-acp 0.45.1's (captured from the adapter).
    fn claude_session() -> Value {
        json!({
            "sessionId": "s-1",
            "modes": { "currentModeId": "default", "availableModes": [] },
            "configOptions": [
                { "id": "mode", "name": "Mode", "category": "mode", "type": "select",
                  "currentValue": "default", "options": [{ "value": "default", "name": "Default" }] },
                { "id": "model", "name": "Model", "category": "model", "type": "select",
                  "currentValue": "fable", "options": [
                      { "value": "default", "name": "Default" },
                      { "value": "opus[1m]", "name": "Opus" },
                      { "value": "fable", "name": "Fable" },
                      { "value": "sonnet", "name": "Sonnet" },
                      { "value": "haiku", "name": "Haiku" }] },
                { "id": "effort", "name": "Effort", "category": "thought_level", "type": "select",
                  "currentValue": "medium", "options": [{ "value": "medium", "name": "Medium" }] }
            ]
        })
    }

    /// A `session/new` result carrying only the legacy `models` object.
    fn legacy_session() -> Value {
        json!({
            "sessionId": "s-2",
            "models": {
                "currentModelId": "grok-4.5[effort=high,fast=true]",
                "availableModels": [
                    { "modelId": "default[]", "name": "Auto" },
                    { "modelId": "grok-4.5[effort=high,fast=true]", "name": "grok-4.5" }
                ]
            }
        })
    }

    #[test]
    fn config_option_dialect_is_read_from_the_session_response() {
        let support = ModelSupport::read(&claude_session());
        assert_eq!(
            support,
            ModelSupport::ConfigOption {
                config_id: "model".into(),
                offered: vec![
                    "default".into(),
                    "opus[1m]".into(),
                    "fable".into(),
                    "sonnet".into(),
                    "haiku".into()
                ],
            }
        );
    }

    #[test]
    fn legacy_models_dialect_is_read_from_the_session_response() {
        assert_eq!(
            ModelSupport::read(&legacy_session()),
            ModelSupport::LegacyModels {
                offered: vec!["default[]".into(), "grok-4.5[effort=high,fast=true]".into()],
            }
        );
    }

    #[test]
    fn config_option_dialect_wins_when_an_adapter_advertises_both() {
        // codex-acp 1.1.2 and cursor-agent advertise both. The config-option dialect must win: it
        // is the only one whose set can be read back, and its ids are the clean ones.
        let mut both = claude_session();
        both["models"] = json!({
            "currentModelId": "gpt-5.6-sol[medium]",
            "availableModels": [{ "modelId": "gpt-5.6-sol[medium]", "name": "sol" }]
        });
        assert!(matches!(
            ModelSupport::read(&both),
            ModelSupport::ConfigOption { .. }
        ));
    }

    #[test]
    fn a_session_advertising_neither_is_unsupported() {
        assert_eq!(
            ModelSupport::read(&json!({ "sessionId": "s-3" })),
            ModelSupport::Unsupported
        );
    }

    #[test]
    fn the_model_option_is_found_by_category_even_under_an_unconventional_id() {
        let session = json!({
            "configOptions": [
                { "id": "llm", "name": "LLM", "category": "model", "type": "select",
                  "currentValue": "a", "options": [{ "value": "a" }, { "value": "b" }] }
            ]
        });
        assert_eq!(
            ModelSupport::read(&session),
            ModelSupport::ConfigOption {
                config_id: "llm".into(),
                offered: vec!["a".into(), "b".into()],
            }
        );
    }

    #[test]
    fn the_model_option_is_found_by_id_when_no_category_is_declared() {
        // The ACP schema makes `category` optional and UX-only, so an adapter may omit it.
        let session = json!({
            "configOptions": [
                { "id": "model", "name": "Model", "type": "select",
                  "currentValue": "a", "options": [{ "value": "a" }] }
            ]
        });
        assert!(matches!(
            ModelSupport::read(&session),
            ModelSupport::ConfigOption { .. }
        ));
    }

    #[test]
    fn an_unsupported_harness_refuses_a_pinned_model_by_name() {
        let error = ModelSupport::Unsupported
            .request("some-adapter", &"s".to_owned(), "haiku")
            .expect_err("unsupported must refuse");
        let message = error.to_string();
        assert!(message.contains("some-adapter"), "{message}");
        assert!(message.contains("haiku"), "{message}");
        assert!(
            message.contains("does not support model selection"),
            "{message}"
        );
    }

    #[test]
    fn a_model_the_harness_never_offered_is_refused_before_anything_is_sent() {
        // ★ The measured failure mode: claude-agent-acp 0.45.1 answers a garbage model with
        // success and silently reports `default`. Validating against the advertised list is what
        // turns that into a config error instead of a job that runs the wrong model.
        let support = ModelSupport::read(&claude_session());
        let error = support
            .request("claude-agent-acp", &"s".to_owned(), "claude-haiku-4-5")
            .expect_err("a model outside the offered list must be refused");
        let message = error.to_string();
        assert!(message.contains("claude-haiku-4-5"), "{message}");
        // The error must list what IS accepted — the operator fixes the config from this alone.
        for offered in ["default", "opus[1m]", "fable", "sonnet", "haiku"] {
            assert!(message.contains(offered), "{message} missing {offered}");
        }
    }

    #[test]
    fn an_offered_model_builds_the_config_option_call() {
        let support = ModelSupport::read(&claude_session());
        let request = support
            .request("claude-agent-acp", &"s-1".to_owned(), "haiku")
            .expect("offered model");
        assert_eq!(request.method, "session/set_config_option");
        assert_eq!(request.params["sessionId"], json!("s-1"));
        assert_eq!(request.params["configId"], json!("model"));
        assert_eq!(request.params["value"], json!("haiku"));
    }

    #[test]
    fn an_offered_model_builds_the_legacy_call() {
        let support = ModelSupport::read(&legacy_session());
        let request = support
            .request("cursor-agent", &"s-2".to_owned(), "default[]")
            .expect("offered model");
        assert_eq!(request.method, "session/set_model");
        assert_eq!(request.params["modelId"], json!("default[]"));
        assert!(
            request.params.get("configId").is_none(),
            "legacy call must not carry a configId: {:?}",
            request.params
        );
    }

    #[test]
    fn a_readback_matching_the_request_confirms() {
        let support = ModelSupport::read(&claude_session());
        let request = support
            .request("claude-agent-acp", &"s-1".to_owned(), "haiku")
            .expect("offered");
        let response = json!({
            "configOptions": [
                { "id": "model", "category": "model", "currentValue": "haiku",
                  "options": [{ "value": "haiku" }] }
            ]
        });
        let outcome = request
            .confirm("claude-agent-acp", &response)
            .expect("matching readback confirms");
        assert_eq!(outcome.model, "haiku");
        assert!(outcome.confirmed);
    }

    #[test]
    fn a_readback_reporting_a_different_model_is_an_error() {
        // ★ The lie this whole module exists to catch: the adapter answers OK and reports another
        // model. Measured for real against claude-agent-acp 0.45.1, which resolves an unknown
        // value to `default` and returns success.
        let support = ModelSupport::read(&claude_session());
        let request = support
            .request("claude-agent-acp", &"s-1".to_owned(), "haiku")
            .expect("offered");
        let response = json!({
            "configOptions": [
                { "id": "model", "category": "model", "currentValue": "default" }
            ]
        });
        let error = request
            .confirm("claude-agent-acp", &response)
            .expect_err("a substituted model must fail");
        let message = error.to_string();
        assert!(message.contains("haiku"), "{message}");
        assert!(message.contains("default"), "{message}");
    }

    #[test]
    fn a_readback_with_no_current_value_is_an_error_not_a_pass() {
        // An accepted-but-unreadable set leaves the running model unknown. Treating that as
        // success is how a check whose failure mode is silence gets built.
        let support = ModelSupport::read(&claude_session());
        let request = support
            .request("claude-agent-acp", &"s-1".to_owned(), "haiku")
            .expect("offered");
        for response in [json!({}), json!({ "configOptions": [] })] {
            let error = request
                .confirm("claude-agent-acp", &response)
                .expect_err("an unreadable response must fail");
            assert!(error.to_string().contains("unknown"), "{error}");
        }
    }

    #[test]
    fn the_legacy_dialect_reports_an_unconfirmed_outcome_rather_than_claiming_confirmation() {
        // session/set_model answers `{}` on every adapter measured. The outcome must say so
        // instead of inferring success from an empty body.
        let support = ModelSupport::read(&legacy_session());
        let request = support
            .request("cursor-agent", &"s-2".to_owned(), "default[]")
            .expect("offered");
        let outcome = request
            .confirm("cursor-agent", &json!({}))
            .expect("legacy set cannot fail on read-back");
        assert_eq!(outcome.model, "default[]");
        assert!(
            !outcome.confirmed,
            "the legacy dialect cannot confirm anything"
        );
    }
}
