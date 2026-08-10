//! `maxplayer sandbox-probe` — does the configured launcher actually contain anything?
//!
//! An open-pool seat executes code written by strangers. Whether it is contained is a property of
//! the launcher AT RUNTIME, and every cheaper question is next to it rather than it:
//!
//!   * `[sandbox]` being present says an operator wrote a config, not that it confines.
//!   * The launcher resolving on PATH says a file exists (#357). Bubblewrap INSTALLS cleanly on
//!     Ubuntu 24.04 and then fails at spawn — `setting up uid map: Permission denied`, the AppArmor
//!     unprivileged-userns restriction — so a resolve check PASSES a sandbox that confines nothing
//!     because it never runs (#451, measured on a live seat).
//!
//! So the gate runs the launcher and reads what it did. Two legs, because one proves nothing:
//!
//!   DENY  — a canary file outside the workdir must NOT be readable from inside.
//!   ALLOW — a file inside the job workdir must be writable.
//!
//! Deny alone passes a launcher that blocks everything, including the job. Allow alone passes a
//! launcher that blocks nothing. Only the pair describes containment.
//!
//! ── The split, and why the payload never judges ──────────────────────────────────────────────────
//! This module is both halves and they are deliberately different jobs. [`run`] is the PAYLOAD: it
//! runs INSIDE the launcher and only reports facts — whether the read worked, whether the write
//! worked. [`probe_containment`] is the GATE: it runs outside, establishes that the canary is
//! readable and the workdir writable WITHOUT the launcher first (without those controls a refusal
//! could be our own broken setup), then spawns the payload through the launcher and judges.
//!
//! A payload that decided its own verdict would be a program inside the sandbox reporting on the
//! sandbox.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use maxplayer_core::seller_exec::{JobLaunch, SandboxPolicy};

const SUCCESS: i32 = 0;
const USAGE_ERROR: i32 = 1;

/// What the payload prints when it could read the canary — the sandbox did not stop it.
const CANARY_READ_OK: &str = "canary_read=ok";
/// What the payload prints when the canary read was refused.
const CANARY_READ_DENIED: &str = "canary_read=denied";
/// What the payload prints when it could write inside the workdir.
const WORKDIR_WRITE_OK: &str = "workdir_write=ok";
/// What the payload prints when the workdir write was refused.
const WORKDIR_WRITE_DENIED: &str = "workdir_write=denied";

/// How long the launcher gets to run the payload. Generous: a first bubblewrap or Landlock spawn on
/// a cold box is slower than a steady-state one, and a timeout here reads as a refusal.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// The probe's workdir, under the same `seller-jobs` root real jobs use. Dotted so it sorts out of
/// the way of job ids and reads as machinery rather than as a job.
const PROBE_WORKDIR_NAME: &str = ".sandbox-probe";
/// The canary, in the home root beside the seller key. Its NAME says what it is: a leftover from a
/// crashed probe should not look like something an operator must protect.
const CANARY_NAME: &str = ".sandbox-canary-not-a-secret";
/// The file the container payload touches to prove the workdir is writable. Relative to the session
/// cwd, which the docker launch pins to the in-container mount point.
const PROBE_WRITE_NAME: &str = ".sandbox-probe-write";

/// The verdict the boot gate acts on. Every variant that is not [`Contained`] carries the sentence
/// an operator needs to fix it — a refusal that only says "sandbox failed" sends them to the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Containment {
    /// Both legs answered as required: the canary was refused, the workdir was writable.
    Contained,
    /// The launcher ran the payload, and the payload READ the canary. Whatever this launcher is
    /// doing, it is not isolating the filesystem.
    NotContained(String),
    /// The launcher ran the payload and the payload could not write its own workdir. A job would
    /// have nothing to deliver from.
    WorkdirUnwritable(String),
    /// The launcher could not run the payload at all. Not a containment failure in itself — but a
    /// launcher that cannot spawn breaks every job the same way (#357/#358), which is why this is
    /// refused rather than warned about.
    LauncherUnusable(String),
    /// Our own setup did not hold, so the run says nothing either way. Never read as a pass.
    Inconclusive(String),
}

