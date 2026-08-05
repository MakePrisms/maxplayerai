//! The mandatory delivery execution sentinel (protocol v1 §19).
//!
//! Every paid delivery MUST carry an execution sentinel INSIDE the delivered tree — never as a tag
//! on the delivery event. A tag is authored by the seller at publish time and can be emitted without
//! the workdir ever being touched (testimony); a file inside the delivered tree sits within the
//! artifact the buyer independently fetches and hashes (evidence). The motivating failure is
//! measured, not hypothetical: a quota-dead harness exits `0` with a `completed` status in ~2s having
//! written nothing, so every status field reports success. The sentinel is the one signal that goes
//! red for exactly that harness.
//!
//! This module is the ONE place the sentinel's shape is decided, shared by the seller writer
//! ([`crate::seller_git::snapshot_delivery_at`]) and the buyer verifier
//! ([`crate::authorize_pay::authorize_pay_async`]). Keeping both ends on one definition is the same
//! discipline #357's `mint_probe_identity` follows — a second literal for the format at either end
//! could drift the writer out of step with the reader and silently turn a real check inert. The
//! module is deliberately dependency-free (no `git2`, no features) so both feature sets compile it.
//!
//! ## What binds the sentinel to THIS job (replay resistance)
//! The sentinel is seeded from the awarded job's `job_hash` (`sha256(job_id | task | amount)`), the
//! same value the buyer holds on its accept-bind (§7.5). A sentinel minted for one job carries that
//! job's hash and no other, so a delivery that replays a DIFFERENT job's sentinel fails the buyer's
//! match. The `job_hash` is derived from the offer, never handed to the harness and never a component
//! of any filesystem path (the seller workdir is keyed on `job_id`, not `job_hash`; the buyer store
//! on the commit oid), so — exactly as #357 keeps its secret out of the workdir path — the binding
//! element cannot be produced by a harness echoing its own cwd. That structural separation is the
//! real fix; the path subtraction in [`content_carries_sentinel`] is the belt to its braces.
//!
//! ## Normative limit (§19)
//! A sentinel proves EXECUTION IN THIS WORKDIR. It never proves work quality, and it can never stand
//! in for acceptance. Nothing here reads or asserts anything about the *content* of the delivered
//! work beyond "something was executed" — quality/acceptance live entirely outside this module.

/// The path, relative to the delivered tree root, the node writes the execution manifest to. A
/// fixed, upper-cased, non-hidden name (like `LICENSE`/`README`) so it is unambiguous in a libgit2
/// pathspec walk and reads as protocol metadata rather than job output. The node force-stages it
/// (bypassing any `.gitignore`) so a coincidental or hostile ignore rule can never drop it from the
/// snapshot — see [`crate::seller_git::snapshot_delivery_at`].
pub const SENTINEL_FILE: &str = "MAXPLAYER_EXECUTION_SENTINEL";

/// The non-secret marker that labels the manifest as a v1 execution sentinel. Distinct from #357's
/// `maxplayer-probe`/`maxplayer-selfprobe` prefixes on purpose: those mean a throwaway pre-advertise
/// capability probe; THIS means a per-job, buyer-verified delivery proof. A grep for either must land
/// on one meaning.
pub const SENTINEL_MARKER: &str = "maxplayer-execution-sentinel/v1";

/// Delivery mode recorded in the manifest — the parentage the node snapshotted under (§18.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryMode {
    /// Greenfield: a root commit whose tree is the whole workdir (parent count 0).
    FromScratch,
    /// Contribution: one commit parented on the buyer-pinned base (parent count 1).
    Contribution,
}

impl DeliveryMode {
    /// The stable wire/manifest label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FromScratch => "from-scratch",
            Self::Contribution => "contribution",
        }
    }
}

/// The single contiguous token the buyer matches: the marker AND this job's hash, together. Matching
/// the WHOLE token (never the marker alone) is what makes presence-without-binding — a stray marker
/// string, or a replayed sentinel carrying a different job's hash — fail. The seller writes this as
/// the manifest's first line, so it is present as an exact substring of a genuine delivery.
pub fn job_bound_token(job_hash: &str) -> String {
    format!("{SENTINEL_MARKER} job-hash={job_hash}")
}

