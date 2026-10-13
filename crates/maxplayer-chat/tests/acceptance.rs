use futures_util::{SinkExt, StreamExt};
use maxplayer_chat::{Home, envelope, watch};
use maxplayer_core::private_content::transport;
use nostr_relay_builder::{LocalRelay, RelayBuilder};
use nostr_sdk::prelude::*;
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{net::TcpListener, task::JoinHandle};

// A real nostr-relay-builder stores and streams events. This small front adds Buzz's
// challenge-on-connect / restricted-before-auth rule (builder otherwise challenges lazily).
#[derive(Default)]
struct Faults {
    refuse_auth: AtomicUsize,
    omit_auth: AtomicBool,
    omit_eose: AtomicBool,
    wrong_eose: AtomicBool,
    closed: AtomicBool,
    drop_socket: AtomicBool,
    reject_next: AtomicUsize,
    refusal: Mutex<String>,
    omit_ok: AtomicBool,
    omit_next_ok: AtomicUsize,
    ids: Mutex<Vec<String>>,
}
fn take_fault(value: &AtomicUsize) -> bool {
    let mut n = value.load(Ordering::SeqCst);
    while n > 0 {
        match value.compare_exchange(n, n - 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(actual) => n = actual,
        }
    }
    false
}
struct RelayFixture {
    faults: Arc<Faults>,
    backend: LocalRelay,
    url: String,
    task: JoinHandle<()>,
    stop: tokio::sync::watch::Sender<bool>,
    reqs: Arc<Mutex<Vec<bool>>>,
    reject_writes: Arc<AtomicBool>,
    writes: Arc<AtomicUsize>,
}
impl RelayFixture {
    async fn new() -> Self {
        // The SDK probes a free port and then binds it separately. Retry only that
        // allocation race, with a fresh relay/address and a bounded attempt count.
        for attempt in 0..8 {
            let backend = LocalRelay::new(
                RelayBuilder::default()
                    .auth_dm(false)
                    .max_filter_limit(1000)
                    .rate_limit(nostr_relay_builder::builder::RateLimit {
                        max_reqs: 500,
                        notes_per_minute: 10000,
                    }),
            );
            match backend.run().await {
                Ok(()) => return Self::start(backend, 0, Arc::default()).await,
                Err(nostr_relay_builder::Error::IO(e))
                    if e.kind() == std::io::ErrorKind::AddrInUse && attempt < 7 => {}
                Err(e) => panic!("local relay startup failed: {e}"),
            }
        }
        unreachable!()
    }
    async fn start(backend: LocalRelay, port: u16, reqs: Arc<Mutex<Vec<bool>>>) -> Self {
        let faults = Arc::new(Faults::default());
        let injected_faults = faults.clone();
        let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let backend_url = backend.url().await.to_string();
        let (stop, rx) = tokio::sync::watch::channel(false);
        let reject_writes = Arc::new(AtomicBool::new(false));
        let writes = Arc::new(AtomicUsize::new(0));
        let rejects = reject_writes.clone();
        let published = writes.clone();
        let recorded = reqs.clone();
        let public_url = url.clone();
        let task = tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let faults = injected_faults.clone();
                let refuse_auth = take_fault(&faults.refuse_auth);
                let backend_url = backend_url.clone();
                let reqs = recorded.clone();
                let reject_writes = rejects.clone();
                let writes = published.clone();
                let mut rx = rx.clone();
                let url = public_url.clone();
                tokio::spawn(async move {
                    let mut front = tokio_tungstenite::accept_async(socket).await.unwrap();
                    let (mut back, _) =
                        tokio_tungstenite::connect_async(backend_url).await.unwrap();
                    let mut challenge = "chat-fixture-challenge";
                    front
                        .send(json!(["AUTH", challenge]).to_string().into())
                        .await
                        .unwrap();
                    let mut authenticated = false;
                    loop {
                        tokio::select! {
                            _=rx.changed()=>break,
                            frame=front.next()=>{let Some(Ok(frame))=frame else{break};if !frame.is_text(){continue}let v:Value=serde_json::from_str(frame.to_text().unwrap()).unwrap();
                                if v[0]=="AUTH" {let event:Event=serde_json::from_value(v[1].clone()).unwrap();
                                    if faults.omit_auth.load(Ordering::SeqCst) { continue; }
                                    authenticated=!refuse_auth && event.verify().is_ok() && event.kind==Kind::Authentication && event.tags.iter().any(|t|t.as_slice()==["challenge",challenge]) && event.tags.iter().any(|t|t.as_slice()==["relay",url.as_str()]);
                                    front.send(json!(["OK",event.id.to_hex(),authenticated,""]).to_string().into()).await.unwrap();
                                }else{if v[0]=="REQ"{
                                    if faults.drop_socket.load(Ordering::SeqCst) { break; }
                                    if faults.closed.load(Ordering::SeqCst) {
                                        let _ = front.send(json!(["CLOSED",v[1],"blocked: fixture\n\u{202e}"]).to_string().into()).await;
                                        continue;
                                    }
                                    reqs.lock().unwrap().push(authenticated);if !authenticated{front.send(json!(["CLOSED",v[1],"restricted: auth first"]).to_string().into()).await.unwrap();continue;}}
                                    if v[0]=="EVENT" {
                                        writes.fetch_add(1,Ordering::SeqCst);
                                        faults.ids.lock().unwrap().push(v[1]["id"].as_str().unwrap().into());
                                        if reject_writes.load(Ordering::SeqCst) || take_fault(&faults.reject_next) {
                                            let reason = { let configured = faults.refusal.lock().unwrap(); if configured.is_empty() { "auth-required: test refusal\u{202e}\u{200b}".to_string() } else { configured.clone() } };
                                            front.send(json!(["OK",v[1]["id"],false,reason]).to_string().into()).await.unwrap();
                                            challenge="chat-fixture-rechallenge";
                                            let _=front.send(json!(["AUTH",challenge]).to_string().into()).await;
                                            continue;
                                        }
                                    }
                                    if back.send(frame).await.is_err(){break}
                                }
                            },
                            frame=back.next()=>{let Some(Ok(mut frame))=frame else{break};
                                if frame.is_text() {
                                    let v: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
                                    if v[0]=="OK" && (faults.omit_ok.load(Ordering::SeqCst) || take_fault(&faults.omit_next_ok)) { continue; }
                                    if v[0]=="EOSE" {
                                        if faults.omit_eose.load(Ordering::SeqCst) { continue; }
                                        if faults.wrong_eose.load(Ordering::SeqCst) { frame = json!(["EOSE", "wrong-subscription"]).to_string().into(); }
                                    }
                                }
                                if front.send(frame).await.is_err(){break}},
                        }
                    }
                });
            }
        });
        Self {
            faults,
            backend,
            url,
            task,
            stop,
            reqs,
            reject_writes,
            writes,
        }
    }
    async fn restart(self) -> Self {
        let port = self.url.rsplit(':').next().unwrap().parse().unwrap();
        let backend = self.backend.clone();
        let reqs = self.reqs.clone();
        self.task.abort();
        let _ = self.stop.send(true);
        tokio::time::sleep(Duration::from_millis(100)).await;
        Self::start(backend, port, reqs).await
    }
}
impl Drop for RelayFixture {
    fn drop(&mut self) {
        self.task.abort();
        let _ = self.stop.send(true);
    }
}
fn home() -> (tempfile::TempDir, Home) {
    let dir = tempfile::tempdir().unwrap();
    let home = Home::open(dir.path().join("home")).unwrap();
    (dir, home)
}
fn approve(a: &Home, b: &Home, name: &str) {
    a.add_peer(&b.keys.public_key().to_hex(), name).unwrap();
}
fn inbox(h: &Home, json: bool) -> String {
    let mut out = Vec::new();
    h.print_inbox(json, &mut out).unwrap();
    String::from_utf8(out).unwrap()
}
async fn wait_entries(h: &Home, n: usize) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if h.entries().unwrap().len() >= n {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("watch stored message");
}
fn watcher(h: &Home, url: &str, notify: Vec<String>) -> JoinHandle<()> {
    let path = h.root.clone();
    let url = url.to_string();
    tokio::spawn(async move {
        watch(&Home::open(path).unwrap(), &url, notify)
            .await
            .unwrap();
    })
}
async fn injected(h: &Home, sender: &Keys, body: String) -> Option<String> {
    let event = transport::wrap(sender, h.keys.public_key(), body)
        .await
        .unwrap();
    h.receive(&event).await.unwrap()
}

