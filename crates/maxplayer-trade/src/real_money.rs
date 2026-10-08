//! Explicit process-local authorization. No persisted or environment-only mint bypass.
use anyhow::{Result, ensure};
use std::sync::OnceLock;
static ALLOWED: OnceLock<Vec<String>> = OnceLock::new();
pub const CAP: u64 = 500;

pub fn configure(urls: Vec<String>) -> Result<()> {
    ensure!(
        urls.is_empty() || std::env::var("TRADE_REAL_MONEY_TEST").ok().as_deref() == Some("1"),
        "real mint URLs require TRADE_REAL_MONEY_TEST=1"
    );
    for url in &urls {
        let asset = crate::Asset::new(url)?;
        ensure!(
            asset.mint_url == *url && url.starts_with("https://"),
            "real mint allow requires exact canonical HTTPS URL"
        );
    }
    ensure!(
        ALLOWED.set(urls).is_ok(),
        "mint authorization already configured"
    );
    Ok(())
}
pub fn allows(url: &str) -> bool {
    ALLOWED
        .get()
        .is_some_and(|urls| urls.iter().any(|u| u == url))
}

pub fn check_lock_gross(gross: u64) -> Result<()> {
    ensure!(gross <= CAP, "real-money lock gross exceeds 500 sats");
    Ok(())
}
#[cfg(test)]
mod tests {
    #[test]
    fn compiled_lock_gross_boundary() {
        assert!(super::check_lock_gross(500).is_ok());
        assert!(super::check_lock_gross(501).is_err());
        assert!(super::check_lock_gross(u64::MAX).is_err());
    }
}
