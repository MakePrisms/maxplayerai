//! Which terms and privacy versions a link request may name.
//!
//! The built-in lists (`legal.terms_versions` / `privacy_versions`) always count. A dated version
//! (`YYYYMMDD`) missing from them also counts when the website's manifest
//! (`legal.versions_url`, `web/app/public/legal/versions.json`) lists it, so publishing a new
//! version on the site needs no deploy of this service. Versions learned from the manifest are
//! stored in `legal_versions` and stay accepted, even if the site is down later or the file
//! drops them.
//!
//! The manifest is fetched at most once per `versions_refresh_secs`, a failed fetch is
//! remembered as long, and only a well-formed JSON file counts: a missing file, an HTML page in
//! its place, a redirect or a malformed entry accepts nothing new.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, TimeDelta, Utc};
use serde::Deserialize;
use sqlx::PgPool;
use tokio::sync::Mutex;
use tracing::warn;

use crate::config::LegalConfig;
use crate::db;

/// The manifest is a few hundred bytes; anything far larger is not it.
const MAX_MANIFEST_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Doc {
    Terms,
    Privacy,
}

impl Doc {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Terms => "terms",
            Self::Privacy => "privacy",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    Offered,
    NotOffered,
    /// The version is not known yet and the manifest could not be read: try again later.
    Unavailable,
}

/// `YYYYMMDD`: the only shape a version learned from the manifest may have.
pub fn is_dated_version(v: &str) -> bool {
    v.len() == 8 && v.bytes().all(|b| b.is_ascii_digit())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub terms: Vec<String>,
    pub privacy: Vec<String>,
}

impl Manifest {
    fn list(&self, doc: Doc) -> &[String] {
        match doc {
            Doc::Terms => &self.terms,
            Doc::Privacy => &self.privacy,
        }
    }
}

#[derive(Deserialize)]
struct Current {
    terms: String,
    privacy: String,
}

#[derive(Deserialize)]
struct Raw {
    current: Current,
    terms: Vec<String>,
    privacy: Vec<String>,
}

/// Parse and check the manifest: every entry dated, `current` listed. Anything else is refused
/// as a whole rather than partly trusted.
pub fn parse_manifest(body: &[u8]) -> Result<Manifest> {
    let raw: Raw = serde_json::from_slice(body).context("not a legal versions manifest")?;
    for (name, list, current) in [
        ("terms", &raw.terms, &raw.current.terms),
        ("privacy", &raw.privacy, &raw.current.privacy),
    ] {
        if list.is_empty() || list.len() > 256 {
            bail!("manifest {name} list is empty or too long");
        }
        if let Some(bad) = list.iter().find(|v| !is_dated_version(v)) {
            bail!("manifest {name} entry {bad:?} is not YYYYMMDD");
        }
        if !list.contains(current) {
            bail!("manifest current {name} {current:?} is not listed");
        }
    }
    Ok(Manifest {
        terms: raw.terms,
        privacy: raw.privacy,
    })
}

struct Fetched {
    at: DateTime<Utc>,
    result: Result<Manifest, String>,
}

pub struct ManifestSource {
    http: reqwest::Client,
    url: String,
    refresh: TimeDelta,
    last: Mutex<Option<Fetched>>,
}