#[tokio::test]
async fn a1_round_trip() {
    let relay = RelayFixture::new().await;
    let (_a, a) = home();
    let (_b, b) = home();
    approve(&a, &b, "B");
    approve(&b, &a, "A");
    let wa = watcher(&a, &relay.url, vec![]);
    let wb = watcher(&b, &relay.url, vec![]);
    let sent = async_cli(&a.root, &["--relay", &relay.url, "send", "B", "hello"]).await;
    assert!(
        sent.status.success(),
        "{}",
        String::from_utf8_lossy(&sent.stderr)
    );
    wait_entries(&b, 1).await;
    assert!(
        String::from_utf8(cli(&b.root, &["inbox"]).stdout)
            .unwrap()
            .contains("hello")
    );
    assert!(inbox(&b, false).is_empty());
    b.send(&relay.url, "A", "reply").await.unwrap();
    wait_entries(&a, 2).await;
    assert!(inbox(&a, false).contains("reply"));
    assert!(inbox(&a, false).is_empty());
    let ae = a.entries().unwrap();
    let be = b.entries().unwrap();
    assert_eq!(ae[0].rumor_id, be[0].rumor_id);
    assert_eq!(ae[1].rumor_id, be[1].rumor_id);
    assert_eq!(ae[0].text, be[0].text);
    assert_eq!(ae[1].text, be[1].text);
    wa.abort();
    wb.abort();
    assert!(relay.reqs.lock().unwrap().iter().all(|v| *v));
}
#[tokio::test]
async fn a2_approval() {
    let (_d, h) = home();
    let stranger = Keys::generate();
    assert!(
        injected(&h, &stranger, envelope("hello").unwrap())
            .await
            .is_none()
    );
    h.add_peer(&stranger.public_key().to_hex(), "friend")
        .unwrap();
    let wrong = transport::wrap(
        &stranger,
        Keys::generate().public_key(),
        envelope("wrong").unwrap(),
    )
    .await
    .unwrap();
    assert!(h.receive(&wrong).await.unwrap().is_none());
    assert!(h.entries().unwrap().is_empty());
    assert!(inbox(&h, true).is_empty());
    h.remove_peer("friend").unwrap();
    // A label that looks like somebody else's key must never grant that key approval.
    h.add_peer(
        &Keys::generate().public_key().to_hex(),
        &stranger.public_key().to_hex(),
    )
    .unwrap();

    assert!(
        injected(&h, &stranger, envelope("removed").unwrap())
            .await
            .is_none()
    );
    assert!(h.entries().unwrap().is_empty());
}
#[tokio::test]
async fn a3_domain_separation() {
    let (_d, h) = home();
    let sender = Keys::generate();
    h.add_peer(&sender.public_key().to_hex(), "friend").unwrap();
    let payment=json!({"id":"job-correlation","mint":"https://mint.invalid","unit":"sat","proofs":[{"amount":1,"id":"0011223344556677","secret":"TOKEN-SENTINEL","C":format!("02{}",sender.public_key().to_hex())}]}).to_string();
    assert!(
        maxplayer_core::payment_send::parse_nip17_payment_payload(&payment, sender.public_key())
            .is_ok()
    );
    assert!(injected(&h, &sender, payment).await.is_none());
    for body in [
        r#"{"id":"job-id","mint":"https://mint.invalid","unit":"sat","proofs":[{"secret":"TOKEN-SENTINEL"}]}"#,
        r#"{"schema":"maxplayer.content-envelope.v2","nonce":"PRIVATE-SENTINEL","body_b64":"eyJ0ZXh0IjoicHJpdmF0ZSJ9"}"#,
        r#"{"schema":"maxplayer-chat/1","text":"hi","token":"EXTRA"}"#,
        r#"{"schema":"other","text":"hi"}"#,
    ] {
        assert!(injected(&h, &sender, body.into()).await.is_none());
    }
    assert!(inbox(&h, true).is_empty());
    assert!(h.entries().unwrap().is_empty());
    let chat = transport::wrap(&sender, h.keys.public_key(), envelope("chat").unwrap())
        .await
        .unwrap();
    assert!(
        maxplayer_core::seller::unwrap_own_payment_gift_wrap(&h.keys, &chat)
            .await
            .unwrap()
            .is_none()
    );
}
#[test]
fn a4_separate_product_same_identity() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let missing_home = std::process::Command::new(env!("CARGO_BIN_EXE_maxplayer-chat"))
        .current_dir(dir.path())
        .env_remove("HOME")
        .env_remove("MAXPLAYER_HOME")
        .arg("whoami")
        .output()
        .unwrap();
    assert!(!missing_home.status.success());
    assert!(!dir.path().join(".maxplayer").exists());
    let env_root = dir.path().join("env-home");
    let by_env = std::process::Command::new(env!("CARGO_BIN_EXE_maxplayer-chat"))
        .env_remove("HOME")
        .env("MAXPLAYER_HOME", &env_root)
        .arg("whoami")
        .output()
        .unwrap();
    assert!(by_env.status.success());
    assert!(env_root.join("key").exists());
    let root = dir.path().join("fresh");
    let out = cli(&root, &["whoami"]);
    assert!(out.status.success());
    let key = std::fs::read_to_string(root.join("key")).unwrap();
    assert_eq!(key.trim().len(), 64);
    assert!(
        key.trim()
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    );
    let mut names: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["chat", "key"]);
    assert_eq!(
        std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(root.join("key"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let boot = maxplayer_core::home::bootstrap(&root).unwrap();
    assert_eq!(
        maxplayer_core::home::read_secret_key_hex(&boot).unwrap(),
        key.trim()
    );
    assert_eq!(std::fs::read_to_string(root.join("key")).unwrap(), key);
    assert_eq!(out.stdout, cli(&root, &["whoami"]).stdout);
    let hex = maxplayer_core::home::public_key_hex(&boot).unwrap();
    assert!(String::from_utf8(out.stdout).unwrap().contains(&hex));
    for invalid in [
        "0".repeat(64),
        "g".repeat(64),
        "a".repeat(63),
        "f".repeat(64),
    ] {
        let invalid_root = dir.path().join("invalid");
        std::fs::create_dir_all(&invalid_root).unwrap();
        std::fs::write(invalid_root.join("key"), &invalid).unwrap();
        let output = cli(&invalid_root, &["whoami"]);
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains(&invalid));
        assert_eq!(
            std::fs::read_to_string(invalid_root.join("key")).unwrap(),
            invalid
        );
    }
}
#[tokio::test]
async fn a5_cap() {
    let relay = RelayFixture::new().await;
    let (_d, h) = home();
    let p = Keys::generate().public_key().to_hex();
    let other = Keys::generate().public_key().to_hex();
    h.add_peer(&p, "p").unwrap();
    h.add_peer(&other, "other").unwrap();
    for i in 0..50 {
        h.send(&relay.url, "p", &format!("send {i}")).await.unwrap();
    }
    assert!(
        h.send(&relay.url, "p", "51st")
            .await
            .unwrap_err()
            .to_string()
            .contains("50")
    );
    h.send(&relay.url, "other", "unaffected").await.unwrap();
    assert_eq!(h.entries().unwrap().len(), 51);
    relay.reject_writes.store(true, Ordering::SeqCst);
    let before = relay.writes.load(Ordering::SeqCst);
    let failure = tokio::time::timeout(
        Duration::from_secs(5),
        h.send(&relay.url, "other", "refused once"),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(failure.to_string().contains("auth-required: test refusal"));
    assert_eq!(
        relay.writes.load(Ordering::SeqCst),
        before + 3,
        "three same-event attempts on auth-required rejection"
    );
    assert_eq!(h.entries().unwrap().len(), 51);
    let sent: Value =
        serde_json::from_slice(&std::fs::read(h.root.join("chat/sent-today")).unwrap()).unwrap();
    assert_eq!(
        sent["per_peer_counts"][&other], 1,
        "definitely rejected command refunds its one reservation"
    );
    h.reserve_send(&p, Timestamp::now().as_secs() + 86400)
        .unwrap();
}
#[tokio::test]
async fn a6_dedup() {
    let (_d, h) = home();
    let sender = Keys::generate();
    h.add_peer(&sender.public_key().to_hex(), "p").unwrap();
    let rumor = EventBuilder::private_msg_rumor(h.keys.public_key(), envelope("once").unwrap())
        .build(sender.public_key());
    let seal = EventBuilder::seal(&sender, &h.keys.public_key(), rumor)
        .await
        .unwrap()
        .sign(&sender)
        .await
        .unwrap();
    let a = EventBuilder::gift_wrap_from_seal(&h.keys.public_key(), &seal, []).unwrap();
    let b = EventBuilder::gift_wrap_from_seal(&h.keys.public_key(), &seal, []).unwrap();
    assert_ne!(a.id, b.id);
    assert!(h.receive(&a).await.unwrap().is_some());
    assert!(h.receive(&b).await.unwrap().is_none());
    let reopened = Home::open(&h.root).unwrap();
    assert!(reopened.receive(&a).await.unwrap().is_none());
    assert_eq!(inbox(&h, true).lines().count(), 1);
    assert!(inbox(&h, true).is_empty());
    assert_eq!(
        transport::receive_since(h.cursor().unwrap()),
        h.cursor().unwrap().saturating_sub(240)
    );
}
#[tokio::test]
async fn a7_length() {
    let (_d, h) = home();
    let sender = Keys::generate();
    h.add_peer(&sender.public_key().to_hex(), "p").unwrap();
    let long = "x".repeat(8001);
    assert!(
        injected(
            &h,
            &sender,
            json!({"schema":"maxplayer-chat/1","text":long}).to_string()
        )
        .await
        .is_none()
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let error = h.send(&url, "p", &long).await.unwrap_err();
    assert!(error.to_string().contains("8000"), "{error}");
    assert!(
        tokio::time::timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
    assert!(
        injected(&h, &sender, envelope(&"é".repeat(8000)).unwrap())
            .await
            .is_some()
    );
    assert!(
        injected(
            &h,
            &sender,
            r#"{"schema":"maxplayer-chat/1","text":""}"#.into()
        )
        .await
        .is_none()
    );
    assert_eq!(h.entries().unwrap().len(), 1);
}
fn cli(root: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_maxplayer-chat"))
        .arg("--home")
        .arg(root)
        .args(args)
        .output()
        .unwrap()
}
async fn async_cli(root: &Path, args: &[&str]) -> std::process::Output {
    let root = root.to_path_buf();
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        cli(&root, &args.iter().map(String::as_str).collect::<Vec<_>>())
    })
    .await
    .unwrap()
}
#[tokio::test]
async fn a8_secrets() {
    let (_d, h) = home();
    let secret = std::fs::read_to_string(h.root.join("key"))
        .unwrap()
        .trim()
        .to_string();
    let p = Keys::generate();
    h.add_peer(&p.public_key().to_hex(), "p").unwrap();
    injected(&h, &p, envelope("ordinary message").unwrap()).await;
    for args in [
        vec!["whoami"],
        vec!["inbox", "--json"],
        vec!["log", "p"],
        vec!["peer", "list"],
    ] {
        let out = cli(&h.root, &args);
        assert!(out.status.success());
        assert!(!String::from_utf8_lossy(&out.stdout).contains(&secret));
        assert!(!String::from_utf8_lossy(&out.stderr).contains(&secret));
    }
    for file in std::fs::read_dir(h.root.join("chat")).unwrap() {
        assert!(
            !std::fs::read_to_string(file.unwrap().path())
                .unwrap()
                .contains(&secret)
        );
    }
}
#[tokio::test]
async fn a9_output_contract() {
    let (_d, h) = home();
    let p = Keys::generate();
    h.add_peer(&p.public_key().to_hex(), "friend").unwrap();
    injected(&h, &p, envelope("one").unwrap()).await;
    let v: Value = serde_json::from_str(inbox(&h, true).trim()).unwrap();
    assert_eq!(v["untrusted"], true);
    assert_eq!(v["from"], p.public_key().to_bech32().unwrap());
    assert_eq!(v["name"], "friend");
    assert!(v["at"].is_u64());
    assert_eq!(v["text"], "one");
    injected(&h, &p, envelope("two").unwrap()).await;
    assert!(inbox(&h, false).starts_with("[untrusted message from friend]"));
    let out = cli(&h.root, &["inbox"]);
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("watch is not running; new messages are not being received")
    );
}
#[tokio::test]
async fn a10_stays_connected() {
    let relay = RelayFixture::new().await;
    let (_a, a) = home();
    let (_b, b) = home();
    approve(&a, &b, "B");
    approve(&b, &a, "A");
    let wb = watcher(&b, &relay.url, vec![]);
    a.send(&relay.url, "B", "online").await.unwrap();
    wait_entries(&b, 1).await;
    // Stop the public endpoint and all sessions. Store an outage message in the persistent
    // builder backend, then restart the same endpoint; only backfill can recover it.
    relay.task.abort();
    let _ = relay.stop.send(true);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let event = transport::wrap(&a.keys, b.keys.public_key(), envelope("offline").unwrap())
        .await
        .unwrap();
    let publisher = Client::new(a.keys.clone());
    publisher
        .add_relay(relay.backend.url().await)
        .await
        .unwrap();
    publisher.connect().await;
    publisher.send_event(&event).await.unwrap();
    publisher.shutdown().await;
    let relay = relay.restart().await;
    wait_entries(&b, 2).await;
    assert_eq!(inbox(&b, true).lines().count(), 2);
    assert!(inbox(&b, true).is_empty());
    assert!(relay.reqs.lock().unwrap().len() >= 2);
    assert!(relay.reqs.lock().unwrap().iter().all(|v| *v));
    wb.abort();
}
#[tokio::test]
async fn a11_notify() {
    let relay = RelayFixture::new().await;
    let (_a, a) = home();
    let (_b, b) = home();
    approve(&a, &b, "B");
    approve(&b, &a, "A");
    let record = b.root.join("chat/notify-record");
    let script = b.root.join("chat/notify.py");
    let release = b.root.join("chat/notify-release");
    std::fs::write(&script,"import os,sys,time,json\nwith open(sys.argv[1], 'a') as f: f.write(json.dumps({'argv':sys.argv,'env':dict(v.decode().split('=',1) for v in open('/proc/self/environ','rb').read().split(b'\\0') if v)})+'\\n'); f.flush()\nwhile not os.path.exists(sys.argv[2]): time.sleep(0.02)\n").unwrap();
    let wb = watcher(
        &b,
        &relay.url,
        vec![
            "python3".into(),
            script.to_str().unwrap().into(),
            record.to_str().unwrap().into(),
            release.to_str().unwrap().into(),
        ],
    );
    a.send(&relay.url, "B", "TEXT-SECRET-SENTINEL")
        .await
        .unwrap();
    wait_entries(&b, 1).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !record.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await
        }
    })
    .await
    .unwrap();
    assert!(b.watch_lock().is_err());
    let duplicate = cli(&b.root, &["watch"]);
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("already running"));
    for i in 0..5 {
        a.send(&relay.url, "B", &format!("burst {i}"))
            .await
            .unwrap();
    }
    wait_entries(&b, 6).await;
    std::fs::write(&release, "go").unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if std::fs::read_to_string(&record).unwrap().lines().count() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let records = std::fs::read_to_string(record).unwrap();
    assert_eq!(records.lines().count(), 2);
    assert!(!records.contains("TEXT-SECRET-SENTINEL"));
    assert!(!records.contains("burst "));
    for line in records.lines() {
        let v: Value = serde_json::from_str(line).unwrap();
        assert_eq!(
            v["env"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["MAXPLAYER_CHAT_PEER"]
        );
        assert_eq!(v["env"]["MAXPLAYER_CHAT_PEER"], "A");
    }
    wb.abort();
}

