use super::*;
use nostr_relay_builder::prelude::{LocalRelay, RateLimit, RelayBuilder};

const CH: &str = "0b0e5f1c-9d4a-4c3e-8f21-6a7b8c9d0e1f";
const OTHER: &str = "1b0e5f1c-9d4a-4c3e-8f21-6a7b8c9d0e1f";

fn home(dir: &Path, relay: &str) -> Home {
    Home::open(dir, relay).unwrap()
}
fn msg(keys: &Keys, tags: Vec<Vec<&str>>, text: &str) -> Event {
    let tags: Vec<Tag> = tags.into_iter().map(|t| Tag::parse(t).unwrap()).collect();
    EventBuilder::new(Kind::Custom(9), text).tags(tags).sign_with_keys(keys).unwrap()
}
fn msg_at(keys: &Keys, text: &str, at: u64) -> Event {
    EventBuilder::new(Kind::Custom(9), text)
        .tag(Tag::parse(["h", CH]).unwrap())
        .custom_created_at(Timestamp::from(at))
        .sign_with_keys(keys)
        .unwrap()
}
/// Configure a content-service reviewer key for this home's relay and return it.
fn reviewed(h: &mut Home) -> Keys {
    let reviewer = Keys::generate();
    h.config.review.reviewers.insert(h.relay.clone(), reviewer.public_key().to_hex());
    h.config.privacy.service_pubkey = Some(reviewer.public_key().to_hex());
    reviewer
}
/// A verdict signed by `reviewer` with this unsafe probability.
fn signed_verdict(reviewer: &Keys, subject: &maxplayer_core::review::Subject, unsafe_p: f64) -> Event {
    let mut probabilities = BTreeMap::new();
    probabilities.insert("safe".to_owned(), 1.0 - unsafe_p);
    probabilities.insert("unsafe".to_owned(), unsafe_p);
    let verdict = maxplayer_core::review::Review {
        schema: 1,
        subject: subject.clone(),
        input_sha256: "aa".repeat(32),
        status: "ok".into(),
        results: vec![maxplayer_core::review::Classification {
            classifier: maxplayer_core::review::CLASSIFIER.into(),
            version: maxplayer_core::review::CLASSIFIER_VERSION.into(),
            label: if unsafe_p >= 0.5 { "unsafe" } else { "safe" }.into(),
            probabilities: probabilities.into_iter().collect(),
        }],
        error_code: None,
    };
    let draft = maxplayer_core::review::review_draft(&verdict).unwrap();
    maxplayer_core::gateway::nostr::event_builder(&draft).unwrap().sign_with_keys(reviewer).unwrap()
}
fn join_crew(h: &Home) -> Team {
    let t = Team { name: "crew".into(), channel: CH.into(), creator: "ab".repeat(32), open: false };
    let _l = h.lock().unwrap();
    let mut s = h.load().unwrap();
    s.teams.insert(CH.into(), t.clone());
    h.save(&s).unwrap();
    t
}
/// Plain git for fixtures, outside the code under test.
fn sh(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "init.defaultBranch=main", "-C"])
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}
/// A team repo served from a local bare repo: the quarantine fetch of the relay URL is
/// redirected there (`insteadOf`, repo-local config). Returns the work repo that commits and
/// pushes to the remote, and an empty worktree whose origin is the relay URL.
struct RepoFixture {
    _dirs: tempfile::TempDir,
    remote: PathBuf,
    work: PathBuf,
    dir: PathBuf,
    q: PathBuf,
}
impl RepoFixture {
    fn new(h: &Home, t: &Team) -> Self {
        let dirs = tempfile::tempdir().unwrap();
        let (remote, work, dir) = (dirs.path().join("remote.git"), dirs.path().join("work"), dirs.path().join("clone"));
        sh(dirs.path(), &["init", "-q", "--bare", "remote.git"]);
        sh(dirs.path(), &["init", "-q", "work"]);
        fs::create_dir(&dir).unwrap();
        sh(&dir, &["init", "-q"]);
        sh(&dir, &["remote", "add", "origin", &h.repo_url(t).unwrap()]);
        let q = h.path(&format!("quarantine/{}.git", t.repo()));
        sh(&h.path("quarantine"), &["init", "-q", "--bare", &q.to_string_lossy()]);
        sh(&q, &["config", &format!("url.{}.insteadOf", remote.display()), &h.repo_url(t).unwrap()]);
        Self { _dirs: dirs, remote, work, dir, q }
    }
    /// Commit a file in the work repo and push it to the remote's main.
    fn commit(&self, text: &str) -> String {
        fs::write(self.work.join("f.txt"), text).unwrap();
        sh(&self.work, &["add", "."]);
        sh(&self.work, &["commit", "-qm", text]);
        sh(&self.work, &["push", "-q", &self.remote.to_string_lossy(), "HEAD:refs/heads/main"]);
        sh(&self.work, &["rev-parse", "HEAD"])
    }
    fn origin(&self) -> Option<String> {
        h_git(&self.dir, &["rev-parse", "--verify", "-q", "refs/remotes/origin/main"])
    }
}
fn h_git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").env("GIT_CONFIG_GLOBAL", "/dev/null").arg("-C").arg(dir).args(args).output().unwrap();
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}
/// `pull_in` with a short deadline: the relay at 127.0.0.1:9 is unreachable, so only screening
/// from cached verdicts (or review switched off) can let a commit through.
fn pull(h: &mut Home, t: &Team, f: &RepoFixture) -> Result<String> {
    h.sync_deadline = Some(std::time::Instant::now() + Duration::from_millis(600));
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(h.pull_in(t, &f.dir, "main"))
}

