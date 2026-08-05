//! Guard against nested `block_on` (recurring MCP crash class).
//!
//! Sync `*_blocking` / `block_on` wrappers must refuse when a Tokio runtime is
//! already current — callers must use the `_async` variant instead. A future
//! regression becomes a caught error, not a live-acceptance server-exit.

/// Returns `Err` when `Handle::try_current()` succeeds.
///
/// `op` names the sync wrapper (e.g. `open_wallet_blocking`) so the
/// error points at the `_async` twin.
pub fn refuse_nested_block_on(op: &str) -> Result<(), String> {
    if tokio::runtime::Handle::try_current().is_ok() {
        Err(format!(
            "{op}: nested block_on refused — call the _async variant from an async context"
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuse_ok_outside_runtime() {
        refuse_nested_block_on("test_op").expect("no runtime");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn refuse_err_inside_runtime() {
        let err = refuse_nested_block_on("test_op").expect_err("must refuse");
        assert!(err.contains("nested block_on refused"), "{err}");
        assert!(err.contains("test_op"), "{err}");
    }
}
