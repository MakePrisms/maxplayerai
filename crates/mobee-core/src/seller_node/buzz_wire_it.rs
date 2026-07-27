//! LOCAL-RELAY teeth for the buzz persona's PRODUCTION WIRING — `SellerNodeRunner::boot`, the path
//! `mobee sell` actually runs. The persona's own behaviour (publish, clobber guard, heartbeat,
//! clean-shutdown clear) is fenced in [`super::buzz_relay_it`]; what is fenced HERE is that the
//! shipping daemon calls it at all, that a node without `[buzz]` stays silent on the buzz wire, and
//! that a buzz relay the node cannot reach never keeps it from booting.
//!
//! Both assertions ride the SAME in-process relay and the SAME observer, so the configured node is
//! the positive control for the unconfigured one: the silence asserted for a `[buzz]`-less node is
//! only evidence because the instrument demonstrably sees the configured node's traffic.

use super::run::{
    BUZZ_START_TIMEOUT, BuzzStartOutcome, SellerNodeRunner, ShutdownSignals, start_buzz_bounded,
};
use crate::home::{self, BuzzConfig};

use nostr_relay_builder::prelude::{
    LocalRelay, PolicyResult, QueryPolicy, RelayBuilder, WritePolicy,
};
use nostr_sdk::prelude::{
    BoxedFuture, Client, Event, Filter, Keys, Kind, PublicKey, RelayPoolNotification,
};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::SellerNode;
use super::buzz::PRESENCE_KIND;

static WIRE_SEQ: AtomicU64 = AtomicU64::new(0);

fn unique_root(label: &str) -> std::path::PathBuf {
    let n = WIRE_SEQ.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!(
        "mobee-buzz-wire-{label}-{}-{n}",
        std::process::id()
    ))
}

async fn start_relay() -> (LocalRelay, String) {
    let relay = LocalRelay::new(RelayBuilder::default());
    relay.run().await.expect("relay run");
    let url = relay.url().await.to_string();
    (relay, url)
}

async fn connect_client(relay_url: &str) -> Client {
    let client = Client::new(Keys::generate());
    client.add_relay(relay_url).await.expect("add relay");
    client.connect().await;
    client.wait_for_connection(Duration::from_secs(5)).await;
    client
}

/// A seller home pointed at `relay_url` for the marketplace leg. `buzz_relay` is the `[buzz]`
/// section's relay — `None` leaves the section absent, which is the inert contract under test.
fn seller_home(
    root: &std::path::Path,
    relay_url: &str,
    buzz_relay: Option<&str>,
) -> home::MobeeHome {
    let mut h = home::bootstrap(root).expect("bootstrap home");
    h.config.relay_url = relay_url.to_string();
    h.config.buzz = buzz_relay.map(|url| BuzzConfig {
        relay_url: url.to_string(),
        name: "Rocky".to_string(),
        about: None,
        rate_sats: Some(50),
        capabilities: vec!["code".to_string()],
        mint: None,
        // Fast beat so a heartbeat lands inside the test window.
        heartbeat_secs: 1,
    });
    h
}

/// Collect every kind-0 / presence event seen from each watched author, keyed by author.
fn spawn_buzz_traffic_collector(client: &Client) -> Arc<Mutex<Vec<(PublicKey, u16)>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let mut notif = client.notifications();
    tokio::spawn(async move {
        while let Ok(n) = notif.recv().await {
            if let RelayPoolNotification::Event { event, .. } = n {
                sink.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((event.pubkey, event.kind.as_u16()));
            }
        }
    });
    seen
}

async fn wait_until<F: FnMut() -> bool>(timeout: Duration, mut cond: F) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn count_from(seen: &Arc<Mutex<Vec<(PublicKey, u16)>>>, author: PublicKey, kind: u16) -> usize {
    seen.lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|(pubkey, seen_kind)| *pubkey == author && *seen_kind == kind)
        .count()
}