#[test]
fn admit_refuses_multi_h_wrong_channel_oversize_and_foreign_follow_author() {
    let (owner, other) = (Keys::generate(), Keys::generate());
    let ok = msg(&owner, vec![vec!["h", CH]], "hi");
    assert!(admit(&ok, CH, None).is_ok());
    assert!(admit(&ok, CH, Some(&owner.public_key().to_hex())).is_ok());
    // A followed post from anyone but the owner is dropped even if the relay sent it.
    assert!(admit(&msg(&other, vec![vec!["h", CH]], "hi"), CH, Some(&owner.public_key().to_hex())).is_err());
    for bad in [
        msg(&owner, vec![vec!["h", CH], vec!["h", OTHER]], "hi"),
        msg(&owner, vec![vec!["h", OTHER]], "hi"),
        msg(&owner, vec![vec!["h", CH]], &"x".repeat(8001)),
        msg(&owner, vec![vec!["h", CH], vec!["pad", &"y".repeat(33 * 1024)]], "hi"),
    ] {
        assert!(admit(&bad, CH, None).is_err());
    }
}

#[test]
fn marker_is_strict() {
    let hex = "ab".repeat(32);
    assert_eq!(parse_marker(&format!("{MARKER} {hex}")), Some(hex.clone()));
    for bad in [format!("{MARKER} {}", hex.to_uppercase()), format!("{MARKER}  {hex}"), format!("x {hex}"), MARKER.to_owned()] {
        assert!(parse_marker(&bad).is_none(), "{bad}");
    }
}

#[test]
fn follow_join_collision_and_default_follow_persist() {
    let dir = tempfile::tempdir().unwrap();
    let h = home(dir.path(), DEFAULT_RELAY);
    let state = h.load().unwrap();
    assert_eq!(h.follows(&state)[0].channel, TEAM_MAXPLAYER.0);
    // Not seeded on another relay.
    let other = tempfile::tempdir().unwrap();
    let o = home(other.path(), "ws://127.0.0.1:9");
    assert!(o.follows(&o.load().unwrap()).is_empty());
    // Unfollowing the default persists.
    h.unfollow(TEAM_MAXPLAYER.0).unwrap();
    assert!(h.follows(&h.load().unwrap()).is_empty());
    assert_eq!(h.load().unwrap().follows, Some(vec![]));
    // A joined channel cannot be followed.
    {
        let _l = h.lock().unwrap();
        let mut s = h.load().unwrap();
        s.teams.insert(CH.into(), Team { name: "crew".into(), channel: CH.into(), creator: "ab".repeat(32), open: false });
        h.save(&s).unwrap();
    }
    assert!(h.follow(CH, &"cd".repeat(32)).is_err());
    assert!(h.follow(OTHER, "not-hex").is_err());
    h.follow(OTHER, &"cd".repeat(32)).unwrap();
    assert_eq!(h.follows(&h.load().unwrap()).len(), 1);
}

#[tokio::test]
async fn join_refuses_a_followed_channel_before_the_relay() {
    let dir = tempfile::tempdir().unwrap();
    let h = home(dir.path(), "ws://127.0.0.1:9");
    h.follow(CH, &"cd".repeat(32)).unwrap();
    let e = h.join(CH, None).await.unwrap_err().to_string();
    assert!(e.contains("followed"), "{e}");
    assert!(h.join("NOT-A-UUID", None).await.is_err());
}

