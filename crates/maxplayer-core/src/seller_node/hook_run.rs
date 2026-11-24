//! Turn 1 of a job pinned to a HOOK entry (docs/specs/hook-agents.md §6–§7): the seller's own agent
//! behind a blocking command. Everything before this point (review, budget probe, creq, pin) and
//! after it (`deliver_inline`, `fail_job_with_feedback`) is the existing machinery.
use super::*;
use crate::hook::{self, JobOutcome};

/// The answer cap a hook is told and held to: the same constant `deliver_inline` enforces.
pub(super) const HOOK_ANSWER_MAX_BYTES: usize = INLINE_ANSWER_MAX_BYTES;

const HOOK_DELIVERY_REFUSED_FEEDBACK: &str =
    "seller's agent could not deliver an inline answer for this job; nothing was delivered";

impl SellerNodeRunner {
    /// Run a job on a hook entry and deliver or fail it. Called by `execute_job` after the review
    /// gate, the delivery-budget probe, the creq read, `assign_agent` and `mark_executing`.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn execute_hook_job(
        &self,
        job_id: &str,
        offer: &crate::seller_node::store::Offer,
        stored_creq: &str,
        harness: usize,
        argv: &[String],
        label: &str,
        kind: &crate::seller_agents::AgentKind,
    ) {
        let buyer = offer.buyer_pubkey.clone();
        // §3.1: the budget probe drops `inline` from the offer row when an inline answer cannot be
        // sealed for review. A hook has no git route, so that is a delivery it cannot make.
        if !offer.accepts_delivery.iter().any(|m| m == gateway::DELIVERY_MODE_INLINE) {
            opline!(
                "seller node hook job_id={job_id}: offer does not (or no longer) accept inline delivery; a hook cannot deliver git"
            );
            self.fail_job_with_feedback(job_id, &buyer, ReasonCode::DeliveryFailed, HOOK_DELIVERY_REFUSED_FEEDBACK, None)
                .await;
            return;
        }

        let invocation = job_id.to_owned();
        let workdir = job_workdir(self.node.home(), job_id);
        let staged = self.node.store().hook_outcome(&invocation).ok().flatten();
        let (status, answer, wall_ms) = match staged {
            // §7.1: a staged outcome is re-delivered, never re-run — its answer may already be bound.
            Some(row) => {
                opline!("seller node hook job_id={job_id}: re-delivering the staged outcome (hook not re-run)");
                (row.status, row.answer, row.wall_time_ms)
            }
            None => {
                // §5.1: a group a crashed daemon left running for THIS invocation dies before the
                // re-drive touches its directory; then the new call gets the next attempt number.
                // An earlier call that cannot be shown stopped keeps its journal entry and defers this
                // one: running the invocation again could overlap it in the same directory.
                if !reap_journaled_groups(self.node.store(), Some(&invocation)) {
                    opline!(
                        "seller node execute defer job_id={job_id}: an earlier hook call is not resolved; not re-run (retried on the next re-drive)"
                    );
                    return;
                }
                let attempt = match self.node.store().begin_hook_attempt(&invocation, job_id, now_unix()) {
                    Ok(Some(attempt)) => attempt,
                    Ok(None) => {
                        opline!(
                            "seller node execute defer job_id={job_id}: an earlier hook call is still journaled; not re-run (retried on the next re-drive)"
                        );
                        return;
                    }
                    Err(error) => {
                        opline!("seller node hook job_id={job_id}: attempt journal failed ({error})");
                        self.fail_job_with_feedback(job_id, &buyer, ReasonCode::ExecutionFailed, EXEC_FAILURE_FEEDBACK, None)
                            .await;
                        return;
                    }
                };
                let inputs = match self.prepare_hook_dir(job_id, &workdir) {
                    Ok(inputs) => inputs,
                    Err(error) => {
                        opline!("seller node hook job_id={job_id}: inputs unavailable ({error})");
                        self.fail_job_with_feedback(job_id, &buyer, ReasonCode::ExecutionFailed, EXEC_FAILURE_FEEDBACK, None)
                            .await;
                        self.finish_hook_job(job_id, &workdir);
                        return;
                    }
                };
                let deadline = u64::try_from(offer.deadline_unix).unwrap_or(0);
                let now = u64::try_from(now_unix()).unwrap_or(0);
                let event = hook::job_event(
                    &invocation,
                    attempt,
                    HOOK_ANSWER_MAX_BYTES,
                    job_id,
                    1,
                    false,
                    &offer.task,
                    offer.output.as_deref(),
                    inputs.as_deref(),
                    deadline.saturating_sub(hook::PUBLISH_MARGIN_SECS),
                );
                let cutoff = hook::job_cutoff(deadline, now, std::time::Instant::now());
                let (argv_owned, dir) = (argv.to_vec(), workdir.clone());
                let (store, inv) = (self.node.store().clone(), invocation.clone());
                opline!("seller node hook job_id={job_id}: running {label} (attempt {attempt})");
                let report = match tokio::task::spawn_blocking(move || {
                    // The group is journaled before the gate releases the hook, and forgotten once
                    // the runner has killed and reaped it (§5.1).
                    let report = hook::run_gated(&argv_owned, &event, &dir, cutoff, &mut |started| {
                        store
                            .record_hook_group(&inv, started.pgid, started.start.as_deref(), now_unix())
                            .map_err(|e| e.to_string())
                    });
                    let _ = store.clear_hook_group(&inv);
                    report
                })
                .await
                {
                    Ok(report) => report,
                    Err(error) => {
                        opline!("seller node hook job_id={job_id}: runner task failed ({error})");
                        self.fail_job_with_feedback(job_id, &buyer, ReasonCode::ExecutionFailed, EXEC_FAILURE_FEEDBACK, None)
                            .await;
                        self.finish_hook_job(job_id, &workdir);
                        return;
                    }
                };
                if !report.stderr_tail.trim().is_empty() {
                    opline!(
                        "seller node hook job_id={job_id} stderr tail: {}",
                        report.stderr_tail.trim().replace('\n', " | ")
                    );
                }
                let wall_ms = u64::try_from(report.wall.as_millis()).unwrap_or(u64::MAX);
                match hook::classify_job(&report, HOOK_ANSWER_MAX_BYTES) {
                    JobOutcome::Answer(answer) if answer.status == hook::HookStatus::Done => {
                        let row = crate::seller_node::store::HookOutcomeRow {
                            invocation: invocation.clone(),
                            job_id: job_id.to_owned(),
                            turn: 1,
                            status: answer.status.as_str().to_owned(),
                            answer: answer.answer,
                            wall_time_ms: wall_ms,
                        };
                        match self.node.store().stage_hook_outcome(&row, now_unix()) {
                            Ok(row) => (row.status, row.answer, row.wall_time_ms),
                            Err(error) => {
                                opline!("seller node hook job_id={job_id}: outcome stage failed ({error})");
                                self.fail_job_with_feedback(job_id, &buyer, ReasonCode::ExecutionFailed, EXEC_FAILURE_FEEDBACK, None)
                                    .await;
                                self.finish_hook_job(job_id, &workdir);
                                return;
                            }
                        }
                    }
                    // §5.0 rule 3: a single-turn job has no next turn to ask a question in or to
                    // continue on. No strike: the hook answered the contract.
                    JobOutcome::Answer(answer) => {
                        opline!(
                            "seller node hook job_id={job_id}: status {} on a single-turn job; nothing delivered",
                            answer.status.as_str()
                        );
                        self.hook_refused(job_id, &buyer, &workdir).await;
                        return;
                    }
                    JobOutcome::TooLarge { bytes, cap } => {
                        opline!(
                            "seller node hook job_id={job_id}: answer is {bytes} bytes, over the {cap}-byte limit — refusing rather than delivering a cut answer"
                        );
                        self.hook_refused(job_id, &buyer, &workdir).await;
                        return;
                    }
                    JobOutcome::CannotDeliver(reason) => {
                        opline!("seller node hook job_id={job_id}: hook declined to deliver ({reason})");
                        self.hook_refused(job_id, &buyer, &workdir).await;
                        return;
                    }
                    JobOutcome::OutOfTime { started } => {
                        opline!(
                            "seller node hook job_id={job_id}: {} the deadline cutoff; no strike",
                            if started { "killed at" } else { "not started: past" }
                        );
                        self.drop_harness(harness, Some(ExecutionFailure::DeadlineExceeded));
                        self.fail_job_with_feedback(job_id, &buyer, ReasonCode::ExecutionFailed, EXEC_FAILURE_FEEDBACK, None)
                            .await;
                        self.finish_hook_job(job_id, &workdir);
                        return;
                    }
                    JobOutcome::Broken(why) => {
                        opline!("seller node hook job_id={job_id}: hook broke the contract ({why})");
                        self.drop_harness(harness, Some(ExecutionFailure::Harness(Fault::Unproven)));
                        self.fail_job_with_feedback(job_id, &buyer, ReasonCode::ExecutionFailed, EXEC_FAILURE_FEEDBACK, None)
                            .await;
                        self.finish_hook_job(job_id, &workdir);
                        return;
                    }
                }
            }
        };
        debug_assert_eq!(status, "done");
        let metadata = hook_exec_metadata(label, kind, wall_ms);
        let seller_pubkey = self.seller_pubkey.to_hex();
        self.deliver_inline(job_id, offer, &seller_pubkey, stored_creq, &answer, &metadata)
            .await;
        // Delivered, or failed by `deliver_inline`: terminal, so the staged answer goes. A failure arm
        // that leaves the job live (it can still be re-delivered) keeps it (§7.1).
        self.finish_hook_job(job_id, &workdir);
    }

    /// `delivery_failed`, no strike: the hook answered honestly but nothing deliverable came of it.
    async fn hook_refused(&self, job_id: &str, buyer: &str, workdir: &std::path::Path) {
        self.fail_job_with_feedback(job_id, buyer, ReasonCode::DeliveryFailed, HOOK_DELIVERY_REFUSED_FEEDBACK, None)
            .await;
        self.finish_hook_job(job_id, workdir);
    }

    /// The end of one hook call: its per-job directory goes; its staged answer and run record go too
    /// once the job is terminal (§6, §7.1).
    fn finish_hook_job(&self, job_id: &str, workdir: &std::path::Path) {
        let _ = std::fs::remove_dir_all(workdir);
        clear_terminal_hook_state(self.node.store(), self.node.home(), job_id);
    }

    /// Create the per-job directory (0700) and, for a private job with input files, re-materialize
    /// the pinned, pre-claim-verified baseline into `<dir>/inputs` (§6). Returns the inputs path.
    fn prepare_hook_dir(
        &self,
        job_id: &str,
        workdir: &std::path::Path,
    ) -> Result<Option<std::path::PathBuf>, String> {
        std::fs::create_dir_all(workdir).map_err(|e| format!("job dir: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(workdir, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| format!("job dir permissions: {e}"))?;
        }
        let execution = crate::seller_node::privacy::execution(self.node.home(), &self.seller_pubkey.to_hex(), job_id)
            .map_err(|e| format!("private selection: {e}"))?;
        let Some((cache, oid)) = execution.and_then(|e| e.baseline) else {
            return Ok(None);
        };
        let dest = workdir.join("inputs");
        let suffix = random_nonce_hex().map_err(|e| format!("staging name: {e}"))?;
        let staging = workdir.join(format!(".inputs-{}", &suffix[..16.min(suffix.len())]));
        let result = materialize_baseline(&cache, &oid, &staging);
        if let Err(error) = result {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(error);
        }
        if dest.exists() {
            std::fs::remove_dir_all(&dest).map_err(|e| format!("previous inputs: {e}"))?;
        }
        std::fs::rename(&staging, &dest).map_err(|e| format!("inputs publish: {e}"))?;
        Ok(Some(dest))
    }
}

