//! The real sidecar over an in-process relay (spec §5 "Required tests", mint side).
//!
//! Wallet tests drive it with the real cdk wallet and the stage-1 `NostrMintConnector`. Raw tests
//! publish hand-built request events, so they can re-send the exact same event (the connector's
//! lost-reply path), reuse an id, or send what a wallet never would.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cdk::Amount;
use cdk::Mint;
use cdk::amount::{FeeAndAmounts, SplitTarget};
use cdk::dhke::construct_proofs;
use cdk::mint_url::MintUrl;
use cdk::nuts::{CurrencyUnit, Id, PreMintSecrets, Proofs, SwapRequest, Token};
use cdk::util::unix_time;
use cdk::wallet::{ReceiveOptions, SendOptions, Wallet};
use maxplayer_core::mint_wire::{
    ErrorBody, ErrorCode, Outcome, REQUEST_KIND, RESPONSE_KIND, Request, Response, code, op,
};
use maxplayer_core::nostr_mint::build_wallet_with_relays;
use maxplayer_mint::home::{MintHome, mint_url};
use maxplayer_mint::replay::{self, Record};
use maxplayer_mint::{backend, issue, server};
use nostr_relay_builder::prelude::{LocalRelay, RelayBuilder};
use nostr_sdk::nips::nip44::{self, Version};
use nostr_sdk::prelude::{
    Client, Event, EventBuilder, Filter, Keys, Kind, PublicKey, RelayPoolNotification, Tag,
    Timestamp,
};
use serde_json::{Value, json};
use tokio::task::JoinHandle;

const WAIT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

struct Harness {
    _relays: Vec<LocalRelay>,
    relay_urls: Vec<String>,
    mint: Mint,
    keys: Keys,
    url: String,
    seed: [u8; 64],
    db: PathBuf,
    rate_limit: u32,
    task: JoinHandle<()>,
    _dir: tempfile::TempDir,
}

fn seed() -> [u8; 64] {
    let mut seed = [0u8; 64];
    getrandom::fill(&mut seed).unwrap();
    seed
}

async fn start_server(
    mint: Mint,
    keys: Keys,
    relays: &[String],
    rate_limit: u32,
) -> JoinHandle<()> {
    let server = server::Server::connect(mint, keys, relays, rate_limit)
        .await
        .expect("server connect");
    tokio::spawn(async move {
        let _ = server.serve().await;
    })
}

async fn harness(relays: usize, rate_limit: u32) -> Harness {
    let mut local = Vec::new();
    let mut relay_urls = Vec::new();
    for _ in 0..relays {
        let relay = LocalRelay::new(RelayBuilder::default());
        relay.run().await.expect("relay run");
        relay_urls.push(relay.url().await.to_string());
        local.push(relay);
    }
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("mint.sqlite");
    let keys = Keys::generate();
    let url = mint_url(&keys).unwrap();
    let seed = seed();
    let mint = backend::open(&db, &seed, &url).await.expect("open mint");
    let task = start_server(mint.clone(), keys.clone(), &relay_urls, rate_limit).await;
    Harness {
        _relays: local,
        relay_urls,
        mint,
        keys,
        url,
        seed,
        db,
        rate_limit,
        task,
        _dir: dir,
    }
}

impl Harness {
    /// Kill the listener and the mint, then start both again from the same `mint.sqlite`.
    async fn restart(&mut self) {
        self.task.abort();
        let _ = (&mut self.task).await;
        self.mint.stop().await.unwrap();
        self.mint = backend::open(&self.db, &self.seed, &self.url)
            .await
            .expect("reopen mint");
        self.task = start_server(
            self.mint.clone(),
            self.keys.clone(),
            &self.relay_urls,
            self.rate_limit,
        )
        .await;
    }

    async fn wallet(&self) -> Wallet {
        let db = cdk_sqlite::wallet::memory::empty().await.unwrap();
        build_wallet_with_relays(
            &self.url,
            CurrencyUnit::Sat,
            Arc::new(db),
            seed(),
            None,
            self.relay_urls.clone(),
        )
        .unwrap()
    }