#[test]
fn stored_verdicts_are_re_decided_with_the_current_config() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = home(dir.path(), "ws://127.0.0.1:9");
    let reviewer = reviewed(&mut h);
    let author = Keys::generate();
    let event = msg(&author, vec![vec!["h", CH]], "maybe risky");
    let subject = review::Request::message(event.clone()).unwrap().subject;
    let verdict = signed_verdict(&reviewer, &subject, 0.6);
    let stored = Stored { channel: CH.into(), event, verdict: Some(verdict.clone()), error: None };
    let mut state = State::default();
    assert!(matches!(h.status(&state, &stored), Status::Withheld { ref state, unsafe_ppm: Some(600_000), .. } if state == "refused"));
    // Raising the threshold applies to the stored message.
    h.config.review.reject_at_or_above_ppm = 700_000;
    assert_eq!(h.status(&state, &stored), Status::Delivered);
    h.config.review.reject_at_or_above_ppm = 500_000;
    // Accepting exactly this verdict delivers it; a different verdict id does not.
    state.accepted.insert(subject.event.clone(), "ff".repeat(32));
    assert!(matches!(h.status(&state, &stored), Status::Withheld { .. }));
    state.accepted.insert(subject.event.clone(), verdict.id.to_hex());
    assert_eq!(h.status(&state, &stored), Status::Delivered);
    // A changed reviewer key invalidates the stored verdict.
    let new = Keys::generate().public_key().to_hex();
    h.config.review.reviewers.insert(h.relay.clone(), new.clone());
    h.config.privacy.service_pubkey = Some(new);
    assert!(matches!(h.status(&state, &stored), Status::Withheld { ref state, .. } if state == "invalid_verdict"));
    // A pending (availability-withheld) message is withheld, an error is final, own messages pass.
    let pending = Stored { verdict: None, ..stored.clone() };
    assert_eq!(h.status(&state, &pending), withheld("pending"));
    let own = Stored { event: msg(&h.keys, vec![vec!["h", CH]], "mine"), verdict: None, ..stored };
    assert_eq!(h.status(&state, &own), Status::Delivered);
}

#[test]
fn push_check_refuses_binary_symlink_and_submodule() {
    let dir = tempfile::tempdir().unwrap();
    let h = home(dir.path(), "ws://127.0.0.1:9");
    let repo = dir.path().join("w");
    fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        let out = h.git(&repo, None, args).unwrap();
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    fs::write(repo.join("a.txt"), "text\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "text"]);
    let ok = git(&["rev-parse", "HEAD"]);
    assert!(h.check_pushable(&repo, &ok).is_ok());
    fs::write(repo.join("b.bin"), b"\0\x01").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "bin"]);
    assert!(h.check_pushable(&repo, &git(&["rev-parse", "HEAD"])).is_err());
    git(&["rm", "-q", "b.bin"]);
    std::os::unix::fs::symlink("a.txt", repo.join("link")).unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "link"]);
    assert!(h.check_pushable(&repo, &git(&["rev-parse", "HEAD"])).is_err());
    git(&["rm", "-q", "link"]);
    git(&["update-index", "--add", "--cacheinfo", &format!("160000,{ok},sub")]);
    git(&["commit", "-qm", "sub"]);
    assert!(h.check_pushable(&repo, &git(&["rev-parse", "HEAD"])).is_err());
    // A git failure is a refusal, never a pass.
    assert!(h.check_pushable(&repo, &"1".repeat(40)).is_err());
    // `push` walks every new commit: the binary one, two commits down, stops it before the relay.
    git(&["reset", "-q", "--hard", &ok]);
    fs::write(repo.join("b.bin"), b"\0").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "bin"]);
    fs::write(repo.join("a.txt"), "more\n").unwrap();
    git(&["commit", "-qam", "text on top"]);
    let t = Team { name: "crew".into(), channel: CH.into(), creator: "ab".repeat(32), open: false };
    git(&["remote", "add", "origin", &h.repo_url(&t).unwrap()]);
    let e = h.push_in(&t, &repo, None).unwrap_err().to_string();
    assert!(e.contains("not pushed") && e.contains("binary"), "{e}");
}

