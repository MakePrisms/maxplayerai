use futures_util::{SinkExt, StreamExt};
use maxplayer_chat::{Home, envelope, watch};
use maxplayer_core::private_content::transport;
use nostr_relay_builder::{LocalRelay, RelayBuilder};
use nostr_sdk::prelude::*;
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{net::TcpListener, task::JoinHandle};

// A real nostr-relay-builder stores and streams events. This small front adds Buzz's
// challenge-on-connect / restricted-before-auth rule (builder otherwise challenges lazily).
struct RelayFixture {
    backend: LocalRelay,
    url: String,
    task: JoinHandle<()>,
    stop: tokio::sync::watch::Sender<bool>,
    reqs: Arc<Mutex<Vec<bool>>>,
}
impl RelayFixture {
    async fn new() -> Self {
        let backend = LocalRelay::new(RelayBuilder::default().auth_dm(false));
        backend.run().await.unwrap();
        Self::start(backend, 0, Arc::default()).await
    }
    async fn start(backend: LocalRelay, port: u16, reqs: Arc<Mutex<Vec<bool>>>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let backend_url = backend.url().await.to_string();
        let (stop, rx) = tokio::sync::watch::channel(false);
        let recorded = reqs.clone();
        let public_url = url.clone();
        let task = tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let backend_url = backend_url.clone();
                let reqs = recorded.clone();
                let mut rx = rx.clone();
                let url = public_url.clone();
                tokio::spawn(async move {
                    let mut front = tokio_tungstenite::accept_async(socket).await.unwrap();
                    let (mut back, _) =
                        tokio_tungstenite::connect_async(backend_url).await.unwrap();
                    let challenge = "chat-fixture-challenge";
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
                                    authenticated=event.verify().is_ok() && event.kind==Kind::Authentication && event.tags.iter().any(|t|t.as_slice()==["challenge",challenge]) && event.tags.iter().any(|t|t.as_slice()==["relay",url.as_str()]);
                                    front.send(json!(["OK",event.id.to_hex(),authenticated,""]).to_string().into()).await.unwrap();
                                }else{if v[0]=="REQ"{reqs.lock().unwrap().push(authenticated);if !authenticated{front.send(json!(["CLOSED",v[1],"restricted: auth first"]).to_string().into()).await.unwrap();continue;}}
                                    if back.send(frame).await.is_err(){break}
                                }
                            },
                            frame=back.next()=>{let Some(Ok(frame))=frame else{break};if front.send(frame).await.is_err(){break}},
                        }
                    }
                });
            }
        });
        Self {
            backend,
            url,
            task,
            stop,
            reqs,
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
    let root = dir.path().join("fresh");
    let out = cli(&root, &["whoami"]);
    assert!(out.status.success());
    let key = std::fs::read_to_string(root.join("key")).unwrap();
    assert_eq!(key.len(), 64);
    assert!(
        key.bytes()
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
        key
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
    assert!(h.send("ws://127.0.0.1:1", "p", &long).await.is_err());
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
    let secret = std::fs::read_to_string(h.root.join("key")).unwrap();
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
    std::fs::write(&script,"import os,sys,time,json\nwith open(sys.argv[1], 'a') as f: f.write(json.dumps({'argv':sys.argv,'env':dict(os.environ)})+'\\n'); f.flush()\nwhile not os.path.exists(sys.argv[2]): time.sleep(0.02)\n").unwrap();
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
        assert_eq!(v["env"]["MAXPLAYER_CHAT_PEER"], "A");
    }
    wb.abort();
}