    async fn raw(&self) -> Raw {
        Raw::new(&self.relay_urls, self.keys.public_key()).await
    }
}

fn active_keyset(mint: &Mint) -> Id {
    backend::active_keyset(mint).unwrap()
}

fn premint(id: Id, amount: u64) -> PreMintSecrets {
    let amounts: Vec<u64> = (0..32).map(|i| 1u64 << i).collect();
    PreMintSecrets::random(
        id,
        Amount::from(amount),
        &SplitTarget::default(),
        &FeeAndAmounts::from((0, amounts)),
    )
    .unwrap()
}

/// Proofs signed directly by the mint (test funding; `issue` is its own test).
async fn fund(mint: &Mint, amount: u64) -> Proofs {
    let id = active_keyset(mint);
    let keys = mint.keyset(&id).unwrap().keys;
    let pre = premint(id, amount);
    let signatures = mint.blind_sign(pre.blinded_messages()).await.unwrap();
    construct_proofs(signatures, pre.rs(), pre.secrets(), &keys).unwrap()
}

async fn swap_request(mint: &Mint, amount: u64) -> SwapRequest {
    let inputs = fund(mint, amount).await;
    SwapRequest::new(
        inputs,
        premint(active_keyset(mint), amount).blinded_messages(),
    )
}

// ---------------------------------------------------------------------------------------------
// Raw client
// ---------------------------------------------------------------------------------------------

struct Raw {
    keys: Keys,
    mint_pk: PublicKey,
    client: Client,
    notifications: tokio::sync::broadcast::Receiver<RelayPoolNotification>,
}

impl Raw {
    async fn new(relays: &[String], mint_pk: PublicKey) -> Self {
        let keys = Keys::generate();
        let client = Client::new(keys.clone());
        for relay in relays {
            client.add_relay(relay.as_str()).await.unwrap();
        }
        client.connect().await;
        client.wait_for_connection(WAIT).await;
        let notifications = client.notifications();
        client
            .subscribe(
                Filter::new()
                    .kind(Kind::Custom(RESPONSE_KIND))
                    .pubkey(keys.public_key())
                    .since(Timestamp::now() - Duration::from_secs(10)),
                None,
            )
            .await
            .unwrap();
        Self {
            keys,
            mint_pk,
            client,
            notifications,
        }
    }

    fn sealed(&self, plain: &str, to: &PublicKey, p_tag: &PublicKey) -> Event {
        let content = nip44::encrypt(self.keys.secret_key(), to, plain, Version::V2).unwrap();
        EventBuilder::new(Kind::Custom(REQUEST_KIND), content)
            .tag(Tag::public_key(*p_tag))
            .sign_with_keys(&self.keys)
            .unwrap()
    }

    fn event(&self, plain: &str) -> Event {
        self.sealed(plain, &self.mint_pk, &self.mint_pk)
    }

    fn request(&self, id: &str, operation: &str, body: Value, exp: u64) -> (Request, Event) {
        let request = Request {
            v: 1,
            id: id.to_owned(),
            op: operation.to_owned(),
            body,
            exp,
        };
        let event = self.event(&serde_json::to_string(&request).unwrap());
        (request, event)
    }

    async fn send(&self, event: &Event) {
        let _ = self.client.send_event(event).await;
    }

    /// Up to `want` replies to `event`, waiting at most `wait`.
    async fn replies(&mut self, event: &Event, want: usize, wait: Duration) -> Vec<Response> {
        let deadline = Instant::now() + wait;
        let mut out = Vec::new();
        while out.len() < want {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match tokio::time::timeout(left, self.notifications.recv()).await {
                Err(_) => break,
                Ok(Ok(RelayPoolNotification::Event { event: reply, .. })) => {
                    if reply.kind != Kind::Custom(RESPONSE_KIND)
                        || reply.pubkey != self.mint_pk
                        || !reply.tags.event_ids().any(|id| *id == event.id)
                    {
                        continue;
                    }
                    let plain =
                        nip44::decrypt(self.keys.secret_key(), &self.mint_pk, &reply.content)
                            .unwrap();
                    out.push(serde_json::from_str(&plain).unwrap());
                }
                Ok(_) => continue,
            }
        }
        out
    }