impl ManifestSource {
    pub fn new(cfg: &LegalConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.versions_timeout_secs))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!(
                "maxplayer-registration/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()?;
        Ok(Self {
            http,
            url: cfg.versions_url.clone(),
            refresh: TimeDelta::seconds(cfg.versions_refresh_secs.min(i64::MAX as u64) as i64),
            last: Mutex::new(None),
        })
    }

    pub fn enabled(&self) -> bool {
        !self.url.is_empty()
    }

    /// The manifest as of at most `versions_refresh_secs` ago. One fetch at a time: concurrent
    /// callers wait for it and share its result.
    pub async fn get(&self, now: DateTime<Utc>) -> Result<Manifest, String> {
        let mut last = self.last.lock().await;
        if let Some(f) = last.as_ref()
            && now >= f.at
            && now - f.at < self.refresh
        {
            return f.result.clone();
        }
        let result = self.fetch().await.map_err(|e| format!("{e:#}"));
        if let Err(e) = &result {
            warn!(url = %self.url, error = %e, "legal versions manifest unavailable");
        }
        *last = Some(Fetched {
            at: now,
            result: result.clone(),
        });
        result
    }

    async fn fetch(&self) -> Result<Manifest> {
        let mut resp = self
            .http
            .get(&self.url)
            .header("Accept", "application/json")
            .send()
            .await
            .context("fetch")?;
        let status = resp.status();
        if !status.is_success() {
            bail!("HTTP {status}");
        }
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.context("read")? {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_MANIFEST_BYTES {
                bail!("larger than {MAX_MANIFEST_BYTES} bytes");
            }
        }
        parse_manifest(&body)
    }
}

/// Whether a link request may name `version` of `doc`.
pub async fn check(
    cfg: &LegalConfig,
    source: &ManifestSource,
    pool: &PgPool,
    doc: Doc,
    version: &str,
    now: DateTime<Utc>,
) -> Result<Check> {
    let builtin = match doc {
        Doc::Terms => &cfg.terms_versions,
        Doc::Privacy => &cfg.privacy_versions,
    };
    if builtin.iter().any(|v| v == version) {
        return Ok(Check::Offered);
    }
    if !is_dated_version(version) {
        return Ok(Check::NotOffered);
    }
    if db::legal_version_known(pool, doc.as_str(), version).await? {
        return Ok(Check::Offered);
    }
    if !source.enabled() {
        return Ok(Check::NotOffered);
    }
    let manifest = match source.get(now).await {
        Ok(m) => m,
        Err(_) => return Ok(Check::Unavailable),
    };
    if !manifest.list(doc).iter().any(|v| v == version) {
        return Ok(Check::NotOffered);
    }
    for d in [Doc::Terms, Doc::Privacy] {
        db::record_legal_versions(pool, d.as_str(), manifest.list(d), now).await?;
    }
    Ok(Check::Offered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_well_formed_manifest_parses() {
        let good = br#"{"current":{"terms":"20261123","privacy":"20261123"},
            "terms":["20261029","20261123"],"privacy":["20261029","20261123"]}"#;
        let m = parse_manifest(good).unwrap();
        assert_eq!(m.terms, ["20261029", "20261123"]);
        assert_eq!(m.privacy, ["20261029", "20261123"]);
        // An unknown extra key is fine (the site may list fees later).
        assert!(
            parse_manifest(
                br#"{"current":{"terms":"20261123","privacy":"20261123"},"fees":["20261029"],
                "terms":["20261123"],"privacy":["20261123"]}"#
            )
            .is_ok()
        );
        for bad in [
            // A catch-all HTML page served in its place.
            &b"<!doctype html><html><body>Not found</body></html>"[..],
            b"",
            b"{}",
            br#"{"current":{"terms":"20261123","privacy":"20261123"},"terms":[],"privacy":["20261123"]}"#,
            br#"{"current":{"terms":"20261123","privacy":"20261123"},"terms":["v2"],"privacy":["20261123"]}"#,
            br#"{"current":{"terms":"20261123","privacy":"20261123"},"terms":["2026112"],"privacy":["20261123"]}"#,
            br#"{"current":{"terms":"20261231","privacy":"20261123"},"terms":["20261123"],"privacy":["20261123"]}"#,
            br#"{"current":{"terms":"20261123","privacy":"20261123"},"terms":["20261123"]}"#,
        ] {
            assert!(
                parse_manifest(bad).is_err(),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn dated_versions_are_eight_digits() {
        assert!(is_dated_version("20261123"));
        for bad in [
            "v1",
            "2026112",
            "202611230",
            "2026-11-",
            "２０２６１１２３",
            "",
        ] {
            assert!(!is_dated_version(bad), "{bad}");
        }
    }
}