/// Stop hook groups a previous daemon journaled and may have left running (§5.1): every one at boot
/// (`only = None`), or just `only`'s before it is re-driven. Each is verified before any signal
/// ([`hook::reap_stale_group`]). A group shown stopped or gone has its journal entry cleared; an
/// unresolved one KEEPS it, so its invocation is not run again and its directory is kept.
/// Returns whether every group in scope was resolved (false also when the journal is unreadable).
pub(super) fn reap_journaled_groups(store: &crate::seller_node::store::SellerStore, only: Option<&str>) -> bool {
    let groups = match store.journaled_hook_groups() {
        Ok(groups) => groups,
        Err(error) => {
            opline!("seller node hook: could not read journaled hook groups ({error})");
            return false;
        }
    };
    let mut resolved = true;
    for (invocation, pgid, start) in groups {
        if only.is_some_and(|only| only != invocation) {
            continue;
        }
        match hook::reap_stale_group(pgid, start.as_deref()) {
            hook::StaleReap::Killed => {
                opline!("seller node hook: stopped the hook group {pgid} an earlier run left for {invocation}")
            }
            hook::StaleReap::Gone => {}
            hook::StaleReap::Unresolved(why) => {
                opline!(
                    "seller node hook: hook group {pgid} for {invocation} is not resolved ({why}); it is not re-run and its directory is kept until it is (check `ps -g {pgid}`)"
                );
                resolved = false;
                continue;
            }
        }
        if let Err(error) = store.clear_hook_group(&invocation) {
            opline!("seller node hook: could not clear the journal of {invocation} ({error})");
            resolved = false;
        }
    }
    resolved
}