/// The wiring tooth (#199) AND the inert-compat tooth (invariant 1), as one differential: two nodes
/// boot against one relay, `[buzz]` present on one and absent on the other, and the observer watches
/// both keys. Configured ⇒ the persona reaches the relay from the production boot path; absent ⇒ not
/// one event on the buzz wire, measured by the instrument that just proved it can see them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn boot_publishes_the_persona_only_when_buzz_is_configured() {
    let (relay, relay_url) = start_relay().await;

    let wired_root = unique_root("wired");
    let inert_root = unique_root("inert");
    let wired_home = seller_home(&wired_root, &relay_url, Some(&relay_url));
    let inert_home = seller_home(&inert_root, &relay_url, None);
    let wired_pk = PublicKey::parse(&home::public_key_hex(&wired_home).expect("wired pubkey"))
        .expect("parse wired pubkey");
    let inert_pk = PublicKey::parse(&home::public_key_hex(&inert_home).expect("inert pubkey"))
        .expect("parse inert pubkey");

    // Watch BOTH keys from before either boots, so no early publish is missed.
    let observer = connect_client(&relay_url).await;
    observer
        .subscribe(
            Filter::new()
                .authors([wired_pk, inert_pk])
                .kinds([Kind::Metadata, Kind::Custom(PRESENCE_KIND)]),
            None,
        )
        .await
        .expect("subscribe buzz kinds");
    let seen = spawn_buzz_traffic_collector(&observer);

    // Both boots run concurrently — the NIP-42 wait is the same for each, so this costs one wait.
    let (wired, inert) = tokio::join!(
        SellerNodeRunner::boot(wired_home),
        SellerNodeRunner::boot(inert_home),
    );
    let _wired = wired.expect("boot with [buzz] configured");
    let _inert = inert.expect("boot with [buzz] absent");

    // Positive control: the configured node's persona reached the relay THROUGH THE BOOT PATH.
    let persona_landed = wait_until(Duration::from_secs(15), || {
        count_from(&seen, wired_pk, Kind::Metadata.as_u16()) > 0
            && count_from(&seen, wired_pk, PRESENCE_KIND) > 0
    })
    .await;
    let wired_kind0 = count_from(&seen, wired_pk, Kind::Metadata.as_u16());
    let wired_presence = count_from(&seen, wired_pk, PRESENCE_KIND);
    eprintln!("wired node: kind0={wired_kind0} presence={wired_presence}");
    assert!(
        persona_landed,
        "booting with [buzz] configured must publish the persona kind-0 and a presence beat \
         (kind0={wired_kind0}, presence={wired_presence}) — the production boot path is the thing \
         under test"
    );

    // The inert claim, asserted only now that the instrument has demonstrably seen buzz traffic.
    let inert_kind0 = count_from(&seen, inert_pk, Kind::Metadata.as_u16());
    let inert_presence = count_from(&seen, inert_pk, PRESENCE_KIND);
    eprintln!("inert node: kind0={inert_kind0} presence={inert_presence}");
    assert_eq!(
        (inert_kind0, inert_presence),
        (0, 0),
        "a node with no [buzz] section must put NOTHING on the buzz wire — got kind0={inert_kind0} \
         presence={inert_presence}"
    );

    observer.disconnect().await;
    relay.shutdown();
    let _ = std::fs::remove_dir_all(&wired_root);
    let _ = std::fs::remove_dir_all(&inert_root);
}

/// Invariant 2 (the #149 brick class): a buzz relay the node cannot reach degrades to no persona and
/// the node boots anyway. A propagated buzz error — or an unbounded bring-up — would take the seller
/// down with the buzz relay, which is exactly what may never happen: the persona is discovery
/// context, the marketplace leg is the product.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_buzz_relay_that_is_down_never_keeps_the_node_from_booting() {
    let (relay, relay_url) = start_relay().await;

    // A port that is bound and immediately released: nothing is listening, so every connect fails.
    let closed_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
        listener.local_addr().expect("addr").port()
    };
    let dead_buzz = format!("ws://127.0.0.1:{closed_port}");

    let root = unique_root("dead-buzz");
    let home = seller_home(&root, &relay_url, Some(&dead_buzz));

    let started = std::time::Instant::now();
    let booted = SellerNodeRunner::boot(home).await;
    let elapsed = started.elapsed();
    eprintln!(
        "boot with a dead buzz relay: ok={} in {elapsed:?}",
        booted.is_ok()
    );

    let runner = booted.expect("a dead buzz relay must NOT fail the boot");
    assert!(
        !runner.seller_pubkey().is_empty(),
        "the booted node must still be usable with no persona"
    );
    // The bring-up is bounded, so the seller's readiness to claim cannot be held hostage by a sick
    // buzz relay. CONNECT_WAIT for the marketplace leg is already spent by this point; the margin
    // covers it.
    assert!(
        elapsed < BUZZ_START_TIMEOUT + Duration::from_secs(30),
        "the buzz bring-up must be bounded — boot took {elapsed:?}"
    );

    relay.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

