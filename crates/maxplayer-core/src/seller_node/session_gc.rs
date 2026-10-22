//! Session GC and parked-session measurement (docs/specs/session-jobs.md §5.2–§5.5).
//!
//! Everything docker-facing goes through the synchronous [`DockerCli`] seam the job-container
//! cleanup already uses, so the sequence (remove container, then holder, then files, then the
//! record) is checked by recording calls rather than by a live daemon. Inert while
//! `[sessions] enabled = false`: the store then holds no session rows, and the caller does not
//! even construct a [`SessionGc`].
use std::path::{Path, PathBuf};

use crate::seller_exec::{DockerCli, force_remove_argv};
use crate::session::{self, HolderObservation, SessionState};

use super::store::{SellerStore, SessionRecord, StoreError};

/// `docker container inspect --size` → `SizeRw`: the container's writable layer in bytes.
pub fn size_rw_argv(container: &str) -> Vec<String> {
    [
        "docker",
        "container",
        "inspect",
        "--size",
        "--format",
        "{{.SizeRw}}",
        container,
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// `docker inspect` of the netns holder: running flag and `State.StartedAt` (§5.5).
pub fn holder_state_argv(holder: &str) -> Vec<String> {
    [
        "docker",
        "inspect",
        "--format",
        "{{.State.Running}} {{.State.StartedAt}}",
        holder,
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// `docker stop` for a job container still running after the orchestrator's exit grace (§5.2).
pub fn stop_argv(container: &str) -> Vec<String> {
    ["docker", "stop", "--time", "10", container]
        .into_iter()
        .map(String::from)
        .collect()
}

/// Read [`holder_state_argv`]'s answer. A daemon error that names a missing container is
/// `Missing`; any other error is treated as missing too — the job container is never started
/// without proof its firewall is the one it was created into (§5.5).
pub fn parse_holder_state(answer: Result<String, String>) -> HolderObservation {
    let Ok(out) = answer else {
        return HolderObservation::Missing;
    };
    let mut parts = out.trim().splitn(2, ' ');
    match (parts.next(), parts.next()) {
        (Some("true"), Some(started)) if !started.trim().is_empty() => HolderObservation::Running {
            started_at: started.trim().to_owned(),
        },
        (Some("false"), _) => HolderObservation::NotRunning,
        _ => HolderObservation::Missing,
    }
}

pub fn observe_holder<D: DockerCli>(cli: &mut D, holder: &str) -> HolderObservation {
    parse_holder_state(cli.run(&holder_state_argv(holder)))
}

/// Bytes under `dir`, never following a symlink (a job could plant one to `/`). Unreadable
/// entries count zero; this is a measurement for the budget, not an audit.
pub fn du_bytes(dir: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&path) {
                stack.extend(entries.flatten().map(|entry| entry.path()));
            }
        } else if meta.is_file() {
            total = total.saturating_add(meta.len());
        }
    }
    total
}

/// The parked size (§5.5): `SizeRw` of the stopped container plus `du` of the host workdir. A
/// container size the daemon will not give is counted as zero and the workdir still counts.
pub fn measure_session<D: DockerCli>(cli: &mut D, container: &str, workdir: &Path) -> u64 {
    let size_rw = cli
        .run(&size_rw_argv(container))
        .ok()
        .and_then(|out| out.trim().parse::<u64>().ok())
        .unwrap_or(0);
    size_rw.saturating_add(du_bytes(workdir))
}

/// Paths a session owns on the host, removed with it (§5.3, Appendix A1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPaths {
    pub workdir: PathBuf,
    pub resolv_conf: PathBuf,
    pub exchange_dir: PathBuf,
}

impl SessionPaths {
    pub fn for_job(home_root: &Path, job_id: &str) -> Self {
        let jobs = home_root.join("seller-jobs");
        Self {
            workdir: jobs.join(job_id),
            resolv_conf: jobs.join(format!("resolv-{job_id}.conf")),
            exchange_dir: home_root.join("seller-delivery").join(job_id),
        }
    }
}

