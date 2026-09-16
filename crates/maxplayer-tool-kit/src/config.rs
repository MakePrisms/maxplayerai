//! Seller configuration: the offering, and the operations it includes.
//!
//! This is the whole entitlement model. There is no per-job counterpart to any of it.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SellerToolConfig {
    /// Identifies the seller whose daemon owns this holder.
    pub seller_id: String,
    /// Human-readable offering. Declared by the seller, not derived from buyer text.
    pub offering: String,
    /// `http://host:port` of the vendor service the tool talks to.
    pub vendor_base_url: String,
    /// The operations this offering includes. Shared by every job of this seller.
    pub operations: Vec<OperationSpec>,
}

impl SellerToolConfig {
    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let cfg: Self = serde_json::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))?;
        cfg.check()?;
        Ok(cfg)
    }

    /// Structural checks that must hold before the daemon will serve anything.
    pub fn check(&self) -> Result<(), String> {
        if self.seller_id.trim().is_empty() {
            return Err("seller_id must not be empty".into());
        }
        if !self.vendor_base_url.starts_with("http://") {
            return Err("vendor_base_url must be http:// (this kit ships no TLS)".into());
        }
        if self.operations.is_empty() {
            return Err("an offering with no operations cannot be served".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        for op in &self.operations {
            if !seen.insert(&op.name) {
                return Err(format!("duplicate operation {}", op.name));
            }
            op.check()?;
        }
        Ok(())
    }

    pub fn operation(&self, name: &str) -> Option<&OperationSpec> {
        self.operations.iter().find(|o| o.name == name)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OperationSpec {
    pub name: String,
    pub description: String,
    /// Vendor CLI subcommand this maps to. Fixed by configuration; never job-selectable.
    pub subcommand: String,
    pub params: Vec<ParamSpec>,
    /// Hard ceiling on bytes the holder will return for one call, enforced by the holder.
    pub max_output_bytes: usize,
}

impl OperationSpec {
    fn check(&self) -> Result<(), String> {
        if !is_plain_ident(&self.name) {
            return Err(format!("operation name {:?} must be [a-z0-9_-]", self.name));
        }
        if !is_plain_ident(&self.subcommand) {
            return Err(format!("subcommand {:?} must be [a-z0-9_-]", self.subcommand));
        }
        if self.max_output_bytes == 0 {
            return Err(format!("operation {} has no output ceiling", self.name));
        }
        let mut seen = std::collections::BTreeSet::new();
        for p in &self.params {
            if !seen.insert(&p.name) {
                return Err(format!("operation {} has duplicate param {}", self.name, p.name));
            }
            p.check(&self.name)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ParamSpec {
    pub name: String,
    /// Vendor CLI flag this param fills, e.g. `--in`. Fixed by configuration.
    pub flag: String,
    pub kind: ParamKind,
}

impl ParamSpec {
    fn check(&self, op: &str) -> Result<(), String> {
        if !is_plain_ident(&self.name) {
            return Err(format!("{op}: param name {:?} must be [a-z0-9_-]", self.name));
        }
        if !self.flag.starts_with("--") || !is_plain_ident(self.flag.trim_start_matches('-')) {
            return Err(format!("{op}: flag {:?} must be --[a-z0-9_-]", self.flag));
        }
        if let ParamKind::Choice { choices } = &self.kind {
            if choices.is_empty() {
                return Err(format!("{op}: param {} has an empty choice set", self.name));
            }
        }
        if let ParamKind::Text { max_len } = &self.kind {
            if *max_len == 0 || *max_len > 8192 {
                return Err(format!("{op}: param {} max_len must be 1..=8192", self.name));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ParamKind {
    /// Bounded literal text. Never interpreted by a shell; passed as one argv element.
    Text { max_len: usize },
    /// One of a fixed set declared by the seller.
    Choice { choices: Vec<String> },
    /// A file the calling job supplies, resolved inside that job's own directory.
    JobInputFile,
    /// A file the holder will create inside the calling job's own directory.
    JobOutputFile,
}

pub fn is_plain_ident(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}