/// Drop a job's hook state (staged answer, run record, per-job directory) if the job is terminal.
/// A live job keeps its staged answer: it may still be re-delivered (§7.1). Returns whether cleared.
pub(super) fn clear_terminal_hook_state(
    store: &crate::seller_node::store::SellerStore,
    home: &MaxplayerHome,
    job_id: &str,
) -> bool {
    use crate::seller_node::store::JobState;
    // A group that is still journaled may still run in the directory: keep everything until it is
    // resolved (§5.1).
    if !matches!(store.hook_group_journaled(job_id), Ok(false)) {
        return false;
    }
    match store.job_state(job_id) {
        Ok(Some(JobState::Delivered | JobState::Paid | JobState::Failed)) => {
            let _ = store.clear_hook_outcomes(job_id);
            let _ = std::fs::remove_dir_all(job_workdir(home, job_id));
            true
        }
        _ => false,
    }
}

/// Boot reconciliation: hook state a crash left behind for jobs that have since become terminal.
pub(super) fn reconcile_terminal_hook_state(store: &crate::seller_node::store::SellerStore, home: &MaxplayerHome) {
    let Ok(jobs) = store.hook_job_ids() else { return };
    let cleared = jobs.iter().filter(|job| clear_terminal_hook_state(store, home, job)).count();
    if cleared > 0 {
        opline!("seller node hook: cleared leftover hook state of {cleared} finished job(s)");
    }
}