/// Spec §5: a pull transfers the screened oid, so a quarantine ref moved by a concurrent pull
/// between screening and transfer never reaches `origin/<branch>`. The worktree keeps no
/// credentials.
#[test]
fn pull_transfers_the_screened_oid_even_if_the_quarantine_ref_moves() {
    let hd = tempfile::tempdir().unwrap();
    let h = home(hd.path(), "ws://127.0.0.1:9");
    let t = join_crew(&h);
    let f = RepoFixture::new(&h, &t);
    let c1 = f.commit("one\n");
    // Hold the state lock so the pull waits right before screening.
    let lock = h.lock().unwrap();
    let (root, team, dir) = (hd.path().to_owned(), t.clone(), f.dir.clone());
    let puller = std::thread::spawn(move || {
        let mut h2 = home(&root, "ws://127.0.0.1:9");
        h2.config.review.team_commit = false;
        h2.sync_deadline = Some(std::time::Instant::now() + Duration::from_secs(1));
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(h2.pull_in(&team, &dir, "main")).map_err(|e| e.to_string())
    });
    let started = std::time::Instant::now();
    while h_git(&f.q, &["rev-parse", "--verify", "-q", "refs/quarantine/main"]).is_none() {
        assert!(started.elapsed() < Duration::from_secs(5), "quarantine fetch never ran");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(200));
    // Another pull moves the quarantine ref to an unscreened commit.
    let c2 = f.commit("two\n");
    sh(&f.q, &["fetch", "-q", &f.remote.to_string_lossy(), "+refs/heads/main:refs/quarantine/main"]);
    drop(lock);
    let out = puller.join().unwrap().unwrap();
    assert!(out.contains("1 new commit(s) passed review"), "{out}");
    assert_eq!(f.origin().as_deref(), Some(c1.as_str()), "c2 = {c2} was never screened");
    let config = fs::read_to_string(f.dir.join(".git/config")).unwrap();
    assert!(!config.to_lowercase().contains("extraheader") && !config.contains('@'), "{config}");
    assert!(h.repo_url(&Team { open: true, ..t }).is_err());
}

/// Review on: a cached verdict under an old reviewer key is asked for again (not a permanent
/// error); a refused verdict keeps `origin` where it was until `accept-verdict` accepts exactly
/// that verdict.
#[test]
fn pull_with_review_on_withholds_a_refused_commit_until_accepted() {
    let hd = tempfile::tempdir().unwrap();
    let mut h = home(hd.path(), "ws://127.0.0.1:9");
    let reviewer = reviewed(&mut h);
    let t = join_crew(&h);
    let f = RepoFixture::new(&h, &t);
    let c1 = f.commit("one\n");
    let subject = review::Request::commit(&t.repo(), &t.creator, &c1).unwrap().subject;
    let seed = |h: &Home, v: &Event| {
        let mut s = h.load().unwrap();
        s.commit_verdicts.insert(subject.event.clone(), v.clone());
        h.save(&s).unwrap();
    };
    seed(&h, &signed_verdict(&Keys::generate(), &subject, 0.1));
    let e = pull(&mut h, &t, &f).unwrap_err().to_string();
    assert!(e.contains("review unavailable"), "a stale verdict is re-requested: {e}");
    let refused = signed_verdict(&reviewer, &subject, 0.6);
    seed(&h, &refused);
    let e = pull(&mut h, &t, &f).unwrap_err().to_string();
    assert!(e.contains("withheld") && e.contains(&refused.id.to_hex()), "{e}");
    assert_eq!(f.origin(), None);
    assert!(h.accept_verdict("crew", &c1, &"ff".repeat(32)).is_err());
    h.accept_verdict("crew", &c1, &refused.id.to_hex()).unwrap();
    let out = pull(&mut h, &t, &f).unwrap();
    assert!(out.contains("passed review"), "{out}");
    assert_eq!(f.origin().as_deref(), Some(c1.as_str()));
}

async fn local_relay() -> (LocalRelay, String, Client) {
    let relay = LocalRelay::new(RelayBuilder::default().rate_limit(RateLimit { max_reqs: 500, notes_per_minute: 1_000_000 }));
    relay.run().await.unwrap();
    let url = relay.url().await.to_string();
    let client = Client::new(Keys::generate());
    client.add_relay(&url).await.unwrap();
    client.connect().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    (relay, url, client)
}
fn delivered(h: &Home) -> usize {
    let state = h.load().unwrap();
    h.stored().unwrap().iter().filter(|s| h.status(&state, s) == Status::Delivered).count()
}