/// Render the node's structured execution manifest — the minimum that proves execution in this
/// workdir (§19). Deterministic in its inputs (no wall-clock, no entropy): the same delivered tree +
/// `job_hash` produce byte-identical bytes, so a delivery commit re-created on resume keeps the same
/// oid (the re-push idempotency invariant the seller snapshot relies on). NOT a transcript: it carries
/// no prompt content and no agent conversation, only the job binding and the node-observed facts.
///
/// `files` / `bytes` are the node's own count and size of the delivered work (the sentinel file
/// itself excluded), recorded as evidence of what the node saw when it decided execution had happened.
pub fn render_manifest(job_hash: &str, mode: DeliveryMode, files: usize, bytes: u64) -> String {
    format!(
        "{token}\nmode: {mode}\nfiles: {files}\nbytes: {bytes}\n",
        token = job_bound_token(job_hash),
        mode = mode.as_str(),
    )
}

/// Whether one file's `content` carries THIS job's execution sentinel.
///
/// The check is a substring match for the job-bound token (marker + this job's `job_hash`), after
/// subtracting `subtract_path` from the content. `subtract_path` is the seller's workdir label the
/// buyer already knows (the `job_id`): removing it before matching means a sentinel reachable only by
/// echoing that path cannot count — the same belt #357's readback uses. Because the job_hash is not a
/// component of that path, a genuine manifest is never affected by the subtraction; the guard only
/// bites a would-be path echo. An empty `subtract_path` is a no-op (never an insert-everywhere
/// replace).
pub fn content_carries_sentinel(content: &str, job_hash: &str, subtract_path: &str) -> bool {
    let token = job_bound_token(job_hash);
    if subtract_path.is_empty() {
        return content.contains(&token);
    }
    content.replace(subtract_path, "").contains(&token)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The writer and the reader agree on ONE format: a manifest rendered for a job is recognised for
    // that job. Runs through the shared render + match, so a format drift at either end fails here.
    #[test]
    fn rendered_manifest_is_recognised_for_its_own_job() {
        let job_hash = "a".repeat(64);
        let manifest = render_manifest(&job_hash, DeliveryMode::FromScratch, 3, 128);
        assert!(
            content_carries_sentinel(&manifest, &job_hash, ""),
            "a manifest must satisfy the check for its own job hash"
        );
        assert!(manifest.contains(SENTINEL_MARKER), "carries the v1 marker");
        assert!(manifest.contains("mode: from-scratch"));
    }

    // Replay resistance: a manifest minted for one job MUST NOT satisfy the check for another. This is
    // the whole point of seeding the sentinel from the job hash.
    #[test]
    fn a_manifest_does_not_satisfy_a_different_job() {
        let this_job = "a".repeat(64);
        let other_job = "b".repeat(64);
        let manifest = render_manifest(&other_job, DeliveryMode::FromScratch, 1, 10);
        assert!(
            !content_carries_sentinel(&manifest, &this_job, ""),
            "a different job's sentinel must not validate this job (replay)"
        );
        assert!(
            content_carries_sentinel(&manifest, &other_job, ""),
            "and it does validate the job it was minted for (positive control)"
        );
    }

    // The marker alone is not a sentinel: content that mentions the marker but not THIS job's hash
    // (the quota-dead / decoration case) fails.
    #[test]
    fn marker_without_the_job_hash_is_not_a_sentinel() {
        let job_hash = "c".repeat(64);
        let decoy = format!("{SENTINEL_MARKER} was here, honest\n");
        assert!(
            !content_carries_sentinel(&decoy, &job_hash, ""),
            "the marker without the job-bound hash must not pass"
        );
    }

    // Path-echo belt: the job hash living ONLY inside the subtracted workdir-label text does not
    // count. (Constructed: the label IS the job hash, so subtracting it strips the token. A genuine
    // delivery is never shaped this way — job_id != job_hash — which is exactly why the structural
    // separation is the real fix and this is only the belt.)
    #[test]
    fn a_job_hash_reachable_only_through_the_path_label_does_not_count() {
        let job_hash = "d".repeat(64);
        let echoed = format!("cwd was /seller-jobs/{}\n", job_bound_token(&job_hash));
        assert!(
            content_carries_sentinel(&echoed, &job_hash, ""),
            "without subtraction the echoed token would pass"
        );
        assert!(
            !content_carries_sentinel(&echoed, &job_hash, &job_bound_token(&job_hash)),
            "subtracting the label text strips a token reachable only through it"
        );
    }
}
