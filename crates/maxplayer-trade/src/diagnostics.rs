//! Bounded, payload-free admission output shared with a serve client. Not financial state.
use std::sync::{Arc, Mutex};
pub type Lines = Arc<Mutex<Vec<String>>>;
tokio::task_local! { static ADMISSION: Lines; }
pub fn emit(line: impl std::fmt::Display) {
    let line = line.to_string();
    eprintln!("{line}");
    let _ = ADMISSION.try_with(|lines| {
        if let Ok(mut lines) = lines.lock() {
            // At most two mints' info evidence, summaries and a shared notice. Never a queue
            // proportional to ongoing recovery; only the admission command runs in this scope.
            if lines.len() < 16 && line.len() <= 16 * 1024 {
                lines.push(line);
            }
        }
    });
}
pub async fn scoped<F: std::future::Future>(lines: Lines, f: F) -> F::Output {
    ADMISSION.scope(lines, f).await
}
pub fn notice() {
    emit(
        "Both mints must remain reachable through their mint relays while this trade runs. This check confirms only the shown operations now, not later claims/refunds. After submission, an unanswered operation may have moved funds: use its status/recovery, not a replacement spend.",
    );
}