/// A message that reaches the relay late with an older timestamp (inside the relay's accepted
/// skew) is still fetched by the next catch-up.
#[tokio::test]
async fn catch_up_rereads_the_clock_skew_before_its_cursor() {
    let (_relay, url, client) = local_relay().await;
    let dir = tempfile::tempdir().unwrap();
    let mut h = home(dir.path(), &url);
    h.config.review.team_message = false;
    join_crew(&h);
    let mut state = h.load().unwrap();
    client.send_event(&msg_at(&Keys::generate(), "first", now())).await.unwrap();
    h.catch_up(&client, &mut state).await.unwrap();
    assert!(state.cursors[CH] >= now() - 1);
    client.send_event(&msg_at(&Keys::generate(), "late", now() - 120)).await.unwrap();
    h.catch_up(&client, &mut state).await.unwrap();
    assert_eq!(delivered(&h), 2);
}

/// More than 1000 messages in one second: the catch-up steps back a second and still reads
/// everything older (spec §3), instead of stopping there.
#[tokio::test]
async fn catch_up_steps_back_past_a_saturated_second() {
    let (_relay, url, client) = local_relay().await;
    let dir = tempfile::tempdir().unwrap();
    let mut h = home(dir.path(), &url);
    h.config.review.team_message = false;
    join_crew(&h);
    let mut state = h.load().unwrap();
    state.cursors.insert(CH.into(), now() - 100);
    let author = Keys::generate();
    for n in 0..3 {
        client.send_event(&msg_at(&author, &format!("older {n}"), now() - 50)).await.unwrap();
    }
    let busy = now() - 10;
    for n in 0..1001 {
        client.send_event(&msg_at(&author, &format!("busy {n}"), busy)).await.unwrap();
    }
    h.catch_up(&client, &mut state).await.unwrap();
    let texts: Vec<String> = h.stored().unwrap().into_iter().map(|s| s.event.content).collect();
    for n in 0..3 {
        assert!(texts.contains(&format!("older {n}")), "older {n} lost ({} stored)", texts.len());
    }
}

/// Review on, reviewer silent: a message waits withheld (`pending`) on its own back-off and does
/// not block later ones; a message stored while review was off is reviewed once it is on.
#[tokio::test]
async fn catch_up_with_review_on_retries_pending_messages() {
    let (_relay, url, client) = local_relay().await;
    let dir = tempfile::tempdir().unwrap();
    let mut h = home(dir.path(), &url);
    h.config.review.team_message = false;
    join_crew(&h);
    let author = Keys::generate();
    let mut state = h.load().unwrap();
    let before = msg_at(&author, "while review was off", now() - 5);
    client.send_event(&before).await.unwrap();
    h.catch_up(&client, &mut state).await.unwrap();
    reviewed(&mut h);
    h.config.review.team_message = true;
    h.config.review.timeout_seconds = 1;
    let later = [msg_at(&author, "two", now() - 2), msg_at(&author, "three", now() - 1)];
    for e in &later {
        client.send_event(e).await.unwrap();
    }
    h.catch_up(&client, &mut state).await.unwrap();
    let all = h.stored().unwrap();
    assert_eq!(all.len(), 3);
    for s in &all {
        assert_eq!(h.status(&state, s), withheld("pending"), "{}", s.event.content);
        assert!(state.retry.contains_key(&s.event.id.to_hex()), "{} has no retry", s.event.content);
    }
    assert!(state.cursors[CH] >= later[1].created_at.as_secs());
    // The next pass asks again for the one that is due, and backs it off.
    h.catch_up(&client, &mut state).await.unwrap();
    assert_eq!(state.retry[&before.id.to_hex()].delay, 60);
}

/// The reader's own message, already stored, still moves the cursor: a later burst larger than
/// the first-sync window is read in full, not cut to the newest [`FIRST_SYNC`].
#[tokio::test]
async fn already_stored_messages_advance_the_cursor() {
    let (_relay, url, client) = local_relay().await;
    let dir = tempfile::tempdir().unwrap();
    let mut h = home(dir.path(), &url);
    h.config.review.team_message = false;
    join_crew(&h);
    let mut state = h.load().unwrap();
    let own = msg_at(&h.keys, "mine", now() - 5);
    client.send_event(&own).await.unwrap();
    h.append(&Stored { channel: CH.into(), event: own, verdict: None, error: None }).unwrap();
    h.catch_up(&client, &mut state).await.unwrap();
    assert!(state.cursors.contains_key(CH), "own stored message left the cursor unset");
    let other = Keys::generate();
    for n in 0..FIRST_SYNC + 10 {
        client.send_event(&msg_at(&other, &format!("burst {n}"), now())).await.unwrap();
    }
    h.catch_up(&client, &mut state).await.unwrap();
    assert_eq!(h.stored().unwrap().len(), FIRST_SYNC + 11);
}

