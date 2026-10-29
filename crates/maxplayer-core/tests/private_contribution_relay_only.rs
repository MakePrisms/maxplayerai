//! Field report (Petar, v0.6.2-rc2): a paid, targeted, PRIVATE contribution job whose
//! `target_repo_url` was a private GitHub repository delivered turn 1 fine, but every buyer
//! `collect` failed before paying. The verify path re-fetched `base_oid` from the external URL
//! with an anonymous transport (NIP-98 goes only to relay Git), so a private source was always
//! refused, turn 1 stayed unpaid and the session could not continue.
//!
//! The rule: from the moment a private job is posted, the job's repository IS the relay copy the
//! buyer uploaded. No seller, reviewer, buyer verify/collect or follow-up turn reads the external
//! source again.
//!
//! The "GitHub" here is an auth-gated HTTPS fixture that refuses every anonymous request and
//! RECORDS every request it sees. The test asserts it receives none after posting, and drives:
//!
//! * A — a REAL private post (`base_local_path`), then a paid collect that must clear every
//!   delivery gate and stop only at the (dead, loopback) mint: zero spend, but verification
//!   passed. Also checks the new wire: the task names the job repository, not the source.
//! * C — the job-repository proof: a base only a seller delivery ref points at is refused; the
//!   buyer's own `input/` ref proves it.
//! * B — the LEGACY wire (an offer published before this fix, whose encrypted task still names
//!   the external URL — Petar's job): free turn 1 collects and materializes, then turn 2 (a
//!   session follow-up whose commit has turn 1 as its parent) collects too.
//!
//! Red on `origin/main`: collect fails with `fetch-base` (the fixture's 401) and the GitHub
//! fixture records the anonymous probe.
#![cfg(all(unix, feature = "wallet"))]

#[allow(dead_code)]
mod git_http_fixture;

use std::path::{Path, PathBuf};
use std::process::Command;

use git_http_fixture::{FixtureOptions, GitHttpAuthServer};
use maxplayer_core::budget::BudgetGate;
use maxplayer_core::collect::{CollectRequest, collect_async, collect_turn_async};
use maxplayer_core::contribution::ContentPolicy;
use maxplayer_core::delivery::{CommitOid, DeliveryError, GitDelivery};
use maxplayer_core::delivery_git::{ContributionBaseSource, PayPathDeliveryVerifier, RelayJobRepo};
use maxplayer_core::{
    contribution, gateway, home, job_lifecycle as jl, private_content as pc, receipt,
};
use nostr_sdk::secp256k1::Message;
use nostr_sdk::{Event, Keys};

const DEAD_MINT: &str = "https://127.0.0.1:1";

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t.invalid")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn commit_file(work: &Path, path: &str, text: &str) -> String {
    std::fs::write(work.join(path), text).unwrap();
    git(work, &["add", "-A"]);
    git(work, &["commit", "-qm", path]);
    git(work, &["rev-parse", "HEAD"])
}

fn tag<'a>(event: &'a Event, name: &str) -> &'a str {
    event
        .tags
        .iter()
        .map(|t| t.as_slice())
        .find(|t| t.first().map(String::as_str) == Some(name))
        .and_then(|t| t.get(1))
        .map(String::as_str)
        .expect("tag")
}

fn job_repo_server(root: &Path, name: &str) -> (PathBuf, GitHttpAuthServer) {
    let repo = root.join(format!("{name}.git"));
    git(root, &["init", "-q", "--bare", repo.to_str().unwrap()]);
    let server = GitHttpAuthServer::spawn_with(
        &repo,
        "/git/buyer/job",
        FixtureOptions {
            private_job_host: true,
            ..Default::default()
        },
    );
    (repo, server)
}

fn buyer_home(
    root: &Path,
    name: &str,
    server: &GitHttpAuthServer,
    service: &str,
) -> home::MaxplayerHome {
    let mut home = home::bootstrap(root.join(name)).unwrap();
    // Nostr publication is outside this fixture: queue durably, fail connection setup at once.
    home.config.relay_url = "not a relay URL".into();
    home.config.privacy.private_jobs = true;
    home.config.privacy.private_content_v2 = true;
    home.config.privacy.private_job_repos = true;
    home.config.privacy.service_pubkey = Some(service.to_owned());
    home.config.privacy.git_base = Some(git_base(server));
    // The dead loopback mint: refuses the connect instantly, opts in to no real money.
    home.config.allow_real_mints = true;
    home
}