impl Containment {
    /// The operator-facing sentence. Empty for [`Contained`].
    pub fn detail(&self) -> &str {
        match self {
            Self::Contained => "",
            Self::NotContained(m)
            | Self::WorkdirUnwritable(m)
            | Self::LauncherUnusable(m)
            | Self::Inconclusive(m) => m,
        }
    }
}

/// Which failure direction the configured launcher's containment model implies. Derived from
/// the target platform because the launcher argv itself is operator-supplied and arbitrary
/// (#475) — this is a documented ASSUMPTION (every launcher this repo ships examples for is
/// bubblewrap-on-Linux or a seatbelt profile on macOS), not a verified property of the actual
/// launcher. An operator running an atypical launcher (e.g. a hand-rolled deny-list script on
/// Linux) gets a label that does not match their real exposure — see the #475 brief's Open
/// Questions before treating this as authoritative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainmentModel {
    /// Unnamed paths are denied by default (Landlock/bubblewrap on Linux). The probe's deny leg
    /// generalizes: an unprobed path also fails closed.
    AllowList,
    /// Unnamed paths are reachable by default (a seatbelt/sandbox-exec profile on macOS). The
    /// probe proves only the specific probed paths; completeness is not provable this way.
    DenyList,
}

impl ContainmentModel {
    /// Assumed from the target platform — see the type doc for the accuracy caveat.
    pub fn assumed_for_platform() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self::DenyList
        }
        #[cfg(not(target_os = "macos"))]
        {
            Self::AllowList
        }
    }

    /// The clause this brief's PASS/WARN messages append, naming the assumed model and what
    /// it does and does not promise.
    pub fn guarantee_clause(&self) -> &'static str {
        match self {
            Self::AllowList => "assumed allow-list for this platform: unprobed paths fail closed",
            Self::DenyList => {
                "assumed deny-list for this platform: probed paths only — unlisted paths remain reachable"
            }
        }
    }
}

// ── The payload ─────────────────────────────────────────────────────────────────────────────────

/// Help that was asked for goes to stdout and succeeds (issue #570). Distinct from the missing-args
/// `usage:` line the payload prints to stderr on a bad invocation.
fn write_usage(out: &mut dyn Write) {
    let _ = writeln!(
        out,
        "Usage:\n  maxplayer sandbox-probe --canary <path> --workdir <dir>\n\nInternal: run INSIDE the configured launcher by the seller boot gate to report what the launcher permits (canary read / workdir write). Exit codes: 0 ran, 1 usage error."
    );
}

