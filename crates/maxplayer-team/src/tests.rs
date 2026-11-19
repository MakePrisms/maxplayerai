use super::*;

const CH: &str = "0b0e5f1c-9d4a-4c3e-8f21-6a7b8c9d0e1f";
const OTHER: &str = "1b0e5f1c-9d4a-4c3e-8f21-6a7b8c9d0e1f";

fn home(dir: &Path, relay: &str) -> Home {
    Home::open(dir, relay).unwrap()
}
fn msg(keys: &Keys, tags: Vec<Vec<&str>>, text: &str) -> Event {
    let tags: Vec<Tag> = tags.into_iter().map(|t| Tag::parse(t).unwrap()).collect();
    EventBuilder::new(Kind::Custom(9), text).tags(tags).sign_with_keys(keys).unwrap()
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
    let reviewer = Keys::generate();
    h.config.review.reviewers.insert(h.relay.clone(), reviewer.public_key().to_hex());
    h.config.privacy.service_pubkey = Some(reviewer.public_key().to_hex());
    let author = Keys::generate();
    let event = msg(&author, vec![vec!["h", CH]], "maybe risky");
    let subject = review::Request::message(event.clone()).unwrap().subject;
    let mut probabilities = BTreeMap::new();
    probabilities.insert("safe".to_owned(), 0.4);
    probabilities.insert("unsafe".to_owned(), 0.6);
    let verdict = maxplayer_core::review::Review {
        schema: 1,
        subject: subject.clone(),
        input_sha256: "aa".repeat(32),
        status: "ok".into(),
        results: vec![maxplayer_core::review::Classification {
            classifier: maxplayer_core::review::CLASSIFIER.into(),
            version: maxplayer_core::review::CLASSIFIER_VERSION.into(),
            label: "unsafe".into(),
            probabilities: probabilities.into_iter().collect(),
        }],
        error_code: None,
    };
    let draft = maxplayer_core::review::review_draft(&verdict).unwrap();
    let verdict = maxplayer_core::gateway::nostr::event_builder(&draft).unwrap().sign_with_keys(&reviewer).unwrap();
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
}

#[test]
fn worktree_origin_has_no_credentials_so_bare_git_fetch_fails() {
    let dir = tempfile::tempdir().unwrap();
    let h = home(dir.path(), "ws://127.0.0.1:9");
    let t = Team { name: "crew".into(), channel: CH.into(), creator: "ab".repeat(32), open: false };
    let url = h.repo_url(&t).unwrap();
    assert_eq!(url, format!("http://127.0.0.1:9/git/{}/0b0e5f1c9d4a4c3e8f216a7b8c9d0e1f{}", "ab".repeat(32), "0".repeat(32)));
    assert!(!url.contains('@'));
    let open = Team { open: true, ..t };
    assert!(h.repo_url(&open).is_err());
}