fn git_base(server: &GitHttpAuthServer) -> String {
    format!("{}/git/", server.repo_url().split("/git/").next().unwrap())
}

/// Everything the seller publishes for one delivered turn, already projected exactly as a v2
/// seat projects it, plus the accept-bind the buyer would record for it.
struct Trade<'a> {
    buyer: &'a Keys,
    seller: &'a Keys,
    host: &'a pc::wire::HostPolicy,
    offer: &'a Event,
    task_envelope: String,
    task_pin: pc::Contribution,
    job_repo: String,
    claim: Event,
    award: Event,
    paid: bool,
}

impl<'a> Trade<'a> {
    fn new(
        buyer: &'a Keys,
        seller: &'a Keys,
        host: &'a pc::wire::HostPolicy,
        offer: &'a Event,
        task: &pc::PreparedContent,
        paid: bool,
    ) -> Self {
        let offer_id = offer.id.to_hex();
        let (buyer_hex, seller_hex) = (buyer.public_key().to_hex(), seller.public_key().to_hex());
        let creq = paid.then(|| {
            gateway::creq::build_seller_creq(&offer_id, 10, "sat", &[DEAD_MINT.into()], &seller_hex)
                .unwrap()
        });
        let claim = pc::builders::sign(
            seller,
            pc::carriers::project(
                offer,
                &gateway::claim_draft(
                    &offer_id,
                    &buyer_hex,
                    &seller_hex,
                    creq.as_deref()
                        .map(gateway::ClaimPayment::Sat)
                        .unwrap_or(gateway::ClaimPayment::None),
                    &["codex".to_string()],
                    &Default::default(),
                ),
                None,
                None,
                host,
            )
            .unwrap(),
        )
        .unwrap();
        let award = pc::builders::sign(
            buyer,
            pc::carriers::project(
                offer,
                &gateway::award_draft(&offer_id, &claim.id.to_hex(), &buyer_hex, &seller_hex),
                None,
                None,
                host,
            )
            .unwrap(),
        )
        .unwrap();
        let job_repo = host.job_repo(&buyer_hex, tag(offer, "job")).unwrap();
        Trade {
            buyer,
            seller,
            host,
            offer,
            task_envelope: task.envelope().to_owned(),
            task_pin: task.body().contribution.clone().expect("contribution task"),
            job_repo,
            claim,
            award,
            paid,
        }
    }