/// `maxplayer sandbox-probe --canary <path> --workdir <dir>`, run INSIDE the launcher by the gate.
///
/// Reports and exits 0 whenever it ran, including when both legs were refused: "the sandbox blocked
/// me" is the answer the gate is asking for, not an error. A non-zero exit here means the arguments
/// were unusable, which the gate reads as inconclusive rather than as containment.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    // #570: a sole `--help` prints usage to STDOUT and exits 0, before the payload parses its flags.
    if crate::cli::is_help_request(args) {
        write_usage(out);
        return SUCCESS;
    }
    let mut canary: Option<PathBuf> = None;
    let mut workdir: Option<PathBuf> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--canary" => {
                index += 1;
                match args.get(index) {
                    Some(value) => canary = Some(PathBuf::from(value)),
                    None => {
                        let _ = writeln!(err, "--canary requires a path");
                        return USAGE_ERROR;
                    }
                }
            }
            "--workdir" => {
                index += 1;
                match args.get(index) {
                    Some(value) => workdir = Some(PathBuf::from(value)),
                    None => {
                        let _ = writeln!(err, "--workdir requires a path");
                        return USAGE_ERROR;
                    }
                }
            }
            other => {
                let _ = writeln!(err, "unknown sandbox-probe option: {other}");
                return USAGE_ERROR;
            }
        }
        index += 1;
    }

    let (Some(canary), Some(workdir)) = (canary, workdir) else {
        let _ = writeln!(
            err,
            "usage: maxplayer sandbox-probe --canary <path> --workdir <dir>"
        );
        return USAGE_ERROR;
    };

    // Read, not stat: a sandbox may leave the name visible while refusing the contents, and the
    // contents are what a stranger's code would exfiltrate.
    match std::fs::read(&canary) {
        Ok(_) => {
            let _ = writeln!(out, "{CANARY_READ_OK}");
        }
        Err(error) => {
            let _ = writeln!(out, "{CANARY_READ_DENIED} ({error})");
        }
    }

    match std::fs::write(workdir.join(".maxplayer-sandbox-probe"), b"probe\n") {
        Ok(()) => {
            let _ = writeln!(out, "{WORKDIR_WRITE_OK}");
        }
        Err(error) => {
            let _ = writeln!(out, "{WORKDIR_WRITE_DENIED} ({error})");
        }
    }

    SUCCESS
}

// ── The gate ────────────────────────────────────────────────────────────────────────────────────

/// Judge a payload run. Separated from the spawning so the decision table is testable without a
/// launcher, a filesystem or a process — every branch below is a way a boot gate could wave through
/// an uncontained seat.
pub fn verdict_from_payload(stdout: &str) -> Containment {
    let read_denied = stdout.contains(CANARY_READ_DENIED);
    let read_ok = stdout.contains(CANARY_READ_OK);
    let write_ok = stdout.contains(WORKDIR_WRITE_OK);
    let write_denied = stdout.contains(WORKDIR_WRITE_DENIED);

    // Neither leg reported: the payload did not run, or ran something that is not our payload.
    if !(read_denied || read_ok) || !(write_ok || write_denied) {
        return Containment::LauncherUnusable(format!(
            "the launcher produced no probe result — it did not run `maxplayer sandbox-probe`, or \
             its output was swallowed. Output: {}",
            summarize(stdout)
        ));
    }
    if read_ok {
        return Containment::NotContained(
            "a file outside the job workdir was READABLE from inside the launcher — this launcher \
             is not isolating the filesystem, so a stranger's job can read this box's secrets"
                .to_owned(),
        );
    }
    if write_denied {
        return Containment::WorkdirUnwritable(
            "the job workdir was NOT writable inside the launcher — the sandbox is too tight for a \
             job to produce anything to deliver"
                .to_owned(),
        );
    }
    Containment::Contained
}

/// Compress launcher output for an error line: enough to recognise, never enough to bury the
/// message it is attached to.
fn summarize(output: &str) -> String {
    let flat: String = output.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.is_empty() {
        "<nothing>".to_owned()
    } else if flat.len() > 300 {
        format!("{}…", &flat[..300])
    } else {
        flat
    }
}