    async fn call(&mut self, event: &Event) -> Response {
        self.send(event).await;
        let mut replies = self.replies(event, 1, WAIT).await;
        assert_eq!(replies.len(), 1, "expected one reply");
        replies.remove(0)
    }
}

fn ok(response: &Response) -> &Value {
    match &response.outcome {
        Outcome::Ok(value) => value,
        Outcome::Err(error) => panic!("expected ok, got {error:?}"),
    }
}

fn named(response: &Response) -> String {
    match &response.outcome {
        Outcome::Err(ErrorBody {
            code: ErrorCode::Named(name),
            ..
        }) => name.clone(),
        other => panic!("expected a named error, got {other:?}"),
    }
}

fn exp() -> u64 {
    unix_time() + 60
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

/// Real cdk wallets over the stage-1 connector: receive, send, and two holders spending the
/// same proofs (one wins).
#[tokio::test(flavor = "multi_thread")]
async fn wallets_receive_send_and_double_spend_one_wins() {
    let h = harness(1, 20).await;
    let proofs = fund(&h.mint, 100).await;
    let token = Token::new(
        MintUrl::from_str(&h.url).unwrap(),
        proofs,
        None,
        CurrencyUnit::Sat,
    )
    .to_string();

    let a = h.wallet().await;
    assert_eq!(
        a.receive(&token, ReceiveOptions::default()).await.unwrap(),
        Amount::from(100)
    );
    let second = h.wallet().await;
    assert!(
        second
            .receive(&token, ReceiveOptions::default())
            .await
            .is_err(),
        "the same proofs must not be spent twice"
    );
    assert_eq!(second.total_balance().await.unwrap(), Amount::ZERO);

    let sent = a
        .prepare_send(Amount::from(30), SendOptions::default())
        .await
        .unwrap()
        .confirm(None)
        .await
        .unwrap();
    let b = h.wallet().await;
    assert_eq!(
        b.receive(&sent.to_string(), ReceiveOptions::default())
            .await
            .unwrap(),
        Amount::from(30)
    );
    assert_eq!(a.total_balance().await.unwrap(), Amount::from(70));
}

/// One request delivered by two relays, then re-sent: every reply is the first execution's.
#[tokio::test(flavor = "multi_thread")]
async fn duplicates_across_relays_execute_once() {
    let h = harness(2, 20).await;
    let mut raw = h.raw().await;
    let swap = swap_request(&h.mint, 64).await;
    let (_, event) = raw.request("dup-1", op::SWAP, json!(swap), exp());

    raw.send(&event).await;
    let first = raw.replies(&event, 2, WAIT).await;
    assert_eq!(first.len(), 2, "one reply per delivering relay");
    raw.send(&event).await;
    let again = raw.replies(&event, 2, WAIT).await;
    assert_eq!(again.len(), 2);
    for reply in first.iter().chain(&again) {
        assert_eq!(ok(reply), ok(&first[0]), "a duplicate re-executed");
    }
}

/// The wallet never sees the first reply and re-sends the same event after `exp`: it gets the
/// original reply, not `expired`.
#[tokio::test(flavor = "multi_thread")]
async fn lost_reply_is_replayed_even_after_exp() {
    let h = harness(1, 20).await;
    let mut raw = h.raw().await;
    let swap = swap_request(&h.mint, 21).await;
    let (_, event) = raw.request("lost-1", op::SWAP, json!(swap), unix_time() + 3);
    let first = raw.call(&event).await; // "lost"
    ok(&first);
    tokio::time::sleep(Duration::from_secs(4)).await;
    let replay = raw.call(&event).await;
    assert_eq!(replay, first);
}

/// A request first seen after `exp` is refused and not executed.
#[tokio::test(flavor = "multi_thread")]
async fn first_seen_after_exp_is_refused_without_executing() {
    let h = harness(1, 20).await;
    let mut raw = h.raw().await;
    let swap = swap_request(&h.mint, 8).await;
    let (_, stale) = raw.request("stale-1", op::SWAP, json!(swap), unix_time() - 1);
    assert_eq!(named(&raw.call(&stale).await), code::EXPIRED);
    // Not executed: the same swap under a fresh id still succeeds.
    let (_, fresh) = raw.request("stale-2", op::SWAP, json!(swap), exp());
    ok(&raw.call(&fresh).await);
}

/// Same (client, id) with different content never executes.
#[tokio::test(flavor = "multi_thread")]
async fn reused_id_with_different_content_is_refused() {
    let h = harness(1, 20).await;
    let mut raw = h.raw().await;
    let swap = swap_request(&h.mint, 5).await;
    let (_, first) = raw.request("reuse-1", op::SWAP, json!(swap), exp());
    ok(&raw.call(&first).await);
    let other = swap_request(&h.mint, 5).await;
    let (_, second) = raw.request("reuse-1", op::SWAP, json!(other), exp());
    assert_eq!(named(&raw.call(&second).await), code::INTERNAL);
    // ...and `other` was not executed.
    let (_, third) = raw.request("reuse-2", op::SWAP, json!(other), exp());
    ok(&raw.call(&third).await);
}

/// Mint killed after cdk committed the swap but before the reply was recorded: the re-send is
/// answered from committed state with the very same signatures.
#[tokio::test(flavor = "multi_thread")]
async fn killed_after_commit_is_answered_from_committed_state() {
    let h = harness(1, 20).await;
    let mut raw = h.raw().await;
    let swap = swap_request(&h.mint, 13).await;
    let (request, event) = raw.request("crash-1", op::SWAP, json!(swap), exp());
    let key = replay::key(&raw.keys.public_key(), &request.id);
    let executing = Record::Executing {
        event_id: event.id.to_hex(),
        digest: replay::digest(&request),
    };
    replay::write(&h.mint, &key, &executing).await.unwrap();
    let committed = h.mint.process_swap_request(swap).await.unwrap();

    let reply = raw.call(&event).await;
    assert_eq!(ok(&reply), &json!(committed));
    assert_eq!(raw.call(&event).await, reply, "now recorded");
}

/// Mint killed after recording `executing` but before cdk committed: the re-send executes it.
#[tokio::test(flavor = "multi_thread")]
async fn killed_before_commit_executes_on_resend() {
    let h = harness(1, 20).await;
    let mut raw = h.raw().await;
    let swap = swap_request(&h.mint, 9).await;
    let (request, event) = raw.request("crash-2", op::SWAP, json!(swap), exp());
    let key = replay::key(&raw.keys.public_key(), &request.id);
    let executing = Record::Executing {
        event_id: event.id.to_hex(),
        digest: replay::digest(&request),
    };
    replay::write(&h.mint, &key, &executing).await.unwrap();

    let reply = raw.call(&event).await;
    let signatures = ok(&reply)["signatures"].as_array().unwrap().len();
    assert_eq!(signatures, swap.outputs().len());
    assert_eq!(raw.call(&event).await, reply);
}

/// The record survives a restart of the process.
#[tokio::test(flavor = "multi_thread")]
async fn replay_survives_restart() {
    let mut h = harness(1, 20).await;
    let mut raw = h.raw().await;
    let swap = swap_request(&h.mint, 34).await;
    let (_, event) = raw.request("restart-1", op::SWAP, json!(swap), exp());
    let first = raw.call(&event).await;
    ok(&first);
    h.restart().await;
    assert_eq!(raw.call(&event).await, first);
}

/// A replay is served before the rate limiter, so it never becomes `rate_limited`.
#[tokio::test(flavor = "multi_thread")]
async fn rate_limit_refuses_new_requests_but_not_replays() {
    let h = harness(1, 1).await;
    let mut raw = h.raw().await;
    let swap = swap_request(&h.mint, 3).await;
    let (_, event) = raw.request("rl-1", op::SWAP, json!(swap), exp());
    let first = raw.call(&event).await;
    ok(&first);
    let mut limited = 0;
    for n in 0..3 {
        let (_, new) = raw.request(&format!("rl-k{n}"), op::KEYSETS, Value::Null, exp());
        let reply = raw.call(&new).await;
        if matches!(&reply.outcome, Outcome::Err(_)) {
            assert_eq!(named(&reply), code::RATE_LIMITED);
            limited += 1;
        }
    }
    assert!(
        limited > 0,
        "a 1/s limit admitted 4 requests in well under a second"
    );
    assert_eq!(
        raw.call(&event).await,
        first,
        "replay must not be rate limited"
    );
}

/// Minting and melting are refused, and info doesn't advertise them.
#[tokio::test(flavor = "multi_thread")]
async fn mint_and_melt_refused_on_local_issue() {
    let h = harness(1, 20).await;
    let mut raw = h.raw().await;
    for operation in [op::MINT_QUOTE, op::MINT, op::MELT_QUOTE, op::MELT] {
        let (_, event) = raw.request(&format!("u-{operation}"), operation, json!({}), exp());
        assert_eq!(
            named(&raw.call(&event).await),
            code::UNSUPPORTED,
            "{operation}"
        );
    }
    let (_, info) = raw.request("info-1", op::INFO, Value::Null, exp());
    let info = raw.call(&info).await;
    for nut in ["4", "5"] {
        let methods = &ok(&info)["nuts"][nut]["methods"];
        assert!(
            methods.as_array().is_none_or(|m| m.is_empty()),
            "NUT-{nut} advertises {methods}"
        );
    }
}

/// Malformed and wrong-version requests get `bad_request`; forged, wrong-key and untagged
/// events get no reply at all.
#[tokio::test(flavor = "multi_thread")]
async fn malformed_forged_and_wrong_key_requests() {
    let h = harness(1, 20).await;
    let mut raw = h.raw().await;

    let malformed = raw.event(r#"{"v":1,"id":"bad-1"}"#);
    assert_eq!(named(&raw.call(&malformed).await), code::BAD_REQUEST);
    let (_, bad_body) = raw.request("bad-2", op::SWAP, json!({"inputs": 7}), exp());
    assert_eq!(named(&raw.call(&bad_body).await), code::BAD_REQUEST);
    let wrong_v = raw.event(
        &json!({"v": 9, "id": "bad-3", "op": op::KEYSETS, "body": null, "exp": exp()}).to_string(),
    );
    assert_eq!(named(&raw.call(&wrong_v).await), code::BAD_REQUEST);

    let plain =
        json!({"v": 1, "id": "silent", "op": op::KEYSETS, "body": null, "exp": exp()}).to_string();
    let stranger = Keys::generate().public_key();
    let wrong_key = raw.sealed(&plain, &stranger, &h.keys.public_key());
    let untagged = raw.sealed(&plain, &h.keys.public_key(), &stranger);
    let mut forged = raw.event(&plain);
    forged.content = raw.event(&plain).content; // valid ciphertext, signature no longer matches
    for event in [&wrong_key, &untagged, &forged] {
        raw.send(event).await;
        assert!(
            raw.replies(event, 1, Duration::from_secs(2))
                .await
                .is_empty(),
            "unanswerable event was answered"
        );
    }
    // The listener is still healthy.
    let (_, info) = raw.request("after", op::KEYSETS, Value::Null, exp());
    ok(&raw.call(&info).await);
}

#[test]
fn init_refuses_over_an_existing_mint() {
    let dir = tempfile::tempdir().unwrap();
    let home = MintHome::at(dir.path());
    home.init().unwrap();
    let loaded = home.load().unwrap();
    assert!(home.init().is_err(), "second init must refuse");
    assert_eq!(
        home.load().unwrap().keys.public_key(),
        loaded.keys.public_key(),
        "the refused init must not touch the existing mint"
    );
    // Also refuses over a stray file or symlink named `mint`.
    let other = tempfile::tempdir().unwrap();
    std::fs::write(other.path().join("mint"), b"").unwrap();
    assert!(MintHome::at(other.path()).init().is_err());
    let _: &Path = home.dir();
}

fn total_issued(mint: &Mint) -> impl std::future::Future<Output = u64> + '_ {
    async move {
        mint.total_issued()
            .await
            .unwrap()
            .values()
            .map(|a| a.clone().to_u64())
            .sum()
    }
}

async fn receive_file(h: &Harness, file: &Path) -> Amount {
    let token = std::fs::read_to_string(file).unwrap();
    h.wallet()
        .await
        .receive(token.trim(), ReceiveOptions::default())
        .await
        .unwrap()
}

/// `issue` writes a 0600 token a real wallet receives over the relay, and cdk records it.
#[tokio::test(flavor = "multi_thread")]
async fn issue_writes_a_token_a_wallet_receives() {
    use std::os::unix::fs::PermissionsExt;
    let h = harness(1, 20).await;
    let dir = tempfile::tempdir().unwrap();
    assert!(issue::issue(&h.mint, dir.path(), &h.url, 0).await.is_err());
    let done = issue::issue(&h.mint, dir.path(), &h.url, 100)
        .await
        .unwrap();
    assert_eq!(done.amount, 100);
    let mode = std::fs::metadata(&done.file).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    assert_eq!(total_issued(&h.mint).await, 100);
    assert_eq!(receive_file(&h, &done.file).await, Amount::from(100));
    assert!(
        issue::reconcile(&h.mint, dir.path(), &h.url)
            .await
            .unwrap()
            .is_empty()
    );
}

/// Interrupted before anything was signed: the next run signs the SAME journaled outputs once.
#[tokio::test(flavor = "multi_thread")]
async fn issue_interrupted_before_signing_is_finished_once() {
    let h = harness(1, 20).await;
    let dir = tempfile::tempdir().unwrap();
    let id = issue::begin(&h.mint, 77).await.unwrap();
    assert_eq!(total_issued(&h.mint).await, 0);
    let done = issue::reconcile(&h.mint, dir.path(), &h.url).await.unwrap();
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].id, id);
    assert!(
        issue::reconcile(&h.mint, dir.path(), &h.url)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(total_issued(&h.mint).await, 77, "re-issued");
    assert_eq!(receive_file(&h, &done[0].file).await, Amount::from(77));
}

/// Interrupted after signing, before the token file: the file is rebuilt from the journal and
/// nothing is signed again.
#[tokio::test(flavor = "multi_thread")]
async fn issue_interrupted_after_signing_rewrites_the_file() {
    let h = harness(1, 20).await;
    let dir = tempfile::tempdir().unwrap();
    let id = issue::begin(&h.mint, 40).await.unwrap();
    issue::sign(&h.mint, &id).await.unwrap();
    assert!(matches!(
        issue::read(&h.mint, &id).await.unwrap(),
        Some(issue::Entry::Committed { .. })
    ));
    let done = issue::reconcile(&h.mint, dir.path(), &h.url).await.unwrap();
    assert_eq!(done.len(), 1);
    assert_eq!(total_issued(&h.mint).await, 40);
    assert_eq!(receive_file(&h, &done[0].file).await, Amount::from(40));
    assert!(matches!(
        issue::read(&h.mint, &id).await.unwrap(),
        Some(issue::Entry::Written { amount: 40, .. })
    ));
}