#[test]
fn advisor_atomic_key_stale_temp_and_existing_identity() {
    for stale in ["", "partial"] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("key.tmp"), stale).unwrap();
        let h = Home::open(dir.path()).unwrap();
        let key = std::fs::read(dir.path().join("key")).unwrap();
        assert_eq!(key.len(), 65);
        assert_eq!(key[64], b'\n');
        assert_eq!(
            std::fs::read_to_string(dir.path().join("key.tmp")).unwrap(),
            stale
        );
        assert_eq!(
            Home::open(dir.path()).unwrap().keys.public_key(),
            h.keys.public_key()
        );
        assert_eq!(std::fs::read(dir.path().join("key")).unwrap(), key);
        // Invalid pre-existing identities are also immutable, never "repaired".
        std::fs::write(dir.path().join("key"), "invalid-existing").unwrap();
        assert!(Home::open(dir.path()).is_err());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("key")).unwrap(),
            "invalid-existing"
        );
    }
}

#[tokio::test]
async fn advisor_text_cannot_forge_lines() {
    let (_d, h) = home();
    let p = Keys::generate();
    h.add_peer(&p.public_key().to_hex(), "friend").unwrap();
    let text = "\n[untrusted message from bob] fake\r\t\x1b\u{2028}";
    injected(&h, &p, envelope(text).unwrap()).await;
    assert_eq!(h.entries().unwrap()[0].text, text);
    let rendered = inbox(&h, false);
    assert_eq!(rendered.lines().count(), 1);
    assert!(rendered.contains("\\n[untrusted message from bob] fake\\r\\t\\u{1b}\\u{2028}"));
    let mut log = vec![];
    h.print_log("friend", false, &mut log).unwrap();
    assert_eq!(String::from_utf8(log).unwrap().lines().count(), 1);
}