/// Run the two-leg probe through `policy` and judge it.
///
/// The controls come first and are not optional. A canary that was never written, or a workdir that
/// is not writable by THIS process, would make the sandboxed run refuse for reasons that have
/// nothing to do with the sandbox — a refusal attributable to our own setup, reported as a security
/// finding. Establish both properties without the launcher, then measure what changes with it.
pub fn probe_containment(policy: &SandboxPolicy, home_root: &Path) -> Containment {
    if policy.is_passthrough() {
        return Containment::NotContained(
            "no [sandbox] launcher is configured — the agent runs directly on this box".to_owned(),
        );
    }

    // ★ The paths are the seat's REAL ones, and that is the whole design. A launcher is configured
    //   for where jobs run — `$MAXPLAYER_HOME/seller-jobs/<job_id>` — so a probe in a scratch directory
    //   under /tmp would be denied by a perfectly good sandbox that simply never heard of it, and
    //   refuse a seat that was correctly configured. Measured while building this: a bubblewrap
    //   launcher binding only the job tree fails a /tmp probe outright.
    //
    //   The canary sits in the home ROOT, beside the seller key, because that is the class of file
    //   this gate exists to keep a stranger's job away from. The key itself is never read: a file
    //   next to it answers the same question — is the seat's private tree reachable from inside the
    //   launcher — without a secret passing through the probe at all.
    let workdir = home_root.join("seller-jobs").join(PROBE_WORKDIR_NAME);
    let canary = home_root.join(CANARY_NAME);

    let setup = (|| -> std::io::Result<()> {
        std::fs::create_dir_all(&workdir)?;
        std::fs::write(
            &canary,
            b"maxplayer sandbox probe canary; not a secret. See sandbox_probe.rs\n",
        )?;
        Ok(())
    })();
    if let Err(error) = setup {
        cleanup(&workdir, &canary);
        return Containment::Inconclusive(format!("cannot lay out the probe files ({error})"));
    }

    // Control one: the canary IS readable without the launcher. Without this, "denied" could mean
    // the file was never there.
    if std::fs::read(&canary).is_err() {
        cleanup(&workdir, &canary);
        return Containment::Inconclusive(
            "the canary is not readable even outside the launcher, so a refusal inside it would \
             prove nothing"
                .to_owned(),
        );
    }
    // Control two: the workdir IS writable without the launcher.
    let control_write = workdir.join(".control");
    if std::fs::write(&control_write, b"control\n").is_err() {
        cleanup(&workdir, &canary);
        return Containment::Inconclusive(
            "the probe workdir is not writable even outside the launcher, so a refusal inside it \
             would prove nothing"
                .to_owned(),
        );
    }
    let _ = std::fs::remove_file(&control_write);

    // Both executors answer the SAME question — is the seat's private tree reachable from inside a
    // job — but they can only be asked it in their own shape, so the payload differs and the verdict
    // does not. Both emit the same markers, and `verdict_from_payload` judges both without knowing
    // which produced them.
    let verdict = match policy.docker_image() {
        Some(_) => run_in_container(policy, &canary, &workdir),
        None => run_under_launcher(policy, &canary, &workdir),
    };

    cleanup(&workdir, &canary);
    verdict
}

/// The launcher probe: spawn OUR binary under the launcher and let it report what it could reach.
///
/// A launcher RESTRICTS a filesystem it shares with us, so the canary is genuinely present inside
/// and a pass means the launcher actively DENIED the read.
fn run_under_launcher(policy: &SandboxPolicy, canary: &Path, workdir: &Path) -> Containment {
    let exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            return Containment::Inconclusive(format!(
                "cannot locate this executable to run the probe with ({error})"
            ))
        }
    };
    let payload: Vec<String> = vec![
        exe.to_string_lossy().into_owned(),
        "sandbox-probe".to_owned(),
        "--canary".to_owned(),
        canary.to_string_lossy().into_owned(),
        "--workdir".to_owned(),
        workdir.to_string_lossy().into_owned(),
    ];
    // Non-docker on this arm, so the policy always yields a host argv.
    let argv = policy
        .wrap(&payload)
        .expect("a launcher/pass-through policy always wraps into a host argv");

    // Measured while building this: a bubblewrap launcher that binds only the agent's paths cannot
    // execute OUR binary, and the run fails with ENOENT that reads like a missing launcher. The
    // remedy is a bind, so the message has to name the path that needs binding — otherwise the
    // operator debugs the wrong end of a correctly-configured sandbox.
    match spawn_and_read(&argv) {
        Containment::LauncherUnusable(detail) => Containment::LauncherUnusable(format!(
            "{detail}. The probe runs `{} sandbox-probe`, so the launcher must be able to execute \
             that path — a launcher that binds only the agent's paths will not see it",
            exe.display()
        )),
        other => other,
    }
}