/// `inbox` and `team_inbox` report an unreachable relay instead of "no new messages".
#[tokio::test]
async fn inbox_sync_reports_an_unreachable_relay() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = home(dir.path(), "ws://127.0.0.1:9");
    join_crew(&h);
    h.sync_deadline = Some(std::time::Instant::now() + Duration::from_millis(600));
    let e = h.sync().await.unwrap_err().to_string();
    assert!(e.contains("could not reach the relay"), "{e}");
}

#[test]
fn a_torn_line_in_the_message_store_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let h = home(dir.path(), "ws://127.0.0.1:9");
    h.append(&Stored { channel: CH.into(), event: msg(&Keys::generate(), vec![vec!["h", CH]], "ok"), verdict: None, error: None }).unwrap();
    fs::OpenOptions::new().append(true).open(h.path("messages.jsonl")).unwrap().write_all(b"{\"trunc").unwrap();
    assert_eq!(h.stored().unwrap().len(), 1);
}

/// A refused post on a followed channel can be accepted like a team message.
#[test]
fn accept_verdict_works_on_a_followed_channel() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = home(dir.path(), "ws://127.0.0.1:9");
    let reviewer = reviewed(&mut h);
    let owner = Keys::generate();
    h.follow(CH, &owner.public_key().to_hex()).unwrap();
    let event = msg(&owner, vec![vec!["h", CH]], "risky");
    let verdict = signed_verdict(&reviewer, &review::Request::message(event.clone()).unwrap().subject, 0.6);
    h.append(&Stored { channel: CH.into(), event: event.clone(), verdict: Some(verdict.clone()), error: None }).unwrap();
    h.accept_verdict(CH, &event.id.to_hex(), &verdict.id.to_hex()).unwrap();
    assert_eq!(h.status(&h.load().unwrap(), &h.stored().unwrap()[0]), Status::Delivered);
}

#[tokio::test]
async fn invites_report_an_unreachable_relay_and_a_failed_clone_leaves_no_directory() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = home(dir.path(), "ws://127.0.0.1:9");
    h.sync_deadline = Some(std::time::Instant::now() + Duration::from_millis(600));
    assert!(h.invites().await.is_err());
    join_crew(&h);
    let target = dir.path().join("clone");
    assert!(h.clone_repo("crew", Some(target.to_str().unwrap())).await.is_err());
    assert!(!target.exists());
}

#[test]
fn home_key_is_the_maxplayer_home_key_created_once() {
    let dir = tempfile::tempdir().unwrap();
    let first = home(dir.path(), "ws://127.0.0.1:9");
    let second = home(dir.path(), "ws://127.0.0.1:9");
    assert_eq!(first.me(), second.me());
    let core = maxplayer_core::home::bootstrap(dir.path()).unwrap();
    let secret = maxplayer_core::home::read_secret_key_hex(&core).unwrap();
    assert_eq!(Keys::parse(&secret).unwrap().public_key().to_hex(), first.me());
    let link = dir.path().with_extension("link");
    std::os::unix::fs::symlink(dir.path(), &link).unwrap();
    assert!(Home::open(&link, "ws://127.0.0.1:9").is_err());
    fs::remove_file(link).unwrap();
}

#[test]
fn untrusted_text_is_escaped_and_length_checked() {
    assert_eq!(display_text("a\u{202e}b\nc\u{200b}"), "a\\u{202e}b\\nc\\u{200b}");
    assert_eq!(display_json(serde_json::json!("x\u{2066}y")), "\"x\\u2066y\"");
    // DEL, C1 (8-bit CSI), soft hyphen, invisible operator and a tag character: escaped in both,
    // as valid JSON (a surrogate pair above U+FFFF), and recoverable.
    let hostile = "a\u{7f}b\u{9b}c\u{ad}d\u{2063}e\u{e0041}f";
    let json = display_json(serde_json::json!(hostile));
    let text = display_text(hostile);
    for out in [&json, &text] {
        assert!(!out.chars().any(|c| c.is_control() || visual_format(c)), "{out:?}");
    }
    assert!(json.contains("\\udb40\\udc41"), "{json}");
    assert_eq!(serde_json::from_str::<String>(&json).unwrap(), hostile);
    assert!(validate_text("").is_err());
    assert!(validate_text(&"é".repeat(8000)).is_ok());
    assert!(validate_text(&"é".repeat(8001)).is_err());
}