    /// The seller's signed result for `commit` on `branch` (a session turn when `followup`),
    /// echoing the task's pin and signing the authorship tuple over it, as `run.rs` does.
    fn bind(&self, branch: &str, commit: &str, followup: Option<&str>) -> jl::AcceptedBind {
        let offer_id = self.offer.id.to_hex();
        let (buyer_hex, seller_hex) = (
            self.buyer.public_key().to_hex(),
            self.seller.public_key().to_hex(),
        );
        let amount = if self.paid { 10 } else { 0 };
        let claim_creq = self
            .claim
            .tags
            .iter()
            .map(|t| t.as_slice())
            .find(|t| t.first().map(String::as_str) == Some("creq"))
            .map(|t| t[1].clone());
        let job_hash = pc::job_hash(&offer_id).unwrap();
        let creq_hash = claim_creq.as_deref().map(gateway::creq_hash_hex);
        let preimage = receipt::ReceiptPreimage {
            protocol: receipt::ReceiptProtocol::V2,
            job_hash: job_hash.clone(),
            offer_id: offer_id.clone(),
            amount,
            unit: "sat".into(),
            buyer_pubkey: buyer_hex.clone(),
            seller_pubkey: seller_hex.clone(),
            delivery_integrity_hash: commit.into(),
            delivery_kind: "fork".into(),
            exec_metadata_commitment: "none".into(),
            creq_hash: creq_hash.clone(),
        };
        let sig = self
            .seller
            .sign_schnorr(&Message::from_digest(preimage.digest_bytes()))
            .to_string();
        let mut draft = gateway::git_result_draft(
            &offer_id,
            &buyer_hex,
            "git",
            &self.job_repo,
            branch,
            commit,
            amount,
            &job_hash,
            &sig,
            "",
            &[],
        );
        if let Some(followup) = followup {
            maxplayer_core::session::append_result_tags(
                &mut draft,
                Some(followup),
                maxplayer_core::session::TurnStatus::Done,
                false,
                None,
            );
        }
        let pin = &self.task_pin;
        let target =
            contribution::TargetRepoPin::new(&pin.target_owner_pubkey, &pin.target_clone_url)
                .unwrap();
        let tuple = contribution::AuthorshipTuple {
            job_id: offer_id.clone(),
            seller_pubkey: seller_hex.clone(),
            target: target.clone(),
            base_oid: pin.base_oid.clone(),
            fork: contribution::ForkRef::new(&self.job_repo, branch).unwrap(),
            commit_oid: commit.into(),
        };
        let tuple_sig = contribution::sign_authorship_tuple(self.seller, &tuple);
        draft.tags.extend(contribution::contribution_result_tags(
            &contribution::ContributionOffer {
                target,
                base: contribution::ContributionBase::new(&pin.base_branch, &pin.base_oid).unwrap(),
                accepts: vec!["fork".into()],
            },
            &tuple_sig,
        ));
        let result = pc::builders::sign(
            self.seller,
            pc::carriers::project(self.offer, &draft, Some(&self.award), None, self.host).unwrap(),
        )
        .unwrap();
        let accepted_mints: Vec<String> = claim_creq
            .as_deref()
            .map(|c| {
                gateway::creq::parse_creq(c)
                    .unwrap()
                    .mints
                    .iter()
                    .map(ToString::to_string)
                    .collect()
            })
            .unwrap_or_default();
        jl::AcceptedBind {
            private_evidence: Some(pc::evidence::PrivateEvidence {
                offer: self.offer.clone(),
                claim: self.claim.clone(),
                award: self.award.clone(),
                result: result.clone(),
                task_envelope: Some(self.task_envelope.clone()),
                answer_envelope: None,
            }),
            job_id: offer_id,
            claim_id: self.claim.id.to_hex(),
            result_id: result.id.to_hex(),
            seller_pubkey: seller_hex,
            commit_oid: commit.into(),
            repo: self.job_repo.clone(),
            branch: branch.into(),
            delivery_kind: Some("fork".into()),
            inline_answer: None,
            job_hash,
            amount_sats: amount,
            accept_event_id: "f".repeat(64),
            accepted_at: 1,
            seller_signature: sig,
            creq_hash,
            funding_mint: accepted_mints.first().cloned(),
            accepted_mints,
            delivery_mint: None,
            agent_used: None,
            model_used: None,
            contribution: Some(jl::AcceptedContribution {
                target_owner_pubkey: pin.target_owner_pubkey.clone(),
                target_clone_url: pin.target_clone_url.clone(),
                base_branch: pin.base_branch.clone(),
                base_oid: pin.base_oid.clone(),
                tuple_signature: tuple_sig,
                store_ref: PayPathDeliveryVerifier::store_ref_for(commit),
            }),
            payment_mode: if self.paid {
                gateway::PaymentMode::Sat
            } else {
                gateway::PaymentMode::None
            },
            followup_id: followup.map(str::to_owned),
        }
    }
}