/// The container probe: run the IMAGE'S OWN SHELL and let it report the same two legs.
///
/// A container SUBSTITUTES the filesystem rather than restricting it, so the canary is absent unless
/// someone mounted the home in — and "absent" is the honest containment answer here, the way
/// "denied" is under a launcher. The leg still earns its keep: it goes red if a mount is ever added
/// that exposes the seat's tree, which is the regression a gate exists to catch.
///
/// The payload is `sh`, not `maxplayer sandbox-probe`: this binary is not in the sandbox image, and
/// bind-mounting it would demand the host's libc match the image's — impossible when the seller runs
/// on darwin. It prints the SAME markers the Rust payload does, so the judgment (including "neither
/// leg reported ⇒ the payload never ran") is shared rather than duplicated.
///
/// The launch comes from [`SandboxPolicy::launch`] — the very call the awarded-job path uses — so
/// this measures the operator's REAL image under the REAL mount and uid flags, never a stand-in.
fn run_in_container(policy: &SandboxPolicy, canary: &Path, workdir: &Path) -> Containment {
    let Some((uid, gid)) = owner_of(workdir) else {
        return Containment::Inconclusive(
            "cannot read the probe workdir's owner, so the container could not be run as the uid an \
             awarded job would get"
                .to_owned(),
        );
    };

    let payload = container_payload(canary);

    let job = JobLaunch {
        workdir,
        env: &[],
        uid,
        gid,
    };
    let launch = match policy.launch(&payload, &job) {
        Ok(launch) => launch,
        Err(error) => {
            return Containment::Inconclusive(format!(
                "could not build the container launch for the probe ({error})"
            ))
        }
    };
    let mut argv = Vec::with_capacity(launch.args.len() + 1);
    argv.push(launch.program);
    argv.extend(launch.args);
    spawn_and_read(&argv)
}

/// The shell payload the container runs: the two legs, in the image's own `sh`, printing the same
/// markers the Rust payload prints.
///
/// The canary path arrives as an ARGUMENT (`$1`) and is never interpolated into the script, so a
/// home path carrying a space or a quote cannot become shell syntax. The write leg is relative to
/// the session cwd, which the docker launch pins to the in-container mount point — so the payload
/// never has to know the host path or the mount point's name.
fn container_payload(canary: &Path) -> Vec<String> {
    let script = format!(
        "if [ -r \"$1\" ]; then echo {CANARY_READ_OK}; else echo {CANARY_READ_DENIED}; fi; \
         if touch ./{PROBE_WRITE_NAME} 2>/dev/null; then echo {WORKDIR_WRITE_OK}; \
         rm -f ./{PROBE_WRITE_NAME}; else echo {WORKDIR_WRITE_DENIED}; fi"
    );
    vec![
        "sh".to_owned(),
        "-c".to_owned(),
        script,
        // $0, then $1 — the canary path the script tests.
        "maxplayer-sandbox-probe".to_owned(),
        canary.to_string_lossy().into_owned(),
    ]
}