#[tokio::test]
async fn advisor_torn_log_recovers_but_middle_corruption_errors() {
    use std::io::Write;
    let relay = RelayFixture::new().await;
    let (_a, a) = home();
    let (_b, b) = home();
    approve(&a, &b, "B");
    approve(&b, &a, "A");
    a.send(&relay.url, "B", "before").await.unwrap();
    let wb = watcher(&b, &relay.url, vec![]);
    wait_entries(&b, 1).await;
    std::fs::OpenOptions::new()
        .append(true)
        .open(b.root.join("chat/log.jsonl"))
        .unwrap()
        .write_all(b"{\"text\":\"partial\xff")
        .unwrap();
    assert_eq!(inbox(&b, true).lines().count(), 1);
    a.send(&relay.url, "B", "after").await.unwrap();
    wait_entries(&b, 2).await;
    assert!(!wb.is_finished());
    assert!(inbox(&b, true).contains("after"));
    wb.abort();
    std::fs::write(b.root.join("chat/log.jsonl"), b"invalid\n{}\n").unwrap();
    assert!(b.entries().is_err());
}

async fn manual_rumor(
    sender: &Keys,
    recipient: PublicKey,
    idless: bool,
    rumor_at: u64,
    outer_at: u64,
) -> Event {
    let mut rumor = EventBuilder::private_msg_rumor(recipient, envelope("manual").unwrap())
        .custom_created_at(Timestamp::from(rumor_at))
        .build(sender.public_key());
    if !idless {
        rumor.ensure_id();
    }
    let mut value = serde_json::to_value(&rumor).unwrap();
    if idless {
        value.as_object_mut().unwrap().remove("id");
    }
    // Deliberately do NOT use EventBuilder::seal: it fills the missing rumor ID.
    let encrypted = nostr_sdk::nostr::nips::nip44::encrypt(
        sender.secret_key(),
        &recipient,
        value.to_string(),
        nostr_sdk::nostr::nips::nip44::Version::default(),
    )
    .unwrap();
    let seal = EventBuilder::new(Kind::Seal, encrypted)
        .sign_with_keys(sender)
        .unwrap();
    let ephemeral = Keys::generate();
    let encrypted = nostr_sdk::nostr::nips::nip44::encrypt(
        ephemeral.secret_key(),
        &recipient,
        seal.as_json(),
        nostr_sdk::nostr::nips::nip44::Version::default(),
    )
    .unwrap();
    EventBuilder::new(Kind::GiftWrap, encrypted)
        .tags([Tag::public_key(recipient)])
        .custom_created_at(Timestamp::from(outer_at))
        .sign_with_keys(&ephemeral)
        .unwrap()
}

#[tokio::test]
async fn advisor_idless_rumors_do_not_kill_watch() {
    let relay = RelayFixture::new().await;
    let (_a, a) = home();
    let (_b, b) = home();
    approve(&a, &b, "B");
    approve(&b, &a, "A");
    let wb = watcher(&b, &relay.url, vec![]);
    a.send(&relay.url, "B", "initial").await.unwrap();
    wait_entries(&b, 1).await;
    let before = std::fs::read(b.root.join("chat/log.jsonl")).unwrap();
    let publisher = Client::new(Keys::generate());
    publisher
        .add_relay(relay.backend.url().await)
        .await
        .unwrap();
    publisher.connect().await;
    for sender in [Keys::generate(), a.keys.clone()] {
        let event = manual_rumor(
            &sender,
            b.keys.public_key(),
            true,
            1,
            Timestamp::now().as_secs(),
        )
        .await;
        let gift = UnwrappedGift::from_gift_wrap(&b.keys, &event)
            .await
            .unwrap();
        assert!(gift.rumor.id.is_none());
        publisher.send_event(&event).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!wb.is_finished(), "bad event killed watch");
        assert_eq!(
            std::fs::read(b.root.join("chat/log.jsonl")).unwrap(),
            before
        );
    }
    a.send(&relay.url, "B", "later valid").await.unwrap();
    wait_entries(&b, 2).await;
    assert!(inbox(&b, true).contains("later valid"));
    publisher.shutdown().await;
    wb.abort();
}