/// What one GC pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GcReport {
    pub expired_lifetime: Vec<String>,
    pub evicted_disk: Vec<String>,
    /// Removals the daemon refused; the session is still marked expired (its container can never
    /// be started again: no record, no resume) and the next expiry sweep removes the holder.
    pub removal_errors: Vec<(String, String)>,
}

/// End a session: remove the job container, then the holder (a joiner pins the namespace, so
/// the container goes first), then the files, then mark the record `state`. A running session is
/// refused here as well as by the selectors, so a GC race can never remove a live turn.
pub fn end_session<D: DockerCli>(
    cli: &mut D,
    store: &SellerStore,
    record: &SessionRecord,
    paths: &SessionPaths,
    state: SessionState,
    now_unix: u64,
) -> Result<Vec<String>, StoreError> {
    if record.state == SessionState::Running && state == SessionState::Expired {
        return Ok(vec!["refused: session is running".into()]);
    }
    let mut errors = Vec::new();
    if let Err(error) = cli.run(&force_remove_argv(&record.container)) {
        errors.push(format!("container {}: {error}", record.container));
    }
    if let Some(holder) = &record.holder
        && let Err(error) = cli.run(&force_remove_argv(holder))
    {
        errors.push(format!("holder {holder}: {error}"));
    }
    let _ = std::fs::remove_dir_all(&paths.workdir);
    let _ = std::fs::remove_file(&paths.resolv_conf);
    let _ = std::fs::remove_dir_all(&paths.exchange_dir);
    store.set_session_state(&record.job_id, state, now_unix)?;
    Ok(errors)
}

/// Configuration a GC pass needs; built by the run loop only when `[sessions] enabled`.
#[derive(Clone)]
pub struct SessionGc {
    pub store: SellerStore,
    pub home_root: PathBuf,
    pub lifetime_secs: u64,
    pub disk_budget_bytes: u64,
}

impl SessionGc {
    /// One pass (§5.5): expire parked sessions past their lifetime, then evict the oldest parked
    /// sessions while the measured total exceeds the disk budget. Never touches a running one.
    pub fn pass<D: DockerCli>(&self, cli: &mut D, now_unix: u64) -> Result<GcReport, StoreError> {
        let mut report = GcReport::default();
        let sessions = self.store.sessions()?;
        let sizes: Vec<_> = sessions
            .iter()
            .filter(|s| s.state.is_live())
            .map(SessionRecord::size)
            .collect();
        report.expired_lifetime =
            session::expired_by_lifetime(&sizes, now_unix, self.lifetime_secs);
        let remaining: Vec<_> = sizes
            .into_iter()
            .filter(|s| !report.expired_lifetime.contains(&s.job_id))
            .collect();
        report.evicted_disk = session::evict_over_budget(&remaining, self.disk_budget_bytes);
        for job in report
            .expired_lifetime
            .iter()
            .chain(report.evicted_disk.iter())
        {
            let Some(record) = sessions.iter().find(|s| &s.job_id == job) else {
                continue;
            };
            let paths = SessionPaths::for_job(&self.home_root, job);
            for error in end_session(
                cli,
                &self.store,
                record,
                &paths,
                SessionState::Expired,
                now_unix,
            )? {
                report.removal_errors.push((job.clone(), error));
            }
        }
        Ok(report)
    }

    /// The keep-set for the expiry sweep, read AFTER the pass so a just-expired session's holder
    /// is swept like any other.
    pub fn keep(&self) -> std::collections::BTreeSet<String> {
        self.store.live_session_jobs().unwrap_or_default()
    }

