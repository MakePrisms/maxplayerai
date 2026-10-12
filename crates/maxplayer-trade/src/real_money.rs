//! Compiled-in exposure limits, independent of mint identity.
use anyhow::{Result, ensure};
pub const CAP: u64 = 100_000;

pub fn check_lock_gross(gross: u64) -> Result<()> {
    ensure!(gross <= CAP, "lock gross exceeds 100,000 sats");
    Ok(())
}
#[cfg(test)]
mod tests {
    #[test]
    fn compiled_lock_gross_boundary() {
        assert!(super::check_lock_gross(100_000).is_ok());
        assert!(
            super::check_lock_gross(100_001).is_err(),
            "SAFETY: lock gross cap must reject 100,001"
        );
        assert!(super::check_lock_gross(u64::MAX).is_err());
    }
}