/// Interval-ping bookkeeping for one hook entry (§9).
#[derive(Debug, Default, Clone)]
pub(super) struct PingTrack {
    pub last_started: Option<std::time::Instant>,
    pub in_flight: bool,
    pub consecutive_failures: u32,
}

/// Two consecutive failed interval pings fault a hook (a single blip does not).
pub(super) const INTERVAL_PING_FAILURES_TO_FAULT: u32 = 2;

impl SellerNodeRunner {
    /// A dropped hook entry's due re-probe is a `restore` ping, off the loop. A pass restores it
    /// (with the roster's backoff and strike history); a failure re-faults it.
    pub(super) fn start_hook_restore_ping(
        &self,
        harness: usize,
        argv: Vec<String>,
        roster: Arc<LiveRoster>,
    ) {
        let cwd = self.node.home().root.clone();
        tokio::task::spawn_local(async move {
            let label = roster.label(harness).unwrap_or_else(|| "<hook>".to_owned());
            let result = tokio::task::spawn_blocking(move || {
                hook::ping(&argv, &cwd, hook::PingReason::Restore)
            })
            .await
            .unwrap_or_else(|error| Err(format!("ping task failed: {error}")));
            match result {
                Ok(()) => {
                    if roster.restore_if_probing(harness) {
                        opline!("seller node hook RESTORED {label}: restore ping passed");
                    } else {
                        opline!("seller node hook {label}: restore ping passed but a newer strike landed meanwhile; staying dropped");
                    }
                }
                Err(reason) => {
                    let state = roster.fault(harness, Fault::Unproven, std::time::Instant::now());
                    opline!("seller node hook restore ping FAILED {label}: {reason} — {}", state.reason());
                }
            }
        });
    }

