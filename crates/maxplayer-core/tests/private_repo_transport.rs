//! Private job repositories over real smart HTTPS: the ref a buyer writes, and the header a
//! reader sends.
//!
//! The relay takes only `refs/heads/input/<64 hex>` from the buyer and
//! `refs/heads/delivery/<64 hex>` from the seller. A private job names these refs in full, so a
//! push must not add a second `refs/heads/`. A reader signs its NIP-98 header itself (the seller
//! signs through its signer actor and holds no key here), so a private fetch must send that
//! header as it is. The fixture records the exact `Authorization` value of each request.
//!
//! A fetch by commit id also needs a server that advertises `allow-reachable-sha1-in-want`:
//! libgit2 refuses an oid refspec without it. The fixture repository sets
//! `uploadpack.allowReachableSHA1InWant`, as the relay must.
//!
//! Self-signed certificate, so this binary sets `GIT_SSL_NO_VERIFY` exactly as the other fixture
//! tests do.
#![cfg(all(unix, feature = "wallet"))]

mod git_http_fixture;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Once};

use git_http_fixture::GitHttpAuthServer;
use maxplayer_core::git_transport::{
    AuthMinter, fetch_bounded_objects, fetch_private_objects, nip98_authorization_header_with_keys,
    push_private_input,
};

static ENV_INIT: Once = Once::new();

fn init_test_env() {
    ENV_INIT.call_once(|| {
        // SAFETY (edition 2024 set_var): every test funnels through this Once before it touches the
        // transport; racing test threads block in call_once until the env is fully staged.
        unsafe {
            std::env::set_var("GIT_SSL_NO_VERIFY", "1");
            std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
            std::env::set_var("no_proxy", "127.0.0.1,localhost");
        }
    });
}

fn temp(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "maxplayer-private-repo-{label}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("mkdir root");
    root
}

/// The relay path of a private job repository: `/git/<buyer>/<job>`.
fn private_mount() -> String {
    format!("/git/{}/{}", "11".repeat(32), "22".repeat(32))
}

/// A bare repository with one commit at `reference`. Returns the commit id.
fn bare_with_commit(path: &Path, reference: &str) -> String {
    let repo = git2::Repository::init_bare(path).expect("init bare");
    let blob = repo.blob(b"The code word is fixture.\n").expect("blob");
    let mut builder = repo.treebuilder(None).expect("tree builder");
    builder.insert("brief.txt", blob, 0o100644).expect("insert");
    let tree = repo
        .find_tree(builder.write().expect("write tree"))
        .expect("find tree");
    let sig = git2::Signature::new("b", "b@example.invalid", &git2::Time::new(1_700_000_000, 0))
        .expect("sig");
    repo.commit(Some(reference), &sig, &sig, "input", &tree, &[])
        .expect("commit")
        .to_string()
}

// Before the fix the push went to `refs/heads/refs/heads/input/<id>`, and the relay hook
// refused it, so no buyer could attach an input file to a private job.
#[test]
fn private_input_push_writes_the_relay_input_ref() {
    init_test_env();
    let root = temp("push");
    let reference = format!("refs/heads/input/{}", "ab".repeat(32));
    let staging = root.join("staging.git");
    let oid = bare_with_commit(&staging, &reference);
    let served = root.join("served.git");
    git2::Repository::init_bare(&served).expect("served repo");
    let server = GitHttpAuthServer::spawn(&served, &private_mount());
    let url = server.repo_url();

    let keys = nostr_sdk::Keys::generate();
    let scope = reference.clone();
    let mint: AuthMinter = Arc::new(move |destination: &str| {
        nip98_authorization_header_with_keys(destination, &keys, Some(&scope), None)
            .map_err(|e| e.to_string())
    });
    let staging = git2::Repository::open_bare(&staging).expect("open staging");
    let pushed = push_private_input(&staging, &url, &reference, &oid, mint).expect("push the input");
    assert_eq!(pushed, oid);

    let served = git2::Repository::open_bare(&served).expect("open served");
    assert_eq!(
        served.refname_to_id(&reference).expect("input ref").to_string(),
        oid
    );
    assert!(
        served
            .refname_to_id(&format!("refs/heads/{reference}"))
            .is_err(),
        "the push wrote a doubled ref"
    );
    let requests = server.requests();
    assert!(!requests.is_empty());
    assert!(requests.iter().all(|r| r.authorization.is_some()), "{requests:?}");
    let _ = std::fs::remove_dir_all(&root);
}