fn write_bind(home: &home::MaxplayerHome, bind: &jl::AcceptedBind) {
    let jobs = home.root.join("jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    let name = bind.followup_id.as_deref().unwrap_or(&bind.job_id);
    std::fs::write(
        jobs.join(format!("{name}.json")),
        serde_json::to_string(bind).unwrap(),
    )
    .unwrap();
}

/// The seller's work: a clone of the source at `base`, one agent change, pushed as the job's
/// delivery ref straight into the relay fixture's repository.
fn deliver(
    root: &Path,
    source: &Path,
    job_repo: &Path,
    parent: &str,
    file: &str,
    branch: &str,
) -> String {
    let work = root.join(format!("work-{}", branch.rsplit('/').next().unwrap()));
    if !work.exists() {
        git(
            root,
            &[
                "clone",
                "-q",
                source.to_str().unwrap(),
                work.to_str().unwrap(),
            ],
        );
    }
    git(
        &work,
        &[
            "fetch",
            "-q",
            job_repo.to_str().unwrap(),
            "+refs/heads/*:refs/remotes/job/*",
        ],
    );
    git(&work, &["checkout", "-q", "--detach", parent]);
    let commit = commit_file(&work, file, "agent change\n");
    git(
        &work,
        &[
            "push",
            "-q",
            job_repo.to_str().unwrap(),
            &format!("HEAD:{branch}"),
        ],
    );
    commit
}

#[test]
fn private_contribution_reads_only_the_relay_job_repo_after_posting() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    // The buyer's private source and its "private GitHub" mirror.
    let source = root.join("source");
    std::fs::create_dir(&source).unwrap();
    git(&source, &["init", "-q", "-b", "main"]);
    let base = commit_file(&source, "README.md", "private base\n");
    let github_git = root.join("github.git");
    git(
        root,
        &[
            "clone",
            "-q",
            "--bare",
            source.to_str().unwrap(),
            github_git.to_str().unwrap(),
        ],
    );
    let github = GitHttpAuthServer::spawn(&github_git, "/owner/private-repo.git");
    let github_url = github.repo_url();
    let (repo_a, server_a) = job_repo_server(root, "job-a");
    let (repo_b, server_b) = job_repo_server(root, "job-b");
    let (repo_c, server_c) = job_repo_server(root, "job-c");
    let ca = root.join("ca.pem");
    let mut certs = std::fs::read_to_string(github.ca_file(root)).unwrap();
    for server in [&server_a, &server_b, &server_c] {
        certs.push_str(&std::fs::read_to_string(server.ca_file(root)).unwrap());
    }
    std::fs::write(&ca, certs).unwrap();
    // SAFETY (edition 2024 set_var): the only test in this binary; set before any transport use.
    // A developer shell's stray MAXPLAYER_* would make every home bootstrap refuse; this test sets
    // its config explicitly, so none is wanted.
    for (key, _) in std::env::vars().filter(|(k, _)| k.starts_with("MAXPLAYER_")) {
        unsafe { std::env::remove_var(key) };
    }
    unsafe {
        std::env::set_var("SSL_CERT_FILE", &ca);
        std::env::set_var("GIT_SSL_NO_VERIFY", "1");
        std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
        std::env::set_var("no_proxy", "127.0.0.1,localhost");
    }
    let service = Keys::generate().public_key().to_hex();
    let seller = Keys::generate();
    let runtime = tokio::runtime::Runtime::new().unwrap();

    // ── A: real private post from a local checkout, then a PAID collect. ─────────────────────
    let home_a = buyer_home(root, "home-a", &server_a, &service);
    let buyer_a = Keys::parse(&home::read_secret_key_hex(&home_a).unwrap()).unwrap();
    let pin = contribution::ContributionOffer {
        target: contribution::TargetRepoPin::new(buyer_a.public_key().to_hex(), github_url.clone())
            .unwrap(),
        base: contribution::ContributionBase::new("main", base.clone()).unwrap(),
        accepts: vec!["fork".into()],
    };
    let request = jl::PostJobRequest {
        visibility: Some(pc::wire::Visibility::Private),
        output_category: Some(pc::wire::Output::Code),
        inputs: vec![],
        task: "contribute".into(),
        output: "git".into(),
        amount_sats: 10,
        seller_pubkey: Some(seller.public_key().to_hex()),
        untargeted: false,
        deadline_unix: Some(4_000_000_000),
        repo: None,
        branch: None,
        job: jl::JobKind::Contribution(jl::ContributionSpec {
            target_repo_owner: buyer_a.public_key().to_hex(),
            target_repo_url: github_url.clone(),
            base_local_path: Some(source.clone()),
            base_branch: "main".into(),
            base_oid: base.clone(),
            accepts: None,
        }),
        requested_agent: None,
        requested_harness_family: None,
        requested_model: None,
        required_capabilities: vec![],
        accepts_delivery: vec![],
        payment_mode: gateway::PaymentMode::Sat,
    };
    let offer_draft = gateway::OfferDraft::new(
        "contribute",
        "git",
        10,
        4_000_000_000,
        seller.public_key().to_hex(),
    );
    let outcome = runtime
        .block_on(pc::posting::post(
            &home_a,
            &buyer_a,
            &request,
            &offer_draft.to_event_draft(),
            Some(&pin),
        ))
        .expect("private post");
    let store = pc::store::ContentStore::open(&home_a.root.join("private-content.sqlite")).unwrap();
    let offer_a = store.event(&outcome.job_id).unwrap().unwrap();
    let task_a = pc::channel::ContentContext::open(&home_a, &buyer_a.public_key().to_hex())
        .unwrap()
        .accept_content(
            &offer_a,
            &offer_a,
            None,
            None,
            None,
            nostr_sdk::Timestamp::now().as_secs(),
        )
        .unwrap();
    let host_a = pc::wire::HostPolicy {
        git_prefix: git_base(&server_a),
        accepted_mints: vec![],
    };
    let trade_a = Trade::new(&buyer_a, &seller, &host_a, &offer_a, &task_a, true);
    let branch_a = format!("refs/heads/delivery/{}", outcome.job_id);
    let commit_a = deliver(root, &source, &repo_a, &base, "feature.rs", &branch_a);
    write_bind(&home_a, &trade_a.bind(&branch_a, &commit_a, None));
    let mut gate = BudgetGate::from_home(&home_a).unwrap();
    let error = runtime
        .block_on(collect_async(
            &home_a,
            Some(&mut gate),
            CollectRequest {
                job_id: outcome.job_id.clone(),
                out: None,
            },
        ))
        .expect_err("the dead mint is the only thing left to refuse");
    let message = error.to_string();
    assert!(
        message.contains("authorize_pay cancelled: mint") && message.contains("no funds moved"),
        "every delivery gate must pass and the pay stop only at the mint preflight: {message}"
    );
    assert_eq!(gate.spent(), 0);

    // ── B: legacy wire (task names the external URL), FREE, turn 1 then turn 2. ───────────────
    let home_b = buyer_home(root, "home-b", &server_b, &service);
    let buyer_b = Keys::parse(&home::read_secret_key_hex(&home_b).unwrap()).unwrap();
    let host_b = pc::wire::HostPolicy {
        git_prefix: git_base(&server_b),
        accepted_mints: vec![],
    };
    let job_b = "4b".repeat(32);
    let prepared = pc::builders::prepare_offer(
        &buyer_b,
        &gateway::OfferDraft::new(
            "contribute",
            "git",
            0,
            4_000_000_000,
            seller.public_key().to_hex(),
        )
        .with_payment_mode(gateway::PaymentMode::None),
        pc::builders::OfferOptions {
            visibility: pc::wire::Visibility::Private,
            category: pc::wire::Output::Code,
            service: &service,
            job_id: &job_b,
            attachments: vec![],
            contribution: Some(pc::Contribution {
                target_owner_pubkey: buyer_b.public_key().to_hex(),
                target_clone_url: github_url.clone(),
                base_branch: "main".into(),
                base_oid: base.clone(),
                accepts: vec!["fork".into()],
                input: None,
            }),
        },
        &host_b,
    )
    .unwrap();
    // What the pre-fix poster uploaded: the base on a buyer `input/` ref of the job repository.
    git(
        &source,
        &[
            "push",
            "-q",
            repo_b.to_str().unwrap(),
            &format!("{base}:refs/heads/input/{}", "1e".repeat(32)),
        ],
    );
    let offer_b = prepared.event.clone();
    let task_b = prepared.task.expect("targeted task");
    assert_eq!(
        task_b
            .body()
            .contribution
            .as_ref()
            .unwrap()
            .target_clone_url,
        github_url
    );
    let trade_b = Trade::new(&buyer_b, &seller, &host_b, &offer_b, &task_b, false);
    let offer_id_b = offer_b.id.to_hex();
    let branch_t1 = format!("refs/heads/delivery/{offer_id_b}");
    let commit_t1 = deliver(root, &source, &repo_b, &base, "turn1.rs", &branch_t1);
    write_bind(&home_b, &trade_b.bind(&branch_t1, &commit_t1, None));
    let turn1 = runtime
        .block_on(collect_async(
            &home_b,
            None,
            CollectRequest {
                job_id: offer_id_b.clone(),
                out: None,
            },
        ))
        .expect("turn 1 collects from the job repository alone");
    assert!(
        turn1.files.contains(&"turn1.rs".to_string()),
        "{:?}",
        turn1.files
    );

    let followup = "2f".repeat(32);
    let branch_t2 = format!("refs/heads/delivery/{followup}");
    let commit_t2 = deliver(root, &source, &repo_b, &commit_t1, "turn2.rs", &branch_t2);
    write_bind(
        &home_b,
        &trade_b.bind(&branch_t2, &commit_t2, Some(&followup)),
    );
    let turn2 = runtime
        .block_on(collect_turn_async(
            &home_b,
            None,
            &offer_id_b,
            &followup,
            2,
            &[],
        ))
        .expect("turn 2 collects from the job repository alone");
    assert_eq!(turn2.commit_oid, commit_t2);
    assert!(
        turn2.files.contains(&"turn2.rs".to_string()),
        "{:?}",
        turn2.files
    );

    // ── C: the job-repo proof is the BUYER's input ref, not mere reachability. A base that only a
    //    seller delivery ref points at (a seller cannot create `input/*`) is refused. ────────────
    let host_c = pc::wire::HostPolicy {
        git_prefix: git_base(&server_c),
        accepted_mints: vec![],
    };
    let job_c = "5c".repeat(32);
    let relay_c = RelayJobRepo::for_job(&host_c, &buyer_a.public_key().to_hex(), &job_c).unwrap();
    let base_c = commit_file(&source, "seller-chosen.md", "not the buyer's upload\n");
    git(
        &source,
        &[
            "push",
            "-q",
            repo_c.to_str().unwrap(),
            &format!("{base_c}:refs/heads/delivery/{}", "6d".repeat(32)),
        ],
    );
    let branch_c = format!("refs/heads/delivery/{}", "7e".repeat(32));
    let commit_c = deliver(root, &source, &repo_c, &base_c, "fork.rs", &branch_c);
    let fork_c = GitDelivery::new(
        relay_c.url(),
        &branch_c,
        CommitOid::parse(commit_c).unwrap(),
    )
    .unwrap();
    let mut verifier = PayPathDeliveryVerifier::new(
        root.join("store-c.git"),
        Some(home::read_secret_key_hex(&home_a).unwrap()),
    );
    let base_c_oid = CommitOid::parse(base_c.clone()).unwrap();
    let refused = verifier.verify_contribution(
        &fork_c,
        &ContributionBaseSource::JobRepo(relay_c.clone()),
        &base_c_oid,
        &ContentPolicy::floor(),
    );
    assert!(
        matches!(refused, Err(DeliveryError::MissingBaseObject)),
        "{refused:?}"
    );
    git(
        &source,
        &[
            "push",
            "-q",
            repo_c.to_str().unwrap(),
            &format!("{base_c}:refs/heads/input/{}", "8f".repeat(32)),
        ],
    );
    verifier
        .verify_contribution(
            &fork_c,
            &ContributionBaseSource::JobRepo(relay_c),
            &base_c_oid,
            &ContentPolicy::floor(),
        )
        .expect("the buyer's own input ref proves the base");

    // ── The rule itself: nothing after posting dialed the external source. ───────────────────
    assert!(
        github.requests().is_empty(),
        "a post-posting leg dialed the external source: {:?}",
        github
            .requests()
            .iter()
            .map(|r| &r.target)
            .collect::<Vec<_>>()
    );

    // ── The new wire (A): the task names the relay copy; the source stays buyer-local. ────────
    let job_repo_a = host_a
        .job_repo(&buyer_a.public_key().to_hex(), tag(&offer_a, "job"))
        .unwrap();
    assert_eq!(
        task_a
            .body()
            .contribution
            .as_ref()
            .unwrap()
            .target_clone_url,
        job_repo_a
    );
    let wire = format!(
        "{}{}",
        serde_json::to_string(&offer_a).unwrap(),
        serde_json::to_string(task_a.body()).unwrap()
    );
    assert!(
        !wire.contains(&github_url),
        "the external source must not ride the wire"
    );
    let record = std::fs::read_to_string(
        home_a
            .root
            .join("contribution-sources")
            .join(format!("{}.json", outcome.job_id)),
    )
    .expect("buyer-local source record for porting");
    assert!(
        record.contains(&github_url) && record.contains(&job_repo_a),
        "{record}"
    );
}