#[tokio::test]
async fn advisor_timestamp_uses_clamped_outer() {
    let (_d, h) = home();
    let p = Keys::generate();
    h.add_peer(&p.public_key().to_hex(), "p").unwrap();
    let now = Timestamp::now().as_secs();
    for (rumor_at, outer_at) in [(1, now - 10), (u32::MAX as u64, now + 60)] {
        let event = manual_rumor(&p, h.keys.public_key(), false, rumor_at, outer_at).await;
        let before = Timestamp::now().as_secs();
        assert!(h.receive(&event).await.unwrap().is_some());
        let at = h.entries().unwrap().last().unwrap().at;
        assert!(at >= outer_at.min(before) && at <= outer_at.min(Timestamp::now().as_secs()));
    }
}

#[test]
fn advisor_notify_spawn_failure_retries() {
    use maxplayer_chat::Notifier;
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let executable = dir.path().join("notify");
    let record = dir.path().join("record");
    let mut notifier = Notifier::new(vec![
        executable.to_str().unwrap().into(),
        record.to_str().unwrap().into(),
    ]);
    notifier.message("friend".into());
    assert!(notifier.tick().is_err());
    let touch = std::process::Command::new("which")
        .arg("touch")
        .output()
        .unwrap();
    symlink(String::from_utf8(touch.stdout).unwrap().trim(), &executable).unwrap();
    notifier.tick().unwrap();
    for _ in 0..100 {
        if record.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(record.exists(), "pending wakeup was lost");
}

#[test]
fn advisor_bad_argv_creates_no_home() {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        vec!["bogus"],
        vec!["watch", "--notify"],
        vec!["inbox", "extra"],
    ] {
        let root = dir.path().join("unused");
        assert!(!cli(&root, &args).status.success());
        assert!(!root.exists(), "invalid argv initialized a home");
    }
}

#[tokio::test]
async fn security_inbound_cap_is_per_peer_without_advancing_checkpoint() {
    let (_d, h) = home();
    let sender = Keys::generate();
    let other = Keys::generate();
    h.add_peer(&sender.public_key().to_hex(), "sender").unwrap();
    h.add_peer(&other.public_key().to_hex(), "other").unwrap();
    for i in 0..maxplayer_chat::INBOUND_DAILY_CAP {
        // Old outer timestamps must still consume today's local receipt quota.
        let event = manual_rumor(&sender, h.keys.public_key(), false, i as u64 + 1, 1).await;
        assert!(h.receive(&event).await.unwrap().is_some());
    }
    // Reopening and remove/re-add must not reset the local receipt-day count.
    h.remove_peer("sender").unwrap();
    h.add_peer(&sender.public_key().to_hex(), "sender").unwrap();
    let h = Home::open(&h.root).unwrap();
    std::fs::write(h.root.join("chat/cursor"), "0").unwrap();
    let event = manual_rumor(&sender, h.keys.public_key(), false, 10001, 1).await;
    let dropped_id = UnwrappedGift::from_gift_wrap(&h.keys, &event)
        .await
        .unwrap()
        .rumor
        .id
        .unwrap()
        .to_hex();
    assert!(
        h.receive(&event).await.unwrap().is_none(),
        "cap+1 must be dropped"
    );
    assert_eq!(
        h.cursor().unwrap(),
        0,
        "quota drop must not advance checkpoint"
    );
    assert!(
        std::fs::read_to_string(h.root.join("chat/dropped.json"))
            .unwrap()
            .contains(&dropped_id)
    );
    assert!(
        injected(&h, &other, envelope("other peer unaffected").unwrap())
            .await
            .is_some()
    );
    let entries = h.entries().unwrap();
    assert_eq!(entries.len(), maxplayer_chat::INBOUND_DAILY_CAP + 1);
    assert!(!entries.iter().any(|e| e.rumor_id == dropped_id));
}

#[tokio::test]
async fn security_watch_recovers_from_append_failure() {
    use std::os::unix::fs::symlink;
    let relay = RelayFixture::new().await;
    let (_a, a) = home();
    let (_b, b) = home();
    approve(&a, &b, "B");
    approve(&b, &a, "A");
    let wb = watcher(&b, &relay.url, vec![]);
    a.send(&relay.url, "B", "initial").await.unwrap();
    wait_entries(&b, 1).await;
    let log = b.root.join("chat/log.jsonl");
    let saved = b.root.join("chat/saved-log");
    std::fs::rename(&log, &saved).unwrap();
    symlink(&saved, &log).unwrap(); // Read succeeds; O_NOFOLLOW append fails, even as root.
    let reqs = relay.reqs.lock().unwrap().len();
    a.send(&relay.url, "B", "failed append").await.unwrap();
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            assert!(!wb.is_finished(), "append failure ended watch");
            if relay.reqs.lock().unwrap().len() > reqs {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("watch must reconnect after append failure");
    assert_eq!(b.entries().unwrap().len(), 1);
    std::fs::remove_file(&log).unwrap();
    std::fs::rename(&saved, &log).unwrap();
    a.send(&relay.url, "B", "later valid").await.unwrap();
    wait_entries(&b, 2).await;
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            assert!(!wb.is_finished());
            if b.entries().unwrap().iter().any(|e| e.text == "later valid") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        b.entries()
            .unwrap()
            .iter()
            .filter(|e| e.text == "later valid")
            .count(),
        1
    );
    wb.abort();
}

const VISUAL_TEXT: &str = "hello\u{202e}\u{200b}\u{200d}world";

#[tokio::test]
async fn security_text_and_labels_escape_visual_formats() {
    let (_d, h) = home();
    let sender = Keys::generate();
    h.add_peer(&sender.public_key().to_hex(), "p\u{202e}\u{200b}")
        .unwrap();
    injected(&h, &sender, envelope(VISUAL_TEXT).unwrap()).await;
    for args in [
        vec!["inbox"],
        vec!["log", &sender.public_key().to_hex()],
        vec!["peer", "list"],
    ] {
        let output = cli(&h.root, &args);
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(!text.contains(['\u{202e}', '\u{200b}']));
        assert!(text.contains("\\u{202e}") && text.contains("\\u{200b}"));
        if args[0] != "peer" {
            assert!(text.contains('\u{200d}'));
        }
    }
    for c in [
        '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}', '\u{202e}', '\u{2066}', '\u{2067}',
        '\u{2068}', '\u{2069}', '\u{200e}', '\u{200f}', '\u{061c}', '\u{200b}', '\u{200c}',
        '\u{feff}', '\u{2060}',
    ] {
        assert_eq!(
            maxplayer_chat::display_text(&c.to_string()),
            c.escape_default().to_string()
        );
    }
}