    /// Re-measure one parked session and record it (§5.5, after each park). Returns the bytes.
    pub fn measure_and_record<D: DockerCli>(
        &self,
        cli: &mut D,
        job_id: &str,
        container: &str,
    ) -> Result<u64, StoreError> {
        let paths = SessionPaths::for_job(&self.home_root, job_id);
        let bytes = measure_session(cli, container, &paths.workdir);
        self.store.set_session_disk_bytes(job_id, bytes)?;
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records every docker call; answers `SizeRw` with a fixed size and `rm` with success.
    struct Recording {
        calls: Vec<Vec<String>>,
        size_rw: String,
        holder: Result<String, String>,
    }
    impl Recording {
        fn new() -> Self {
            Self {
                calls: Vec::new(),
                size_rw: "1000".into(),
                holder: Ok("true 2026-10-21T10:00:00Z".into()),
            }
        }
        fn removed(&self) -> Vec<String> {
            self.calls
                .iter()
                .filter(|c| c.get(1).map(String::as_str) == Some("rm"))
                .filter_map(|c| c.last().cloned())
                .collect()
        }
    }
    impl DockerCli for Recording {
        fn run(&mut self, argv: &[String]) -> Result<String, String> {
            self.calls.push(argv.to_vec());
            match argv.get(1).map(String::as_str) {
                Some("container") => Ok(self.size_rw.clone()),
                Some("inspect") => self.holder.clone(),
                _ => Ok(String::new()),
            }
        }
    }

    fn record(job: &str, at: u64, bytes: u64, state: SessionState) -> SessionRecord {
        SessionRecord {
            job_id: job.into(),
            buyer_pubkey: "b".repeat(64),
            container: format!("maxplayer-job-{job}"),
            holder: Some(format!("maxplayer-netns-{job}")),
            holder_started_at: Some("2026-10-21T10:00:00Z".into()),
            image_id: "sha256:img".into(),
            orchestrator_version: session::SESSION_PROTOCOL.to_string(),
            acp_session_id: Some("acp".into()),
            harness: "claude".into(),
            model: None,
            turn: 1,
            last_turn_at: at,
            disk_bytes: bytes,
            state,
        }
    }

    fn gc(tag: &str) -> (SessionGc, PathBuf) {
        let root = std::env::temp_dir().join(format!("mx-session-gc-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        let store = SellerStore::open(&root.join("seller.sqlite")).expect("store");
        (
            SessionGc {
                store,
                home_root: root.clone(),
                lifetime_secs: 100,
                disk_budget_bytes: 2_500,
            },
            root,
        )
    }

    /// §5.5 lifetime + failure test 12 (disk): past its lifetime a parked session is expired
    /// (container, then holder, then files, then the record); over budget the OLDEST parked
    /// session is evicted; a running one never is, whatever its age or size.
    #[test]
    fn gc_expires_by_lifetime_and_evicts_oldest_parked_never_running() {
        let (gc, root) = gc("pass");
        for r in [
            record("old", 0, 500, SessionState::Parked),
            record("mid", 905, 1_500, SessionState::Parked),
            record("new", 950, 1_500, SessionState::Parked),
            record("run", 0, 9_000, SessionState::Running),
        ] {
            gc.store.put_session(&r).expect("put");
            let paths = SessionPaths::for_job(&root, &r.job_id);
            std::fs::create_dir_all(&paths.workdir).expect("workdir");
            std::fs::write(&paths.resolv_conf, "nameserver 1.1.1.1\n").expect("resolv");
        }
        let mut cli = Recording::new();
        let report = gc.pass(&mut cli, 1_000).expect("pass");
        assert_eq!(report.expired_lifetime, vec!["old".to_owned()]);
        // mid + new = 3000 > 2500: the older of the two goes; the running 9000 is not counted.
        assert_eq!(report.evicted_disk, vec!["mid".to_owned()]);
        assert_eq!(
            cli.removed(),
            vec![
                "maxplayer-job-old".to_owned(),
                "maxplayer-netns-old".to_owned(),
                "maxplayer-job-mid".to_owned(),
                "maxplayer-netns-mid".to_owned(),
            ],
            "the container goes before the holder it pins"
        );
        for gone in ["old", "mid"] {
            let paths = SessionPaths::for_job(&root, gone);
            assert!(!paths.workdir.exists() && !paths.resolv_conf.exists());
            assert_eq!(
                gc.store.session(gone).unwrap().unwrap().state,
                SessionState::Expired
            );
        }
        assert!(SessionPaths::for_job(&root, "run").workdir.exists());
        assert_eq!(
            gc.keep(),
            ["new".to_owned(), "run".to_owned()].into_iter().collect(),
            "the sweep keeps exactly the live sessions after the pass"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `end_session` refuses to expire a running session even when called directly.
    #[test]
    fn end_session_never_expires_a_running_turn() {
        let (gc, root) = gc("running");
        let r = record("live", 0, 0, SessionState::Running);
        gc.store.put_session(&r).expect("put");
        let mut cli = Recording::new();
        let paths = SessionPaths::for_job(&root, "live");
        let errors =
            end_session(&mut cli, &gc.store, &r, &paths, SessionState::Expired, 10).expect("end");
        assert_eq!(errors.len(), 1);
        assert!(cli.calls.is_empty(), "no docker call at all");
        assert_eq!(
            gc.store.session("live").unwrap().unwrap().state,
            SessionState::Running
        );
        // A buyer close of a parked session ends it as `closed`.
        let parked = record("p", 0, 0, SessionState::Parked);
        gc.store.put_session(&parked).expect("put");
        end_session(
            &mut cli,
            &gc.store,
            &parked,
            &SessionPaths::for_job(&root, "p"),
            SessionState::Closed,
            10,
        )
        .expect("close");
        assert_eq!(
            gc.store.session("p").unwrap().unwrap().state,
            SessionState::Closed
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The parked-size measurement: `SizeRw` plus the workdir, symlinks not followed.
    #[test]
    fn measurement_is_size_rw_plus_workdir_and_ignores_symlinks() {
        let (gc, root) = gc("measure");
        gc.store
            .put_session(&record("m", 0, 0, SessionState::Parked))
            .expect("put");
        let paths = SessionPaths::for_job(&root, "m");
        std::fs::create_dir_all(paths.workdir.join("sub")).expect("mkdir");
        std::fs::write(paths.workdir.join("a"), vec![0u8; 300]).expect("a");
        std::fs::write(paths.workdir.join("sub/b"), vec![0u8; 200]).expect("b");
        #[cfg(unix)]
        std::os::unix::fs::symlink("/", paths.workdir.join("root-link")).expect("link");
        let mut cli = Recording::new();
        let bytes = gc
            .measure_and_record(&mut cli, "m", "maxplayer-job-m")
            .expect("measure");
        assert_eq!(bytes, 1_000 + 500);
        assert_eq!(gc.store.session("m").unwrap().unwrap().disk_bytes, 1_500);
        assert_eq!(cli.calls[0], size_rw_argv("maxplayer-job-m"));
        // The daemon will not answer: the workdir still counts.
        cli.size_rw = "<no value>".into();
        assert_eq!(
            gc.measure_and_record(&mut cli, "m", "c").expect("measure"),
            500
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Failure test 6 (parse half): what `docker inspect` says about the holder.
    #[test]
    fn holder_state_parses_running_stopped_and_missing() {
        assert_eq!(
            parse_holder_state(Ok("true 2026-10-21T10:00:00.1Z\n".into())),
            HolderObservation::Running {
                started_at: "2026-10-21T10:00:00.1Z".into()
            }
        );
        assert_eq!(
            parse_holder_state(Ok("false 0001-01-01T00:00:00Z".into())),
            HolderObservation::NotRunning
        );
        assert_eq!(
            parse_holder_state(Err("Error: No such object: maxplayer-netns-x".into())),
            HolderObservation::Missing
        );
        assert_eq!(
            parse_holder_state(Ok(String::new())),
            HolderObservation::Missing
        );
        let mut cli = Recording::new();
        cli.holder = Ok("true 2026-10-22T00:00:00Z".into());
        // Restarted holder => the resume gate expires.
        assert_eq!(
            session::resume_gate(
                Some("2026-10-21T10:00:00Z"),
                &observe_holder(&mut cli, "h"),
                session::SESSION_PROTOCOL
            ),
            session::ResumeGate::Expire("holder_restarted")
        );
    }
}
