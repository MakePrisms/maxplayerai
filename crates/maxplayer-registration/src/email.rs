//! Resend client: sends a published template by id or alias plus its variables.
//!
//! Template variables (Resend drafts `maxplayer-code` and `maxplayer-welcome`, as
//! created on 27 Oct 2026 and read back from the Resend API on 28 Oct):
//! - code:    CODE, AGENT_LABEL, NPUB_SHORT, TERMS_URL, PRIVACY_URL
//! - welcome: AGENT_LABEL, TERMS_URL, PRIVACY_URL
//!
//! Resend substitutes `{{{VAR}}}` without escaping, so every value is HTML-escaped
//! here. The subject and design live in the template; `from` is sent explicitly.

use reqwest::header::AUTHORIZATION;
use serde_json::{Value, json};

use crate::config::EmailConfig;
use crate::validate::html_escape;

pub struct EmailClient {
    http: reqwest::Client,
    base: String,
    api_key: Option<String>,
    from: String,
    pub code_template: String,
    pub welcome_template: String,
}

pub struct CodeEmail<'a> {
    pub code: &'a str,
    pub agent_label: &'a str,
    pub npub_short: &'a str,
    pub terms_url: &'a str,
    pub privacy_url: &'a str,
}

pub struct WelcomeEmail<'a> {
    pub agent_label: &'a str,
    pub terms_url: &'a str,
    pub privacy_url: &'a str,
}

pub fn code_variables(c: &CodeEmail<'_>) -> Value {
    json!({
        "CODE": html_escape(c.code),
        "AGENT_LABEL": html_escape(c.agent_label),
        "NPUB_SHORT": html_escape(c.npub_short),
        "TERMS_URL": html_escape(c.terms_url),
        "PRIVACY_URL": html_escape(c.privacy_url),
    })
}

pub fn welcome_variables(w: &WelcomeEmail<'_>) -> Value {
    json!({
        "AGENT_LABEL": html_escape(w.agent_label),
        "TERMS_URL": html_escape(w.terms_url),
        "PRIVACY_URL": html_escape(w.privacy_url),
    })
}

impl EmailClient {
    pub fn new(cfg: &EmailConfig, api_key: Option<String>) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(cfg.timeout_secs))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            http,
            base: cfg.resend_api_base.trim_end_matches('/').to_string(),
            api_key,
            from: cfg.from.clone(),
            code_template: cfg.code_template.clone(),
            welcome_template: cfg.welcome_template.clone(),
        })
    }

    pub fn has_api_key(&self) -> bool {
        self.api_key.is_some()
    }

    /// Returns Resend's email id. Errors never contain the address or the variables.
    pub async fn send_template(
        &self,
        to: &str,
        template: &str,
        variables: Value,
        idempotency_key: &str,
    ) -> Result<String, String> {
        let key = self
            .api_key
            .as_deref()
            .ok_or("resend api key not configured")?;
        let body = json!({
            "from": self.from,
            "to": [to],
            "template": { "id": template, "variables": variables },
        });
        let resp = self
            .http
            .post(format!("{}/emails", self.base))
            .header(AUTHORIZATION, format!("Bearer {key}"))
            .header("Idempotency-Key", idempotency_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("resend unreachable: {}", e.without_url()))?;
        let status = resp.status();
        let json: Value = resp.json().await.unwrap_or(Value::Null);
        if status.is_success() {
            return Ok(json["id"].as_str().unwrap_or_default().to_string());
        }
        // Resend's `message` can quote the recipient; only its error `name` is kept.
        let name = json["name"].as_str().unwrap_or("unknown");
        Err(format!("resend HTTP {}: {name}", status.as_u16()))
    }

    pub async fn send_code(
        &self,
        to: &str,
        c: &CodeEmail<'_>,
        idempotency_key: &str,
    ) -> Result<String, String> {
        self.send_template(to, &self.code_template, code_variables(c), idempotency_key)
            .await
    }

    pub async fn send_welcome(
        &self,
        to: &str,
        w: &WelcomeEmail<'_>,
        idempotency_key: &str,
    ) -> Result<String, String> {
        self.send_template(
            to,
            &self.welcome_template,
            welcome_variables(w),
            idempotency_key,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variables_match_the_resend_templates_exactly() {
        let v = code_variables(&CodeEmail {
            code: "482913",
            agent_label: "my-laptop",
            npub_short: "npub1qz7…8k3x",
            terms_url: "https://www.maxplayer.ai/terms/v1",
            privacy_url: "https://www.maxplayer.ai/privacy/v1",
        });
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "AGENT_LABEL",
                "CODE",
                "NPUB_SHORT",
                "PRIVACY_URL",
                "TERMS_URL"
            ]
        );
        let w = welcome_variables(&WelcomeEmail {
            agent_label: "my-laptop",
            terms_url: "https://www.maxplayer.ai/terms/v1",
            privacy_url: "https://www.maxplayer.ai/privacy/v1",
        });
        let mut keys: Vec<&str> = w.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(keys, ["AGENT_LABEL", "PRIVACY_URL", "TERMS_URL"]);
    }

    #[test]
    fn values_are_html_escaped() {
        let w = welcome_variables(&WelcomeEmail {
            agent_label: "<script>",
            terms_url: "https://x/?a=1&b=2",
            privacy_url: "p",
        });
        assert_eq!(w["AGENT_LABEL"], "&lt;script&gt;");
        assert_eq!(w["TERMS_URL"], "https://x/?a=1&amp;b=2");
    }
}