// Before the fix the fetch parsed the header as a secret key and failed with
// "invalid key: Invalid secret key" before it sent a request, so no seller could read a
// private input.
#[test]
fn private_fetch_sends_the_signed_header_unchanged() {
    init_test_env();
    let root = temp("fetch");
    let reference = format!("refs/heads/input/{}", "cd".repeat(32));
    let served = root.join("served.git");
    let oid = bare_with_commit(&served, &reference);
    git2::Repository::open_bare(&served)
        .expect("open served")
        .config()
        .expect("served config")
        .set_bool("uploadpack.allowReachableSHA1InWant", true)
        .expect("allow a fetch by commit id");
    let server = GitHttpAuthServer::spawn(&served, &private_mount());
    let url = server.repo_url();
    let header = nip98_authorization_header_with_keys(&url, &nostr_sdk::Keys::generate(), None, None)
        .expect("sign header");

    let dest = git2::Repository::init_bare(root.join("dest.git")).expect("dest repo");
    fetch_private_objects(&dest, &url, &[&oid], &header).expect("fetch the pinned input");
    assert!(dest.find_commit(git2::Oid::from_str(&oid).unwrap()).is_ok());

    let requests = server.requests();
    assert!(!requests.is_empty());
    for request in &requests {
        assert_eq!(
            request.authorization.as_deref(),
            Some(header.as_str()),
            "{request:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

// The header goes only to relay Git, as it did when the fetch signed it from a key.
#[test]
fn bounded_fetch_sends_no_header_off_relay_git() {
    init_test_env();
    let root = temp("foreign");
    let served = root.join("served.git");
    let oid = bare_with_commit(&served, "refs/heads/main");
    let server = GitHttpAuthServer::spawn(&served, "/other/base.git");
    let url = server.repo_url();
    let header = nip98_authorization_header_with_keys(&url, &nostr_sdk::Keys::generate(), None, None)
        .expect("sign header");

    let dest = git2::Repository::init_bare(root.join("dest.git")).expect("dest repo");
    // The fixture refuses a request with no auth, so this fetch fails. The test is about what
    // the request carried.
    assert!(fetch_bounded_objects(&dest, &url, &[&oid], Some(&header)).is_err());
    let requests = server.requests();
    assert!(!requests.is_empty(), "the fetch reached the fixture");
    assert!(requests.iter().all(|r| r.authorization.is_none()), "{requests:?}");
    assert!(
        fetch_private_objects(&dest, &url, &[&oid], &header).is_err(),
        "a private fetch needs relay Git"
    );
    assert_eq!(server.requests().len(), requests.len(), "the private fetch sent nothing");
    let _ = std::fs::remove_dir_all(&root);
}

// Fetch from a real upload-pack backend, then prove receive-pack got the exact
// same compressed bytes, a fixed Content-Length and one freshly minted token per
// request. No packbuilder invocation can satisfy the byte-for-byte assertion by
// chance (the fixture source is deliberately packed with Git's reuse settings).
fn fetched_staging(root: &Path) -> (git2::Repository, String, String) {
    let source = root.join("source.git");
    let oid = bare_with_commit(&source, "refs/heads/main");
    let repo = git2::Repository::open_bare(&source).unwrap();
    repo.config().unwrap().set_bool("uploadpack.allowReachableSHA1InWant", true).unwrap();
    let server = GitHttpAuthServer::spawn_with(&source, "/source", git_http_fixture::FixtureOptions { allow_anonymous:true, ..Default::default() });
    let staging = git2::Repository::init_bare(root.join("fetched.git")).unwrap();
    maxplayer_core::git_transport::fetch_private_input_base(&staging,&server.repo_url(),&oid,None).unwrap();
    let packs = std::fs::read_dir(staging.path().join("objects/pack")).unwrap()
        .map(|e| e.unwrap().path()).filter(|p| p.extension().is_some_and(|e| e=="pack")).collect::<Vec<_>>();
    assert_eq!(packs.len(),1);
    use sha2::{Digest,Sha256};
    let hash=hex::encode(Sha256::digest(std::fs::read(&packs[0]).unwrap()));
    (staging,oid,hash)
}
fn counted_auth(url: &str) -> AuthMinter {
    let intended=url.to_owned();
    let count=std::sync::atomic::AtomicUsize::new(0);
    Arc::new(move |destination| {
        assert_eq!(destination,intended);
        Ok(format!("Nostr request-{}",count.fetch_add(1,std::sync::atomic::Ordering::SeqCst)))
    })
}
#[test]
fn fetched_pack_forwarding_preserves_bytes_and_reconciles_lost_response() {
    init_test_env();
    for lose in [false,true] {
        let root=tempfile::tempdir().unwrap();
        let (staging,oid,hash)=fetched_staging(root.path());
        let dest=git2::Repository::init_bare(root.path().join("dest.git")).unwrap();
        let server=GitHttpAuthServer::spawn_with(dest.path(),&private_mount(),git_http_fixture::FixtureOptions {lose_receive_pack_response:lose,..Default::default()});
        let reference=format!("refs/heads/input/{}","ab".repeat(32));
        let url=server.repo_url();
        assert_eq!(push_private_input(&staging,&url,&reference,&oid,counted_auth(&url)).unwrap(),oid);
        assert_eq!(dest.refname_to_id(&reference).unwrap().to_string(),oid);
        let requests=server.requests();
        let posts=requests.iter().filter(|r|r.method=="POST").collect::<Vec<_>>();
        assert_eq!(posts.len(),1,"lost ACK must reconcile instead of reposting");
        assert_eq!(posts[0].pack_sha256.as_deref(),Some(hash.as_str()));
        assert!(posts[0].content_length.unwrap()>32);
        assert_eq!(requests.len(),if lose {3} else {2});
        for (i,r) in requests.iter().enumerate() {assert_eq!(r.authorization,Some(format!("Nostr request-{i}")));}
    }
}
#[test]
fn forward_pack_redirect_on_either_leg_is_refused() {
    init_test_env();
    for leg in [1,2] {
        let root=tempfile::tempdir().unwrap();
        let (staging,oid,_)=fetched_staging(root.path());
        let dest=git2::Repository::init_bare(root.path().join("dest.git")).unwrap();
        let trap=std::net::TcpListener::bind("127.0.0.1:0").unwrap();trap.set_nonblocking(true).unwrap();
        let server=GitHttpAuthServer::spawn_with(dest.path(),&private_mount(),git_http_fixture::FixtureOptions {
            redirect_to:Some(format!("https://{}/stolen",trap.local_addr().unwrap())),redirect_method:Some(if leg == 1 {"GET"} else {"POST"}),..Default::default()});
        let url=server.repo_url();
        assert!(push_private_input(&staging,&url,&format!("refs/heads/input/{}","ab".repeat(32)),&oid,counted_auth(&url)).is_err());
        assert!(trap.accept().is_err());
        assert_eq!(dest.references().unwrap().count(),0);
    }
}
#[test]
fn forward_pack_refuses_bad_status_on_wire() {
    init_test_env();
    fn pkt(s:&str)->String{format!("{:04x}{s}",s.len()+4)}
    for reply in ["0000".to_owned(),format!("{}{}0000",pkt("unpack ok\n"),pkt("ok refs/heads/wrong\n")),
        format!("{}{}0000",pkt("unpack ok\n"),pkt("ng refs/heads/input/test denied\n")),
        format!("{}0000",pkt("unpack bad pack\n"))] {
        let root=tempfile::tempdir().unwrap();let (staging,oid,_)=fetched_staging(root.path());
        let dest=git2::Repository::init_bare(root.path().join("dest.git")).unwrap();
        let server=GitHttpAuthServer::spawn_with(dest.path(),&private_mount(),git_http_fixture::FixtureOptions{receive_status:Some(reply.into_bytes()),..Default::default()});
        let url=server.repo_url();
        assert!(push_private_input(&staging,&url,&format!("refs/heads/input/{}","ab".repeat(32)),&oid,counted_auth(&url)).is_err());
        assert_eq!(server.requests().len(),6,"three explicit attempts retain the existing retry policy");
    }
}

#[test]
fn forward_conditions_fall_back_to_libgit2() {
    init_test_env();
    for condition in ["two-packs","loose-object","non-empty","no-report-status","no-ofs-delta"] {
        let root=tempfile::tempdir().unwrap();
        let (staging,oid,_)=fetched_staging(root.path());
        let dest=git2::Repository::init_bare(root.path().join("dest.git")).unwrap();
        let mut options=git_http_fixture::FixtureOptions::default();
        match condition {
            "two-packs" => {
                let path=std::fs::read_dir(staging.path().join("objects/pack")).unwrap().map(|e|e.unwrap().path()).find(|p|p.extension().is_some_and(|e|e=="pack")).unwrap();
                std::fs::copy(path,staging.path().join(format!("objects/pack/pack-{}.pack","ee".repeat(20)))).unwrap();
            }
            "loose-object" => { staging.blob(b"an additional input snapshot object").unwrap(); }
            "non-empty" => { bare_with_commit(dest.path(),"refs/heads/main"); }
            other => {
                let caps=if other=="no-report-status" {"ofs-delta"} else {"report-status"};
                let line=format!("{} capabilities^{{}}\0{caps}\n",git2::Oid::zero());
                let prelude="# service=git-receive-pack\n";
                options.receive_advertisement=Some(format!("{:04x}{prelude}0000{:04x}{line}0000",prelude.len()+4,line.len()+4).into_bytes());
            }
        }
        let server=GitHttpAuthServer::spawn_with(dest.path(),&private_mount(),options);
        let url=server.repo_url();
        let result=push_private_input(&staging,&url,&format!("refs/heads/input/{}","ab".repeat(32)),&oid,counted_auth(&url));
        if condition!="no-report-status" {assert!(result.is_ok(),"{condition}: {result:?}");}
        let requests=server.requests();
        assert!(requests.iter().filter(|r|r.method=="GET").count()>=2,"{condition}: libgit2 must make its own advertisement request");
    }
}

#[test]
fn forward_candidate_still_refuses_repo_local_destination_rewrite() {
    init_test_env();
    let root=tempfile::tempdir().unwrap(); let (staging,oid,_)=fetched_staging(root.path());
    let intended="https://relay.example.invalid/git/owner/job";
    staging.config().unwrap().set_str("url.https://attacker.example.invalid/.insteadOf", "https://relay.example.invalid/").unwrap();
    let mint:AuthMinter=Arc::new(|_|panic!("must reject before signing or sending"));
    assert!(push_private_input(&staging,intended,&format!("refs/heads/input/{}","ab".repeat(32)),&oid,mint).is_err());
}