    /// Ping every SERVING hook entry once per heartbeat interval (whether or not beats are
    /// published), off the loop. Two consecutive failures fault it; a pass never restores (only a
    /// `restore` ping does). A result is applied only if the entry is still serving, so a stale
    /// ping cannot strike an entry a job already dropped.
    pub(super) fn start_hook_interval_pings(&self) {
        let interval = std::time::Duration::from_secs(
            crate::heartbeat::resolve_interval_secs(&self.node.home().config.seller_heartbeat).max(30),
        );
        let now = std::time::Instant::now();
        for index in 0..self.agents.entry_count() {
            let Some(entry) = self.agents.entry(index) else { continue };
            if !entry.kind.is_hook() || self.agents.unavailable(index).is_some() {
                continue;
            }
            {
                let mut pings = self.hook_pings.lock().expect("hook pings");
                let track = pings.entry(index).or_default();
                let due = track.last_started.is_none_or(|t| now.duration_since(t) >= interval);
                if track.in_flight || !due {
                    continue;
                }
                track.in_flight = true;
                track.last_started = Some(now);
            }
            let (roster, pings, cwd) = (Arc::clone(&self.agents), Arc::clone(&self.hook_pings), self.node.home().root.clone());
            let argv = entry.argv.clone();
            let label = entry.name.clone().unwrap_or_else(|| "<hook>".to_owned());
            tokio::task::spawn_local(async move {
                let result = tokio::task::spawn_blocking(move || {
                    hook::ping(&argv, &cwd, hook::PingReason::Interval)
                })
                .await
                .unwrap_or_else(|error| Err(format!("ping task failed: {error}")));
                let fault_now = {
                    let mut pings = pings.lock().expect("hook pings");
                    let track = pings.entry(index).or_default();
                    track.in_flight = false;
                    match &result {
                        Ok(()) => {
                            track.consecutive_failures = 0;
                            false
                        }
                        Err(_) => {
                            track.consecutive_failures += 1;
                            track.consecutive_failures >= INTERVAL_PING_FAILURES_TO_FAULT
                        }
                    }
                };
                if let Err(reason) = result {
                    if fault_now && roster.unavailable(index).is_none() {
                        let state = roster.fault(index, Fault::Unproven, std::time::Instant::now());
                        pings.lock().expect("hook pings").entry(index).or_default().consecutive_failures = 0;
                        opline!("seller node hook {label} DROPPED after failed interval pings: {reason} — {}", state.reason());
                    } else {
                        opline!("seller node hook {label} interval ping failed ({reason})");
                    }
                }
            });
        }
    }
}