/// The uid/gid owning `path`. The probe workdir was just created by THIS process, so its owner is
/// the uid an awarded job's container would be run as — read from the filesystem rather than pulled
/// in through a `libc` dependency this crate does not otherwise carry.
fn owner_of(path: &Path) -> Option<(u32, u32)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).ok().map(|m| (m.uid(), m.gid()))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Spawn `argv` with a deadline and turn the result into a verdict. A launcher that hangs is as
/// unusable as one that cannot start — both leave every job stuck — so the timeout is a refusal
/// with its own sentence rather than a wait.
fn spawn_and_read(argv: &[String]) -> Containment {
    let Some((program, args)) = argv.split_first() else {
        return Containment::Inconclusive("empty launcher argv".to_owned());
    };

    let mut child = match Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return Containment::LauncherUnusable(format!(
                "the launcher '{program}' could not be started ({error}) — every awarded job would \
                 die the same way at spawn"
            ))
        }
    };

    let deadline = std::time::Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Containment::LauncherUnusable(format!(
                        "the launcher '{program}' did not finish the probe within {}s — a job under \
                         it would hang the same way",
                        PROBE_TIMEOUT.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => {
                return Containment::Inconclusive(format!("could not wait for the launcher ({error})"))
            }
        }
    }

    let output = match child.wait_with_output() {
        Ok(output) => output,
        Err(error) => {
            return Containment::Inconclusive(format!("could not read the probe's output ({error})"))
        }
    };

    // stderr joins stdout: a launcher may merge or reorder streams, and the payload's lines are
    // matched by content rather than by which pipe carried them. The launcher's own diagnostics
    // (`setting up uid map: Permission denied`) are the useful part of an unusable-launcher report.
    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    combined.push('\n');
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    verdict_from_payload(&combined)
}

/// Leave nothing behind in the seat's home. Best-effort on purpose: a probe that failed to clean up
/// must not turn into a boot failure of its own, and both names are fixed so a crashed run's
/// leftovers are recognisable rather than mysterious.
fn cleanup(workdir: &Path, canary: &Path) {
    let _ = std::fs::remove_dir_all(workdir);
    let _ = std::fs::remove_file(canary);
}