#[tokio::test]
async fn security_json_escapes_preserve_original_text() {
    let (_d, h) = home();
    let sender = Keys::generate();
    h.add_peer(&sender.public_key().to_hex(), "p\u{202e}")
        .unwrap();
    injected(&h, &sender, envelope(VISUAL_TEXT).unwrap()).await;
    for args in [vec!["inbox", "--json"], vec!["log", "p\u{202e}", "--json"]] {
        let output = cli(&h.root, &args);
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(!text.contains(['\u{202e}', '\u{200b}']));
        assert!(text.contains("\\u202e") && text.contains("\\u200b"));
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["untrusted"], true);
        assert_eq!(value["text"], VISUAL_TEXT);
    }
    assert_eq!(h.entries().unwrap()[0].text, VISUAL_TEXT);
}

#[test]
fn security_whoami_tightens_existing_key() {
    use std::os::unix::fs::PermissionsExt;
    let (_d, h) = home();
    let key = h.root.join("key");
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(cli(&h.root, &["whoami"]).status.success());
    assert_eq!(
        std::fs::metadata(key).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn security_symlink_directories_are_refused_before_mutation() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    for chat in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        let root = dir.path().join("home");
        if chat {
            std::fs::create_dir(&root).unwrap();
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
            symlink(&target, root.join("chat")).unwrap();
        } else {
            symlink(&target, &root).unwrap();
        }
        for spelling in [root.clone(), root.join(""), root.join(".")] {
            assert!(Home::open(&spelling).is_err(), "symlink must be refused");
        }
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
        if chat {
            assert_eq!(
                std::fs::metadata(root).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
    }
}

#[tokio::test]
async fn security_relay_rejection_is_display_safe() {
    let relay = RelayFixture::new().await;
    relay.reject_writes.store(true, Ordering::SeqCst);
    let (_a, a) = home();
    let (_b, b) = home();
    approve(&a, &b, "B");
    let output = async_cli(&a.root, &["--relay", &relay.url, "send", "B", "hello"]).await;
    assert!(!output.status.success());
    let text = String::from_utf8(output.stderr).unwrap();
    assert_eq!(text.lines().count(), 1);
    assert!(!text.contains(['\u{202e}', '\u{200b}']));
    assert!(text.contains("\\u{202e}") && text.contains("\\u{200b}"));
}

fn fast_options() -> maxplayer_chat::WatchOptions {
    maxplayer_chat::WatchOptions {
        max_failures: 3,
        cycle_timeout: Duration::from_millis(500),
        liveness_interval: Duration::from_millis(200),
        liveness_timeout: Duration::from_millis(150),
        refresh_interval: Duration::from_millis(100),
        backoff_unit: Duration::from_millis(30),
    }
}
async fn wait_healthy(h: &Home) {
    wait_healthy_pid(h, None).await
}
async fn wait_healthy_pid(h: &Home, pid: Option<u32>) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while !h
            .status()
            .is_ok_and(|s| s.state == "healthy" && pid.is_none_or(|p| s.pid == p))
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("EOSE-verified healthy status");
}
struct WatchProcess(std::process::Child);
impl Drop for WatchProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn watch_process(h: &Home, url: &str, max: &str) -> WatchProcess {
    let stderr = std::fs::File::create(h.root.join("chat/watch-test.log")).unwrap();
    WatchProcess(
        std::process::Command::new(env!("CARGO_BIN_EXE_maxplayer-chat"))
            .args([
                "--home",
                h.root.to_str().unwrap(),
                "--relay",
                url,
                "watch",
                "--max-failures",
                max,
            ])
            .stdout(std::process::Stdio::null())
            .stderr(stderr)
            .spawn()
            .unwrap(),
    )
}
#[tokio::test]
async fn health_cli_ready_logs_status_and_process_restart_catchup() {
    let relay = RelayFixture::new().await;
    let (_a, a) = home();
    let (_b, b) = home();
    approve(&a, &b, "B");
    approve(&b, &a, "A");
    let first = watch_process(&b, &relay.url, "10");
    wait_healthy(&b).await;
    let status = cli(&b.root, &["status", "--json"]);
    assert!(status.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&status.stdout).unwrap()["state"],
        "healthy"
    );
    let logs = std::fs::read_to_string(b.root.join("chat/watch-test.log")).unwrap();
    assert!(
        logs.contains("authenticated") && logs.contains("subscribed, caught up 0 messages since"),
        "{logs}"
    );
    assert!(logs.contains(&relay.url) && logs.contains("attempt=1") && logs.contains("Z relay="));
    drop(first); // Actual OS process gone, retaining the durable home and relay.
    assert!(!cli(&b.root, &["status"]).status.success());
    for i in 0..4 {
        a.send(&relay.url, "B", &format!("offline-{i}"))
            .await
            .unwrap();
    }
    let second = watch_process(&b, &relay.url, "10");
    wait_healthy_pid(&b, Some(second.0.id())).await;
    assert_eq!(b.entries().unwrap().len(), 4);
    drop(second);
    let third = watch_process(&b, &relay.url, "10");
    wait_healthy_pid(&b, Some(third.0.id())).await;
    assert_eq!(
        b.entries().unwrap().len(),
        4,
        "overlap must dedup across process restarts"
    );
    assert_eq!(inbox(&b, true).lines().count(), 4);
    drop(third);
}
#[tokio::test]
async fn health_auth_failures_grow_backoff_and_cli_exits() {
    let relay = RelayFixture::new().await;
    relay.faults.refuse_auth.store(10, Ordering::SeqCst);
    let (_d, h) = home();
    let mut process = watch_process(&h, &relay.url, "3");
    let exit = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(status) = process.0.try_wait().unwrap() {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(exit.code(), Some(1));
    let stored: Value =
        serde_json::from_str(&std::fs::read_to_string(h.root.join("chat/status.json")).unwrap())
            .unwrap();
    assert_eq!(stored["state"], "unhealthy");
    assert_eq!(stored["consecutive_failures"], 3);
    let logs = std::fs::read_to_string(h.root.join("chat/watch-test.log")).unwrap();
    let delays: Vec<u64> = logs
        .lines()
        .filter_map(|l| l.split("next_delay_ms=").nth(1))
        .map(|v| v.parse().unwrap())
        .collect();
    assert_eq!(delays.len(), 3, "{logs}");
    assert!((1800..=2200).contains(&delays[0]), "{logs}");
    assert!((3600..=4400).contains(&delays[1]), "{logs}");
    assert!(logs.contains("auth: NIP-42 rejected") && logs.contains("attempt=3"));
    assert!(!logs.contains("subscribed") && !logs.contains(" authenticated"));
    assert!(relay.reqs.lock().unwrap().is_empty());
}
#[tokio::test]
async fn health_withheld_or_wrong_eose_and_missing_auth_never_ready() {
    for fault in ["omit", "wrong", "auth"] {
        let relay = RelayFixture::new().await;
        match fault {
            "omit" => relay.faults.omit_eose.store(true, Ordering::SeqCst),
            "wrong" => relay.faults.wrong_eose.store(true, Ordering::SeqCst),
            _ => relay.faults.omit_auth.store(true, Ordering::SeqCst),
        }
        let (_d, h) = home();
        let result = tokio::time::timeout(
            Duration::from_secs(4),
            maxplayer_chat::watch_with_options(&h, &relay.url, vec![], fast_options()),
        )
        .await
        .expect("failed readiness must stop within the bounded cycles");
        assert!(result.is_err());
        let stored: Value = serde_json::from_str(
            &std::fs::read_to_string(h.root.join("chat/status.json")).unwrap(),
        )
        .unwrap();
        assert!(stored["last_healthy_at"].is_null(), "{fault}: {stored}");
        assert_eq!(stored["consecutive_failures"], 3);
        assert_eq!(h.cursor().unwrap(), 0);
        if fault == "auth" {
            assert!(relay.reqs.lock().unwrap().is_empty());
        }
    }
}
#[tokio::test]
async fn health_dead_open_socket_liveness_and_refresh() {
    let relay = RelayFixture::new().await;
    let (_d, h) = home();
    let root = h.root.clone();
    let url = relay.url.clone();
    let task = tokio::spawn(async move {
        maxplayer_chat::watch_with_options(&Home::open(root).unwrap(), &url, vec![], fast_options())
            .await
    });
    wait_healthy(&h).await;
    // Prove periodic refresh repairs an artificially old snapshot while the lock stays held.
    let path = h.root.join("chat/status.json");
    let mut value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    value["updated_at"] = json!(1);
    std::fs::write(&path, value.to_string()).unwrap();
    tokio::time::sleep(Duration::from_millis(130)).await;
    assert!(h.status().unwrap().updated_at > 1);
    relay.faults.omit_eose.store(true, Ordering::SeqCst); // TCP stays open and answers ping, but REQ is deaf.
    tokio::time::timeout(Duration::from_secs(4), task)
        .await
        .expect("liveness must reconnect")
        .unwrap()
        .unwrap_err();
    let logs = std::fs::read_to_string(&path).unwrap();
    assert!(logs.contains("unhealthy"));
    assert!(
        relay.reqs.lock().unwrap().len() >= 4,
        "probe plus reconnects"
    );
}
#[test]
fn health_status_stale_and_lock_only_are_not_healthy() {
    let (_d, h) = home();
    let guard = h.watch_lock().unwrap();
    assert!(
        !cli(&h.root, &["status", "--json"]).status.success(),
        "lock alone is not readiness"
    );
    let path = h.root.join("chat/status.json");
    let mut value = json!({"state":"healthy", "pid":std::process::id(), "relay":"ws://127.0.0.1:1", "last_healthy_at":1,
        "consecutive_failures":0,"last_error":null,"updated_at":Timestamp::now().as_secs()-91});
    std::fs::write(&path, value.to_string()).unwrap();
    let status = cli(&h.root, &["status", "--json"]);
    assert!(!status.status.success());
    assert!(String::from_utf8_lossy(&status.stdout).contains("stale"));
    value["updated_at"] = json!(Timestamp::now().as_secs());
    std::fs::write(&path, value.to_string()).unwrap();
    assert!(cli(&h.root, &["status"]).status.success());
    drop(guard);
    assert!(
        !cli(&h.root, &["status"]).status.success(),
        "fresh file without lock must fail"
    );
}
#[tokio::test]
async fn health_closed_and_socket_drop_reasons() {
    for closed in [true, false] {
        let relay = RelayFixture::new().await;
        relay.faults.closed.store(closed, Ordering::SeqCst);
        relay.faults.drop_socket.store(!closed, Ordering::SeqCst);
        let (_d, h) = home();
        maxplayer_chat::watch_with_options(&h, &relay.url, vec![], fast_options())
            .await
            .unwrap_err();
        let value: Value = serde_json::from_str(
            &std::fs::read_to_string(h.root.join("chat/status.json")).unwrap(),
        )
        .unwrap();
        let error = value["last_error"].as_str().unwrap();
        assert!(
            error.contains(if closed {
                "CLOSED: blocked: fixture"
            } else {
                "disconnected"
            }),
            "{error}"
        );
        assert!(!error.contains(['\n', '\u{202e}']));
    }
}
#[tokio::test]
async fn retry_auth_required_second_attempt_same_event_one_delivery() {
    let relay = RelayFixture::new().await;
    relay.faults.reject_next.store(1, Ordering::SeqCst);
    let (_a, a) = home();
    let (_b, b) = home();
    approve(&a, &b, "B");
    approve(&b, &a, "A");
    let watcher = watcher(&b, &relay.url, vec![]);
    let result = async_cli(
        &a.root,
        &["--relay", &relay.url, "send", "B", "once", "--json"],
    )
    .await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let outcome: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(outcome["status"], "accepted");
    assert_eq!(outcome["attempts"], 2);
    wait_entries(&b, 1).await;
    assert_eq!(a.entries().unwrap().len(), 1);
    assert_eq!(b.entries().unwrap().len(), 1);
    assert_eq!(
        a.entries().unwrap()[0].rumor_id,
        b.entries().unwrap()[0].rumor_id
    );
    let ids = relay.faults.ids.lock().unwrap();
    assert_eq!(ids.len(), 2);
    assert_eq!(ids[0], ids[1]);
    assert_eq!(outcome["event_id"], ids[0]);
    watcher.abort();
}
#[tokio::test]
async fn retry_definite_refusal_not_retried_cap_refunded() {
    let relay = RelayFixture::new().await;
    let (_a, a) = home();
    let (_b, b) = home();
    approve(&a, &b, "B");
    for reason in [
        "blocked: policy",
        "invalid: bad event",
        "error: definite refusal",
    ] {
        *relay.faults.refusal.lock().unwrap() = reason.into();
        relay.faults.reject_next.store(1, Ordering::SeqCst);
        let outcome = a.send_outcome(&relay.url, "B", "refuse").await.unwrap();
        assert_eq!(outcome.status, "failed");
        assert_eq!(outcome.attempts, 1);
        assert_eq!(outcome.exit_code(), 1);
        let sent: Value =
            serde_json::from_str(&std::fs::read_to_string(a.root.join("chat/sent-today")).unwrap())
                .unwrap();
        assert_eq!(sent["per_peer_counts"][b.keys.public_key().to_hex()], 0);
    }
    assert_eq!(relay.writes.load(Ordering::SeqCst), 3);
    assert!(a.entries().unwrap().is_empty());
}
#[tokio::test]
async fn retry_unconfirmed_exit_two_keeps_cap_no_out_entry() {
    let relay = RelayFixture::new().await;
    relay.faults.omit_ok.store(true, Ordering::SeqCst);
    let (_a, a) = home();
    let (_b, b) = home();
    approve(&a, &b, "B");
    approve(&b, &a, "A");
    let watcher = watcher(&b, &relay.url, vec![]);
    let output = async_cli(
        &a.root,
        &["--relay", &relay.url, "send", "B", "maybe", "--json"],
    )
    .await;
    assert_eq!(output.status.code(), Some(2));
    let outcome: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(outcome["status"], "unconfirmed");
    assert_eq!(outcome["attempts"], 3);
    assert!(a.entries().unwrap().is_empty());
    assert_eq!(b.entries().unwrap().len(), 1);
    let sent: Value =
        serde_json::from_str(&std::fs::read_to_string(a.root.join("chat/sent-today")).unwrap())
            .unwrap();
    assert_eq!(sent["per_peer_counts"][b.keys.public_key().to_hex()], 1);
    let ids = relay.faults.ids.lock().unwrap();
    assert_eq!(ids.len(), 3);
    assert!(ids.iter().all(|id| id == &ids[0]));
    watcher.abort();
}
#[tokio::test]
async fn catchup_backlog_exceeds_builder_and_buzz_caps() {
    let relay = RelayFixture::new().await;
    let (_d, h) = home();
    let publisher = Client::new(Keys::generate());
    publisher
        .add_relay(relay.backend.url().await)
        .await
        .unwrap();
    publisher.connect().await;
    // 1105 > builder's default 500 AND Buzz SQL's effective 1000. Spread over
    // approved senders so the deliberate 100/day/peer quota does not mask paging.
    let senders: Vec<_> = (0..12)
        .map(|i| {
            let keys = Keys::generate();
            h.add_peer(&keys.public_key().to_hex(), &format!("peer-{i}"))
                .unwrap();
            keys
        })
        .collect();
    let now = Timestamp::now().as_secs();
    let publisher_relay = publisher.relay(relay.backend.url().await).await.unwrap();
    for i in 0..1105 {
        let event = manual_rumor(
            &senders[i % 12],
            h.keys.public_key(),
            false,
            i as u64 + 1,
            now - 2200 + i as u64,
        )
        .await;
        publisher_relay.send_event(&event).await.unwrap();
    }
    publisher.shutdown().await;
    let process = watch_process(&h, &relay.url, "3");
    wait_healthy(&h).await;
    assert_eq!(
        h.entries().unwrap().len(),
        1105,
        "all pages before checkpoint"
    );
    assert!(h.cursor().unwrap() >= now);
    drop(process);
    let process = watch_process(&h, &relay.url, "3");
    wait_healthy(&h).await;
    assert_eq!(h.entries().unwrap().len(), 1105);
    drop(process);
}
#[tokio::test]
async fn catchup_newer_event_does_not_skip_older_delayed_event() {
    let relay = RelayFixture::new().await;
    let (_d, h) = home();
    let sender = Keys::generate();
    h.add_peer(&sender.public_key().to_hex(), "friend").unwrap();
    let now = Timestamp::now().as_secs();
    std::fs::write(h.root.join("chat/checkpoint-version"), "2").unwrap();
    std::fs::write(h.root.join("chat/cursor"), (now - 1000).to_string()).unwrap();
    let newer = manual_rumor(&sender, h.keys.public_key(), false, 2, now).await;
    h.receive(&newer).await.unwrap(); // Simulate process dying before history EOSE.
    let older = manual_rumor(&sender, h.keys.public_key(), false, 1, now - 500).await;
    let publisher = Client::new(Keys::generate());
    publisher
        .add_relay(relay.backend.url().await)
        .await
        .unwrap();
    publisher.connect().await;
    publisher.send_event(&older).await.unwrap();
    publisher.shutdown().await;
    let process = watch_process(&h, &relay.url, "3");
    wait_healthy(&h).await;
    assert_eq!(h.entries().unwrap().len(), 2);
    drop(process);
}

#[tokio::test]
async fn retry_connect_auth_and_rate_limit_are_retryable() {
    for auth in [true, false] {
        let relay = RelayFixture::new().await;
        if auth {
            relay.faults.refuse_auth.store(1, Ordering::SeqCst);
        } else {
            *relay.faults.refusal.lock().unwrap() = "rate-limited: try again".into();
            relay.faults.reject_next.store(1, Ordering::SeqCst);
        }
        let (_a, a) = home();
        let (_b, b) = home();
        approve(&a, &b, "B");
        let outcome = a.send_outcome(&relay.url, "B", "retry").await.unwrap();
        assert_eq!(outcome.status, "accepted");
        assert_eq!(outcome.attempts, 2);
        assert_eq!(a.entries().unwrap().len(), 1);
    }
}
#[tokio::test]
async fn retry_lost_first_ok_duplicate_acceptance_resolves_ambiguity() {
    let relay = RelayFixture::new().await;
    relay.faults.omit_next_ok.store(1, Ordering::SeqCst);
    let (_a, a) = home();
    let (_b, b) = home();
    approve(&a, &b, "B");
    let outcome = a
        .send_outcome(&relay.url, "B", "duplicate accepted")
        .await
        .unwrap();
    assert_eq!(outcome.status, "accepted");
    assert_eq!(outcome.attempts, 2);
    assert_eq!(a.entries().unwrap().len(), 1);
    let ids = relay.faults.ids.lock().unwrap();
    assert_eq!(ids.len(), 2);
    assert_eq!(ids[0], ids[1]);
}
#[tokio::test]
async fn catchup_inclusive_boundary_second_is_not_skipped() {
    let relay = RelayFixture::new().await;
    let (_d, h) = home();
    let a = Keys::generate();
    let b = Keys::generate();
    h.add_peer(&a.public_key().to_hex(), "a").unwrap();
    h.add_peer(&b.public_key().to_hex(), "b").unwrap();
    let publisher = Client::new(Keys::generate());
    publisher
        .add_relay(relay.backend.url().await)
        .await
        .unwrap();
    publisher.connect().await;
    let r = publisher.relay(relay.backend.url().await).await.unwrap();
    let now = Timestamp::now().as_secs();
    for i in 0..150 {
        let sender = if i % 2 == 0 { &a } else { &b };
        r.send_event(&manual_rumor(sender, h.keys.public_key(), false, i + 1, now - 500).await)
            .await
            .unwrap();
    }
    publisher.shutdown().await;
    let process = watch_process(&h, &relay.url, "3");
    wait_healthy(&h).await;
    assert_eq!(h.entries().unwrap().len(), 150);
    drop(process);
}

#[test]
fn health_status_does_not_initialize_home_or_read_key() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing");
    assert!(!cli(&missing, &["status", "--json"]).status.success());
    assert!(!missing.exists());
    let (_d, h) = home();
    std::fs::remove_file(h.root.join("key")).unwrap();
    assert!(!cli(&h.root, &["status", "--json"]).status.success());
    assert!(
        !h.root.join("key").exists(),
        "status must not bootstrap a new identity"
    );
}