/// The result's exec metadata for a hook: the entry label as harness, `usage_transport=hook`, the
/// configured (self-reported) model when set, and the daemon-measured wall time.
pub(super) fn hook_exec_metadata(
    label: &str,
    kind: &crate::seller_agents::AgentKind,
    wall_ms: u64,
) -> Vec<gateway::TagSpec> {
    let wall = wall_ms.to_string();
    let mut tags = vec![
        gateway::TagSpec::new(["harness", label]),
        gateway::TagSpec::new(["usage_transport", "hook"]),
        gateway::TagSpec::new(["metadata_trust", "seller-claimed"]),
        gateway::TagSpec::new(["wall_time", wall.as_str(), "ms"]),
    ];
    if let crate::seller_agents::AgentKind::Hook { model: Some(model), .. } = kind {
        tags.push(gateway::TagSpec::new(["model", model.as_str()]));
    }
    tags
}

/// Write the files of the verified `input-baseline` commit in the bare `cache` into a FRESH
/// `dest` (refused if it exists): regular blobs only, every path validated, 0700 dirs, 0600 files,
/// no exec bit, `O_NOFOLLOW` + create-new. The cache was verified against the signed manifest
/// before the claim; its objects are re-checked here.
pub(super) fn materialize_baseline(
    cache: &std::path::Path,
    oid: &str,
    dest: &std::path::Path,
) -> Result<(), String> {
    let repo = git2::Repository::open_bare(cache).map_err(|_| "input cache unavailable".to_owned())?;
    crate::private_content::repositories::check_objects(&repo).map_err(|e| e.to_string())?;
    let commit = git2::Oid::from_str(oid)
        .and_then(|oid| repo.find_commit(oid))
        .map_err(|_| "input baseline missing".to_owned())?;
    let tree = commit.tree().map_err(|_| "input tree unavailable".to_owned())?;
    make_private_dir(dest)?;
    write_tree(&repo, &tree, dest, "")
}

fn make_private_dir(path: &std::path::Path) -> Result<(), String> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(|e| format!("{}: {e}", path.display()))
}