/// Whether a seat serving the OPEN POOL may boot, given what the probe found and whether the
/// operator asked for the unsafe path. Pure, because this is the decision the whole check exists to
/// make and it must be testable without a sandbox.
///
/// Targeted-only seats are advisory: they run work from counterparties they chose, which is a
/// different risk than executing whatever the open market posts.
pub fn open_pool_admission(
    claims_open_pool: bool,
    containment: &Containment,
    unsafe_override: bool,
) -> Result<(), String> {
    if !claims_open_pool {
        return Ok(());
    }
    if unsafe_override {
        return Ok(());
    }
    match containment {
        Containment::Contained => Ok(()),
        other => Err(other.detail().to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory for the PAYLOAD tests only. The gate uses the seat's home; the payload
    /// takes whatever paths it is handed, so exercising it needs somewhere disposable.
    fn scratch() -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "maxplayer-probe-test-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("scratch dir");
        path
    }

    #[test]
    fn payload_reports_both_legs_and_exits_zero_even_when_denied() {
        let root = scratch();
        let workdir = root.join("workdir");
        std::fs::create_dir_all(&workdir).expect("workdir");
        let canary = root.join("absent");

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(
            &[
                "--canary".into(),
                canary.to_string_lossy().into_owned(),
                "--workdir".into(),
                workdir.to_string_lossy().into_owned(),
            ],
            &mut out,
            &mut err,
        );
        let text = String::from_utf8(out).expect("utf8");

        // Exit 0 with a denial: "the sandbox blocked me" is the answer, not an error. A payload that
        // exited non-zero here would be indistinguishable from a launcher that could not run it.
        assert_eq!(code, SUCCESS);
        assert!(text.contains(CANARY_READ_DENIED), "{text}");
        assert!(text.contains(WORKDIR_WRITE_OK), "{text}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn payload_reads_a_readable_canary() {
        let root = scratch();
        let workdir = root.join("workdir");
        std::fs::create_dir_all(&workdir).expect("workdir");
        let canary = root.join("canary");
        std::fs::write(&canary, b"secret").expect("canary");

        let mut out = Vec::new();
        let mut err = Vec::new();
        run(
            &[
                "--canary".into(),
                canary.to_string_lossy().into_owned(),
                "--workdir".into(),
                workdir.to_string_lossy().into_owned(),
            ],
            &mut out,
            &mut err,
        );
        assert!(String::from_utf8(out).expect("utf8").contains(CANARY_READ_OK));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn payload_refuses_missing_arguments() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(run(&[], &mut out, &mut err), USAGE_ERROR);
        assert!(String::from_utf8(err).expect("utf8").contains("usage:"));
    }

    // ── The decision table ──────────────────────────────────────────────────────────────────────

    #[test]
    fn a_readable_canary_is_not_containment() {
        let verdict = verdict_from_payload("canary_read=ok\nworkdir_write=ok\n");
        assert!(matches!(verdict, Containment::NotContained(_)), "{verdict:?}");
    }

    #[test]
    fn both_legs_as_required_is_containment() {
        let verdict = verdict_from_payload("canary_read=denied (permission denied)\nworkdir_write=ok\n");
        assert_eq!(verdict, Containment::Contained);
    }

    #[test]
    fn a_sandbox_too_tight_to_write_is_refused_on_its_own_terms() {
        let verdict = verdict_from_payload("canary_read=denied\nworkdir_write=denied (read-only)\n");
        assert!(matches!(verdict, Containment::WorkdirUnwritable(_)), "{verdict:?}");
    }

    // The motivating live failure: bubblewrap resolves, then dies at spawn. No probe line is
    // printed, and a gate that read "no canary_read=ok" as containment would PASS it.
    #[test]
    fn a_launcher_that_never_ran_the_payload_is_not_containment() {
        let verdict = verdict_from_payload("bwrap: setting up uid map: Permission denied\n");
        assert!(matches!(verdict, Containment::LauncherUnusable(_)), "{verdict:?}");
        assert!(verdict.detail().contains("uid map"), "the launcher's own diagnostic must survive: {verdict:?}");
    }

    #[test]
    fn half_an_answer_is_no_answer() {
        // Only the read leg reported: the payload died between the two writes, so nothing is known
        // about the workdir.
        let verdict = verdict_from_payload("canary_read=denied\n");
        assert!(matches!(verdict, Containment::LauncherUnusable(_)), "{verdict:?}");
    }

    // ── Admission ───────────────────────────────────────────────────────────────────────────────

    #[test]
    fn an_open_pool_seat_needs_containment() {
        let refused = open_pool_admission(true, &Containment::NotContained("x".into()), false);
        assert!(refused.is_err());
        assert!(open_pool_admission(true, &Containment::Contained, false).is_ok());
    }

    #[test]
    fn a_targeted_only_seat_is_advisory() {
        // The same failing probe, and this seat boots: it runs work from counterparties it chose.
        assert!(open_pool_admission(false, &Containment::NotContained("x".into()), false).is_ok());
    }

    #[test]
    fn the_override_is_the_only_way_past_a_failed_probe() {
        assert!(open_pool_admission(true, &Containment::NotContained("x".into()), true).is_ok());
        assert!(open_pool_admission(true, &Containment::LauncherUnusable("x".into()), true).is_ok());
    }

    // Every non-Contained variant must refuse an open-pool seat. Enumerated rather than sampled:
    // a new variant added without a decision here would default to whatever the match arm allows.
    #[test]
    fn every_failing_verdict_refuses_an_open_pool_seat() {
        for verdict in [
            Containment::NotContained("a".into()),
            Containment::WorkdirUnwritable("b".into()),
            Containment::LauncherUnusable("c".into()),
            Containment::Inconclusive("d".into()),
        ] {
            assert!(
                open_pool_admission(true, &verdict, false).is_err(),
                "{verdict:?} let an open-pool seat boot"
            );
        }
    }

    #[test]
    fn a_passthrough_policy_is_reported_as_uncontained_without_spawning_anything() {
        let root = scratch();
        let verdict = probe_containment(&SandboxPolicy::passthrough(), &root);
        let _ = std::fs::remove_dir_all(&root);
        assert!(matches!(verdict, Containment::NotContained(_)), "{verdict:?}");
        assert!(verdict.detail().contains("no [sandbox] launcher"), "{verdict:?}");
    }

    // ── the container payload ───────────────────────────────────────────────────────────────────
    // The shell payload is judged by the SAME `verdict_from_payload` as the Rust one, so what has to
    // be pinned is that its output actually speaks that contract. Run it under the host's own `sh`:
    // no docker, so it runs in CI, and it isolates the script's grammar and markers from anything
    // the container adds. Both directions, because a script that always printed `denied` would pass
    // a one-sided test while proving nothing.
    fn run_payload_on_host(canary: &Path, cwd: &Path) -> String {
        let payload = super::container_payload(canary);
        let output = Command::new(&payload[0])
            .args(&payload[1..])
            .current_dir(cwd)
            .output()
            .expect("run the container payload under the host shell");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    #[test]
    fn the_container_payload_speaks_the_shared_verdict_contract() {
        let root = scratch();
        let workdir = root.join("work");
        std::fs::create_dir_all(&workdir).expect("mk workdir");
        let canary = root.join(CANARY_NAME);

        // Canary REACHABLE (as it would be if someone mounted the home in) ⇒ not contained.
        std::fs::write(&canary, b"canary\n").expect("write canary");
        let reachable = run_payload_on_host(&canary, &workdir);
        assert!(
            matches!(verdict_from_payload(&reachable), Containment::NotContained(_)),
            "a readable canary must read as NotContained, got: {reachable:?}"
        );

        // Canary ABSENT (the container's normal state — the home was never mounted) ⇒ contained,
        // and the write leg still had to succeed for that verdict to be reachable.
        std::fs::remove_file(&canary).expect("rm canary");
        let absent = run_payload_on_host(&canary, &workdir);
        assert_eq!(
            verdict_from_payload(&absent),
            Containment::Contained,
            "an absent canary plus a writable workdir is containment, got: {absent:?}"
        );

        // The write leg is REAL: a workdir the payload cannot write must not read as contained.
        let readonly = root.join("readonly");
        std::fs::create_dir_all(&readonly).expect("mk readonly");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o500))
                .expect("chmod 0500");
            // Root ignores the mode bits, so this leg would false-pass when tests run as root.
            if super::owner_of(&readonly).map(|(uid, _)| uid) != Some(0) {
                let denied = run_payload_on_host(&canary, &readonly);
                assert!(
                    matches!(verdict_from_payload(&denied), Containment::WorkdirUnwritable(_)),
                    "an unwritable workdir must not read as contained, got: {denied:?}"
                );
            }
            let _ = std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o700));
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    // End-to-end against a REAL container, opt-in via `MAXPLAYER_SANDBOX_PROBE_DOCKER_E2E=1` because
    // it needs a docker daemon and an image with a shell. This is the one that proves the whole
    // claim: an image mounting only the job workdir is CONTAINED — the seat's home is unreachable
    // not because a rule denied it, but because it was never mounted.
    #[test]
    fn docker_probe_reports_containment_against_a_real_container() {
        if std::env::var("MAXPLAYER_SANDBOX_PROBE_DOCKER_E2E").as_deref() != Ok("1") {
            return;
        }
        let image = std::env::var("MAXPLAYER_SANDBOX_PROBE_E2E_IMAGE")
            .unwrap_or_else(|_| "alpine:latest".to_owned());
        let root = scratch();
        std::fs::create_dir_all(&root).expect("mk home root");

        // Built through the real config route, so the test exercises the same resolution an
        // operator's `[sandbox]` section goes through — not a hand-made policy.
        let policy = SandboxPolicy::from_config(Some(&maxplayer_core::home::SandboxConfig {
            mode: maxplayer_core::home::SandboxMode::Docker,
            launcher: Vec::new(),
            image: Some(image),
            forward_env: Vec::new(),
        }))
        .expect("mode=docker with an image resolves");
        let verdict = probe_containment(&policy, &root);
        let _ = std::fs::remove_dir_all(&root);

        assert_eq!(
            verdict,
            Containment::Contained,
            "a container mounting only the job workdir must prove containment; got {verdict:?}"
        );
    }
}