/// A relay that completes the websocket handshake and then answers NOTHING — the REQ never gets its
/// EOSE, the EVENT never gets its OK. This is the dishonest failure: the socket looks alive.
#[derive(Debug)]
struct NeverAnswers;

impl QueryPolicy for NeverAnswers {
    fn admit_query<'a>(
        &'a self,
        _query: &'a Filter,
        _addr: &'a SocketAddr,
    ) -> BoxedFuture<'a, PolicyResult> {
        Box::pin(std::future::pending())
    }
}

impl WritePolicy for NeverAnswers {
    fn admit_event<'a>(
        &'a self,
        _event: &'a Event,
        _addr: &'a SocketAddr,
    ) -> BoxedFuture<'a, PolicyResult> {
        Box::pin(std::future::pending())
    }
}

/// A relay that hangs mid-conversation degrades — AND the arm that saves the node is the bring-up's
/// INNER bounds, not the outer backstop. That second half is a tripwire on `BUZZ_START_TIMEOUT`'s
/// headroom, because the bound is only a backstop while this arithmetic holds:
///
/// ```text
///   kind-0 fetch      8s   buzz::KIND0_FETCH_TIMEOUT_SECS
///   OK-wait          10s   nostr-relay-pool relay::constants::WAIT_FOR_OK_TIMEOUT
///   ────────────────────
///   worst reproducible 18s   <   25s   BUZZ_START_TIMEOUT
/// ```
///
/// The assertion is on the RETURNED VARIANT, never on elapsed time — the call has returned, so there
/// is no window here to be wrong about and nothing that rots when a machine or a suite gets slower.
/// If a dependency bump ever pushes that sum past the bound, this flips to `TimedOut` and goes red:
/// the day the backstop starts bearing load is the day someone needs to know.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_relay_that_hangs_is_caught_by_the_inner_bounds_not_the_backstop() {
    let relay = LocalRelay::new(
        RelayBuilder::default()
            .query_policy(NeverAnswers)
            .write_policy(NeverAnswers),
    );
    relay.run().await.expect("relay run");
    let hung_url = relay.url().await.to_string();

    // Marketplace leg unused here — this drives the bring-up directly, so no boot/NIP-42 wait.
    let root = unique_root("hung-buzz");
    let home = seller_home(&root, &hung_url, Some(&hung_url));
    let node = SellerNode::open(home).await.expect("open node");

    let started = std::time::Instant::now();
    let outcome = start_buzz_bounded(&node).await;
    let elapsed = started.elapsed();

    let arm = match &outcome {
        BuzzStartOutcome::Live(_) => "Live",
        BuzzStartOutcome::Inert => "Inert",
        BuzzStartOutcome::Failed(error) => {
            eprintln!("inner-bound failure: {error}");
            "Failed"
        }
        BuzzStartOutcome::TimedOut => "TimedOut",
    };
    eprintln!("hung relay: arm={arm} elapsed={elapsed:?} (bound {BUZZ_START_TIMEOUT:?})");

    assert_eq!(
        arm, "Failed",
        "a hung relay must be refused by the bring-up's own bounds, leaving BUZZ_START_TIMEOUT a \
         backstop. arm={arm} means the arithmetic in this test's docs no longer holds — if it is \
         TimedOut, the inner bounds now sum past {BUZZ_START_TIMEOUT:?} and the backstop is bearing \
         load"
    );

    relay.shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

/// The other half of invariant 1: a signal receiver REPLACES the process default for that signal, so
/// the run loop installs one only for a daemon carrying a live persona. A node with no persona
/// installs nothing and keeps dying on SIGTERM exactly as it does today.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_signals_are_installed_only_for_a_live_persona() {
    assert!(
        ShutdownSignals::install(false).is_none(),
        "a buzz-inert daemon must install NO signal handler — installing one silently changes how \
         every seller daemon dies"
    );
    #[cfg(unix)]
    assert!(
        ShutdownSignals::install(true).is_some(),
        "a daemon with a live persona must install the handlers, or the clean-shutdown clear can \
         never fire"
    );
}