fn write_tree(repo: &git2::Repository, tree: &git2::Tree<'_>, dir: &std::path::Path, prefix: &str) -> Result<(), String> {
    use std::io::Write;
    for entry in tree.iter() {
        let name = entry.name().map_err(|_| "non-UTF-8 input path".to_owned())?;
        let rel = if prefix.is_empty() { name.to_owned() } else { format!("{prefix}/{name}") };
        crate::private_content::validate_path(&rel).map_err(|e| format!("{rel}: {e}"))?;
        let target = dir.join(name);
        match (entry.kind(), entry.filemode()) {
            (Some(git2::ObjectType::Tree), _) => {
                make_private_dir(&target)?;
                let sub = repo.find_tree(entry.id()).map_err(|_| format!("{rel}: tree unavailable"))?;
                write_tree(repo, &sub, &target, &rel)?;
            }
            (Some(git2::ObjectType::Blob), 0o100644 | 0o100755) => {
                let blob = repo.find_blob(entry.id()).map_err(|_| format!("{rel}: blob unavailable"))?;
                let mut options = std::fs::OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
                }
                let mut file = options.open(&target).map_err(|e| format!("{rel}: {e}"))?;
                file.write_all(blob.content()).map_err(|e| format!("{rel}: {e}"))?;
            }
            _ => return Err(format!("{rel}: not a regular file")),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn baseline(entries: &[(&str, &[u8], i32)]) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().expect("tmp");
        let repo = git2::Repository::init_bare(dir.path()).expect("bare");
        let mut index = git2::Index::new().expect("index");
        for (path, bytes, mode) in entries {
            let id = repo.blob(bytes).expect("blob");
            index
                .add(&git2::IndexEntry {
                    ctime: git2::IndexTime::new(0, 0),
                    mtime: git2::IndexTime::new(0, 0),
                    dev: 0,
                    ino: 0,
                    mode: *mode as u32,
                    uid: 0,
                    gid: 0,
                    file_size: bytes.len() as u32,
                    id,
                    flags: 0,
                    flags_extended: 0,
                    path: path.as_bytes().to_vec(),
                })
                .expect("add");
        }
        let tree = repo.find_tree(index.write_tree_to(&repo).expect("tree")).expect("tree");
        let sig = git2::Signature::new("t", "t@t", &git2::Time::new(0, 0)).expect("sig");
        let oid = repo.commit(None, &sig, &sig, "b", &tree, &[]).expect("commit");
        (dir, oid.to_string())
    }

    /// codex r1 #3: the daemon died after the hook started and before it staged an answer; the
    /// hook is still running. The next boot (or the re-drive) must stop it before anything runs
    /// the invocation again, and forget it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_hook_left_running_by_a_dead_daemon_is_reaped_from_the_journal() {
        use std::os::unix::process::CommandExt;
        let dir = tempfile::tempdir().expect("tmp");
        let store = crate::seller_node::store::SellerStore::open(dir.path().join("seller.sqlite")).expect("open");
        let mut orphan = std::process::Command::new("sh")
            .args(["-c", "sleep 30"])
            .process_group(0)
            .spawn()
            .expect("spawn");
        assert_eq!(store.begin_hook_attempt("job-x", "job-x", 1).expect("begin"), Some(1));
        let start = crate::hook::process_identity(orphan.id());
        store.record_hook_group("job-x", orphan.id(), start.as_deref(), 2).expect("record");
        // Another invocation's group is not touched by a targeted reap.
        reap_journaled_groups(&store, Some("job-other"));
        assert!(orphan.try_wait().expect("try_wait").is_none());
        reap_journaled_groups(&store, None);
        let status = orphan.wait().expect("wait");
        assert!(status.code().is_none(), "the orphan was killed: {status:?}");
        assert!(store.journaled_hook_groups().expect("list").is_empty(), "its journal entry is cleared");
        assert_eq!(store.begin_hook_attempt("job-x", "job-x", 3).expect("begin"), Some(2), "the re-drive is attempt 2");
    }

    /// codex r2 #2: a journaled group whose stop cannot be established (here: its leader is gone but
    /// a process is left in the group, which can no longer be proven ours) keeps its journal entry,
    /// blocks the next attempt and keeps the job's hook state, even once the job is terminal. When
    /// the group is empty it resolves and the re-drive proceeds.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_unresolved_stale_group_blocks_the_re_drive_and_keeps_its_state() {
        use crate::seller_node::store::JobState;
        use std::os::unix::process::CommandExt;
        let dir = tempfile::tempdir().expect("tmp");
        let home = crate::home::bootstrap(dir.path()).expect("home");
        let store = crate::seller_node::store::SellerStore::open(dir.path().join("seller.sqlite")).expect("open");
        let mut leader = std::process::Command::new("sh")
            .args(["-c", "sleep 30 & exit 0"])
            .process_group(0)
            .spawn()
            .expect("spawn");
        let pgid = leader.id();
        assert_eq!(store.begin_hook_attempt("job-u", "job-u", 1).expect("begin"), Some(1));
        let identity = crate::hook::process_identity(pgid);
        store.record_hook_group("job-u", pgid, identity.as_deref(), 2).expect("record");
        let _ = leader.wait();
        let workdir = job_workdir(&home, "job-u");
        std::fs::create_dir_all(&workdir).expect("dir");

        assert!(!reap_journaled_groups(&store, Some("job-u")), "unresolved");
        assert_eq!(store.journaled_hook_groups().expect("list").len(), 1, "the journal entry is kept");
        assert_eq!(store.begin_hook_attempt("job-u", "job-u", 3).expect("begin"), None, "no new attempt while it is journaled");
        store.insert_job_for_test("job-u", JobState::Failed);
        assert!(!clear_terminal_hook_state(&store, &home, "job-u"), "terminal, but the group may still run there");
        assert!(workdir.exists());

        // SAFETY: test cleanup of the group this test created.
        unsafe { libc::kill(-(pgid as libc::pid_t), libc::SIGKILL) };
        let by = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !reap_journaled_groups(&store, Some("job-u")) && std::time::Instant::now() < by {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(store.journaled_hook_groups().expect("list").is_empty(), "resolved once the group is empty");
        assert_eq!(store.begin_hook_attempt("job-u", "job-u", 4).expect("begin"), Some(2));
        assert!(clear_terminal_hook_state(&store, &home, "job-u"));
        assert!(!workdir.exists());
    }

    /// codex r1 #12: hook state goes once the job is terminal (delivered, paid or failed, including
    /// a refusal), and is kept while the job is live and could still re-deliver its staged answer.
    /// Boot reconciliation clears what a crash left behind.
    #[test]
    fn hook_state_is_cleared_for_terminal_jobs_only_and_on_boot() {
        use crate::seller_node::store::{HookOutcomeRow, JobState};
        let dir = tempfile::tempdir().expect("tmp");
        let home = crate::home::bootstrap(dir.path()).expect("home");
        let store = crate::seller_node::store::SellerStore::open(dir.path().join("seller.sqlite")).expect("open");
        let stage = |job: &str| {
            store.begin_hook_attempt(job, job, 1).expect("begin");
            store
                .stage_hook_outcome(
                    &HookOutcomeRow {
                        invocation: job.to_owned(),
                        job_id: job.to_owned(),
                        turn: 1,
                        status: "done".into(),
                        answer: "a".into(),
                        wall_time_ms: 1,
                    },
                    1,
                )
                .expect("stage");
            std::fs::create_dir_all(job_workdir(&home, job)).expect("workdir");
        };
        for (job, state) in [
            ("job-live", JobState::Executing),
            ("job-failed", JobState::Failed),
            ("job-delivered", JobState::Delivered),
        ] {
            store.insert_job_for_test(job, state);
            stage(job);
        }
        assert!(!clear_terminal_hook_state(&store, &home, "job-live"), "a live job keeps its staged answer");
        assert!(store.hook_outcome("job-live").expect("read").is_some());
        reconcile_terminal_hook_state(&store, &home);
        assert_eq!(store.hook_job_ids().expect("ids"), vec!["job-live".to_owned()]);
        assert!(!job_workdir(&home, "job-failed").exists());
        assert!(!job_workdir(&home, "job-delivered").exists());
        assert!(job_workdir(&home, "job-live").exists());
    }

    #[test]
    fn baseline_files_land_private_and_non_executable() {
        use std::os::unix::fs::PermissionsExt;
        let (cache, oid) = baseline(&[("brief.txt", b"hello", 0o100644), ("sub/run.sh", b"#!", 0o100755)]);
        let out = tempfile::tempdir().expect("tmp");
        let dest = out.path().join("inputs");
        materialize_baseline(cache.path(), &oid, &dest).expect("materialize");
        assert_eq!(std::fs::read(dest.join("brief.txt")).unwrap(), b"hello");
        for f in ["brief.txt", "sub/run.sh"] {
            let mode = std::fs::metadata(dest.join(f)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{f}");
        }
        assert!(materialize_baseline(cache.path(), &oid, &dest).is_err(), "an existing destination is refused");
    }

    #[test]
    fn a_symlink_in_the_baseline_is_refused() {
        let (cache, oid) = baseline(&[("link", b"/etc/passwd", 0o120000)]);
        let out = tempfile::tempdir().expect("tmp");
        assert!(materialize_baseline(cache.path(), &oid, &out.path().join("inputs")).is_err());
    }

    #[test]
    fn hook_metadata_names_the_entry_and_its_configured_model() {
        let kind = crate::seller_agents::AgentKind::Hook { family: Some("claude-code".into()), model: Some("m1".into()) };
        let tags = hook_exec_metadata("my-hook", &kind, 42);
        let flat: Vec<Vec<String>> = tags.iter().map(|t| t.0.clone()).collect();
        assert!(flat.contains(&vec!["harness".into(), "my-hook".into()]));
        assert!(flat.contains(&vec!["usage_transport".into(), "hook".into()]));
        assert!(flat.contains(&vec!["model".into(), "m1".into()]));
        assert!(flat.contains(&vec!["wall_time".into(), "42".into(), "ms".into()]));
    }
}