#[tokio::test]
async fn catchup_upgrade_rechecks_legacy_cursor_skipped_history() {
    let relay = RelayFixture::new().await;
    let (_d, h) = home();
    let sender = Keys::generate();
    h.add_peer(&sender.public_key().to_hex(), "friend").unwrap();
    let now = Timestamp::now().as_secs();
    std::fs::write(h.root.join("chat/cursor"), now.to_string()).unwrap();
    let publisher = Client::new(Keys::generate());
    publisher
        .add_relay(relay.backend.url().await)
        .await
        .unwrap();
    publisher.connect().await;
    let r = publisher.relay(relay.backend.url().await).await.unwrap();
    r.send_event(&manual_rumor(&sender, h.keys.public_key(), false, 1, now - 500).await)
        .await
        .unwrap();
    publisher.shutdown().await;
    let process = watch_process(&h, &relay.url, "3");
    wait_healthy(&h).await;
    assert_eq!(h.entries().unwrap().len(), 1);
    assert_eq!(
        std::fs::read_to_string(h.root.join("chat/checkpoint-version")).unwrap(),
        "2"
    );
    drop(process);
}

#[tokio::test]
async fn health_status_from_dead_previous_process_is_not_healthy() {
    let (_d, h) = home();
    // Lock held (new watcher just started), but the file is a fresh "healthy" from a dead pid.
    let _lock = h.watch_lock().unwrap();
    let mut dead = std::process::Command::new("true").spawn().unwrap();
    let dead_pid = dead.id();
    dead.wait().unwrap();
    let json = json!({"state":"healthy","pid":dead_pid,"relay":"ws://x","last_healthy_at":1,
        "consecutive_failures":0,"last_error":null,"updated_at":Timestamp::now().as_secs()});
    std::fs::write(h.root.join("chat/status.json"), json.to_string()).unwrap();
    let status = h.status().unwrap();
    assert_ne!(
        status.state, "healthy",
        "stale file from a dead watcher read as healthy"
    );
    let out = cli(&h.root, &["status"]);
    assert!(!out.status.success());
}
