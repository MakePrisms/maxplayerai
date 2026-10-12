//! Single-owner, durable coordinator. The CLI holds owner.lock for every entry point.
use crate::{
    journal::Journal,
    market::{Envelope, Market},
    mint::Plan,
    *,
};
use anyhow::{Context, Result, ensure};
use cashu::nuts::{Id, Proofs};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
pub fn now() -> u64 {
    cdk::util::unix_time()
}
fn hash<T: Serialize>(v: &T) -> Result<String> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(v)?)))
}
fn secret() -> String {
    hex::encode(cashu::nuts::SecretKey::generate().to_secret_bytes())
}
fn pubkey(s: &str) -> Result<String> {
    Ok(s.parse::<cashu::nuts::SecretKey>()?
        .public_key()
        .to_string())
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Terms {
    pub keyset: Id,
    pub ppk: u64,
    pub gross: u64,
    pub lock_fee: u64,
    pub claim_fee: u64,
    pub debit: u64,
}
impl From<&Plan> for Terms {
    fn from(p: &Plan) -> Self {
        Self {
            keyset: p.keyset,
            ppk: p.ppk,
            gross: p.gross,
            lock_fee: p.lock_fee,
            claim_fee: p.claim_fee,
            debit: p.debit,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub hash: String,
    pub taker_key: String,
    pub funding: Terms,
    pub max_give: u64,
    pub min_receive: u64,
    pub max_fees: u64,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Quote {
    pub lot: Lot,
    pub lot_id: String,
    pub swap_id: String,
    pub maker: String,
    pub taker: String,
    pub maker_key: String,
    pub request: Request,
    pub give: Terms,
    pub issued: u64,
    pub exp: u64,
    pub long: u64,
    pub short: u64,
    pub cutoff: u64,
    pub margin: u64,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Listing {
    pub published: usize,
    pub max_fees: u64,
    pub event: Event,
    pub statuses: Vec<Event>,
    pub plan: Plan,
    pub reservation: String,
    pub active: Option<String>,
    pub cancelled: bool,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Swap {
    pub created: u64,
    pub id: String,
    pub lot: Event,
    pub role: String,
    pub peer: String,
    pub key: String,
    pub preimage: Option<String>,
    pub request: Request,
    pub quote: Option<Quote>,
    pub plan: Plan,
    pub state: String,
    pub outgoing: Proofs,
    pub incoming: Proofs,
    pub max_fees: u64,
    #[serde(default)]
    pub refund_generation: u32,
}
pub(crate) fn swap_terminal(s: &Swap) -> bool {
    terminal(s)
}
pub fn terminal(s: &Swap) -> bool {
    [
        "complete",
        "complete_unclaimed",
        "refunded",
        "expired",
        "refund_quarantined",
        "claim_quarantined",
    ]
    .contains(&s.state.as_str())
}
#[cfg(feature = "lab")]
static LAB_DEADLINES: std::sync::Mutex<Option<(u64, u64, u64, u64)>> = std::sync::Mutex::new(None);
/// Lab-only: (long, short, cutoff, margin) lock timing for loopback mints, overriding
/// `TRADE_LAB_SECONDS` (tests that need room for several silent `nostr://` calls before the
/// cutoff). `None` restores the default.
#[cfg(feature = "lab")]
pub fn lab_set_deadlines(t: Option<(u64, u64, u64, u64)>) {
    *LAB_DEADLINES.lock().unwrap_or_else(|e| e.into_inner()) = t;
}
fn timing(l: &Lot) -> Result<(u64, u64, u64, u64)> {
    #[cfg(feature = "lab")]
    if let Some(t) = *LAB_DEADLINES.lock().unwrap_or_else(|e| e.into_inner()) {
        for a in [&l.give.asset, &l.want.asset] {
            ensure!(
                crate::transport::loopback_only(&a.mint_url),
                "lab timing requires loopback mints (and loopback mint relays for nostr://)"
            );
        }
        return Ok(t);
    }
    #[cfg(feature = "lab")]
    if std::env::var("TRADE_LAB_SECONDS").is_ok() {
        for a in [&l.give.asset, &l.want.asset] {
            ensure!(
                crate::transport::loopback_only(&a.mint_url),
                "lab timing requires loopback mints (and loopback mint relays for nostr://)"
            );
        }
        // Debug SQLite/crypto plus the fresh /info RPC took ~2.8s for one
        // lock step on the shared host. Eight seconds left only six usable
        // seconds for the whole exchange. Scale ONLY loopback lab locks;
        // retain the 3:1 ratio, cutoff, margin and all production gates.
        return Ok((48, 16, 2, 1));
    }
    let _ = l;
    Ok((LONG_SECONDS, SHORT_SECONDS, CLAIM_CUTOFF_SECONDS, 60))
}
async fn preflight(l: &Lot) -> Result<()> {
    for a in [&l.give.asset, &l.want.asset] { a.fence()?; }
    let (give, want) = tokio::join!(wallet::preflight_admission(&l.give.asset.mint_url), wallet::preflight_admission(&l.want.asset.mint_url));
    give?; want?;
    Ok(())
}
async fn recovery_preflight(l: &Lot) -> Result<()> {
    for a in [&l.give.asset, &l.want.asset] { wallet::preflight(&a.mint_url).await?; }
    Ok(())
}
async fn save(j: &Journal, s: &Swap) -> Result<()> {
    j.put("swap", &s.id, s).await
}
fn envelope(s: &Swap, step: &str, body: serde_json::Value) -> Result<Envelope> {
    Ok(Envelope {
        trade_v: 1,
        request_id: uuid::Uuid::new_v4().to_string(),
        swap_id: s.id.clone(),
        lot_id: s.lot.id.to_hex(),
        quote_hash: s.quote.as_ref().map(hash).transpose()?.unwrap_or_default(),
        step: step.into(),
        body,
        exp: s
            .quote
            .as_ref()
            .map(|q| q.long + 86400)
            .unwrap_or(s.created + 60),
    })
}
async fn send(
    m: &Market,
    j: &Journal,
    s: &Swap,
    step: &str,
    body: serde_json::Value,
) -> Result<()> {
    m.send(j, s.peer.parse()?, &envelope(s, step, body)?).await
}
pub async fn list(
    home: &Path,
    j: &Journal,
    m: &Market,
    give: Leg,
    want: Leg,
    max_fees: u64,
) -> Result<String> {
    give.asset.fence()?;
    want.asset.fence()?;
    // A lot id is a hash of second-granular terms: two identical lists in the same second
    // (back-to-back requests handed to serve) would share an id, and the second journal row
    // would overwrite the first, orphaning its reservation. Wait for a fresh id instead.
    let mut event = lot_event(&m.keys, give.clone(), want.clone())?;
    for _ in 0..3 {
        if j.get::<Listing>("listing", &event.id.to_hex())
            .await?
            .is_none()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        event = lot_event(&m.keys, give.clone(), want.clone())?;
    }
    ensure!(
        j.get::<Listing>("listing", &event.id.to_hex())
            .await?
            .is_none(),
        "an identical lot was just listed; retry"
    );
    let lot = parse_lot(&event, now())?;
    check_nostr_timing(&lot)?;
    preflight(&lot).await?;
    let plan = mint::plan(home, &lot.give.asset.mint_url, lot.give.net, max_fees).await?;
    let initial = status_event(&m.keys, event.id, 1, event.id, Status::Available)?;
    let id = event.id.to_hex();
    let mut row = Listing {
        published: 0,
        max_fees,
        event,
        statuses: vec![initial],
        plan,
        reservation: uuid::Uuid::new_v4().to_string(),
        active: None,
        cancelled: false,
    };
    // Persist reservation intent first; recovery completes it before any new wallet operation.
    j.put("listing", &id, &row).await?;
    mint::reserve(home, &row.plan, &row.reservation).await?;
    m.publish_durable(j, &row.event).await?;
    m.publish_durable(j, &row.statuses[0]).await?;
    row.published = 1;
    j.put("listing", &id, &row).await?;
    println!(
        "{}",
        serde_json::json!({"lot_id":id,"status_event_id":row.statuses[0].id,"status":"available","debit":row.plan.debit})
    );
    Ok(id)
}
pub async fn cancel(home: &Path, j: &Journal, m: &Market, id: &str) -> Result<()> {
    let mut l: Listing = j.get("listing", id).await?.context("unknown local lot")?;
    ensure!(
        lifecycle(&l.event, &l.statuses)? == Status::Available,
        "lot is terminal"
    );
    ensure!(l.active.is_none(), "cannot cancel active swap");
    l.cancelled = true;
    j.put("listing", id, &l).await?;
    mint::release(home, &l.plan, &l.reservation).await?;
    finish_listing(j, m, &mut l, Status::Cancelled).await
}
async fn finish_listing(j: &Journal, m: &Market, l: &mut Listing, status: Status) -> Result<()> {
    if lifecycle(&l.event, &l.statuses)? == Status::Available {
        let prev = l.statuses.last().context("listing has no status history")?;
        l.statuses.push(status_event(
            &m.keys,
            l.event.id,
            l.statuses.len() as u64 + 1,
            prev.id,
            status,
        )?);
        j.put("listing", &l.event.id.to_hex(), l).await?;
    }
    if l.published == l.statuses.len() {
        return Ok(());
    }
    for e in &l.statuses[l.published..] {
        m.publish_durable(j, e).await?;
    }
    l.published = l.statuses.len();
    j.put("listing", &l.event.id.to_hex(), l).await?;
    println!(
        "{}",
        serde_json::json!({"lot_id":l.event.id,"status_event_ids":l.statuses.iter().map(|e|e.id.to_hex()).collect::<Vec<_>>(),"status":lifecycle(&l.event,&l.statuses)?})
    );
    Ok(())
}
pub async fn start_take(
    home: &Path,
    j: &Journal,
    m: &Market,
    id: &str,
    max_give: u64,
    min_receive: u64,
    max_fees: u64,
) -> Result<String> {
    ensure!(
        j.all::<Swap>("swap").await?.iter().all(terminal),
        "max one open quote per taker"
    );
    let lots = m.discover(Some(id.parse()?)).await?;
    let lot = lots
        .into_iter()
        .next()
        .context("lot unavailable, invalid, expired or cancelled")?;
    let l = parse_lot(&lot, now())?;
    ensure!(lot.pubkey != m.keys.public_key(), "self trade");
    ensure!(l.give.net >= min_receive, "minimum receive cap");
    check_nostr_timing(&l)?;
    preflight(&l).await?;
    let plan = mint::plan(home, &l.want.asset.mint_url, l.want.net, max_fees).await?;
    ensure!(plan.debit <= max_give, "maximum give cap");
    let key = secret();
    let pre = secret();
    let request = Request {
        hash: hex::encode(Sha256::digest(hex::decode(&pre)?)),
        taker_key: pubkey(&key)?,
        funding: Terms::from(&plan),
        max_give,
        min_receive,
        max_fees,
    };
    let s = Swap {
        created: now(),
        id: uuid::Uuid::new_v4().to_string(),
        lot: lot.clone(),
        role: "taker".into(),
        peer: lot.pubkey.to_hex(),
        key,
        preimage: Some(pre),
        request,
        quote: None,
        plan,
        refund_generation: 0,
        state: "requested".into(),
        outgoing: vec![],
        incoming: vec![],
        max_fees,
    };
    save(j, &s).await?;
    mint::reserve(home, &s.plan, &s.id).await?;
    send(m, j, &s, "request", serde_json::to_value(&s.request)?).await?;
    println!("{}", serde_json::json!({"swap_id":s.id,"state":s.state}));
    Ok(s.id)
}
pub async fn handle(home: &Path, j: &Journal, m: &Market, e: &Event) -> Result<()> {
    let message = m.decode(e)?;
    let key = format!("{}_{}", e.pubkey, message.request_id);
    if let Some((digest, reason)) = j.get::<(String, String)>("rejected", &key).await? {
        ensure!(
            digest == hash(&message)?,
            "conflicting rejected request-id reuse"
        );
        if message.step == "request" && legacy_unanswered(&reason)
            && j.get::<Swap>("swap", &message.swap_id).await?.is_none()
        {
            // Preserve the old digest BEFORE removing its rejection, including across a crash.
            if let Some(prior) = j.get::<String>("inbox", &key).await? {
                ensure!(prior == digest, "conflicting request-id reuse");
            }
            j.put("inbox", &key, &digest).await?;
            j.remove("rejected", &key).await?;
        } else {
            anyhow::bail!("recorded rejection: {reason}");
        }
    }
    let result = handle_message(home, j, m, e).await;
    if let Err(ref error) = result {
        if crate::backoff::unanswered(error) {
            retain(j, e, &message).await?;
        } else if message.step == "request" && j.get::<Swap>("swap", &message.swap_id).await?.is_none() {
            j.put("rejected", &key, &(hash(&message)?, error.to_string())).await?;
        }
    }
    result
}

// Narrow legacy compatibility bridge, authorized for recorded unanswered/timeout errors only.
// Never match arbitrary occurrences of "timeout" in peer-provided terms or mint details.
fn legacy_unanswered(reason: &str) -> bool {
    reason == "no answer within the CDK wallet bound"
        || reason.starts_with("mint ") && (
            reason.ends_with("Operation timeout (no mint NUT error; ambiguous)")
            || reason.ends_with("no answer within the nostr bound (ambiguous)")
            || reason.ends_with("keysets: timed out")
            || reason.ends_with("no answer within the HTTP bound (ambiguous)"))
}

async fn request_retry_live(j: &Journal, e: &Event, msg: &Envelope) -> Result<bool> {
    if msg.step != "request" || msg.exp <= now() { return Ok(false); }
    let Some(l) = j.get::<Listing>("listing", &msg.lot_id).await? else { return Ok(false); };
    if l.cancelled || l.active.is_some() || l.published == 0 || parse_lot(&l.event, now()).is_err() { return Ok(false); }
    let key = format!("{}_{}", e.pubkey, msg.request_id);
    Ok(j.get::<String>("inbox", &key).await?.as_ref() == Some(&hash(msg)?))
}

/// An inbound trade message whose handling failed only because a mint did not answer
/// (`backoff::unanswered`), kept so the recovery tick can handle it again.
///
/// Why: before this, such a message was logged as "trade message rejected" and dropped. The
/// counterparty's re-send of the same signed event is a no-op (relays already acked it and never
/// re-deliver a stored event to a live subscription), so one unanswered verification call lost
/// the message for good and both legs refunded. Seen live on two `nostr://` mints
/// (2026-10-11 23:58, 2026-10-12 10:50:39 and 14:54:57 UTC): a taker's check of the maker's
/// `second` lock got no answer and the mint answered again minutes before the claim cutoff.
#[derive(Clone, Serialize, Deserialize)]
pub struct Retained {
    pub event: Event,
    pub swap_id: String,
    pub step: String,
    pub since: u64,
}
/// The swap state in which `step` (from the counterparty of a `role` swap) still has work to do.
/// Any other state means the message was handled or superseded.
fn awaiting(step: &str, role: &str) -> Option<&'static str> {
    Some(match (step, role) {
        ("quote", "taker") => "requested",
        ("first", "maker") => "quoted",
        ("second", "taker") => "first_locked",
        ("claimed", "maker") => "second_locked",
        _ => return None,
    })
}
/// Unix time (local) after which handling `step` can no longer help this swap: the taker's claim
/// send deadline for its check of the maker's lock, the maker's late-first-lock bound for its
/// check of the taker's lock, the quote expiry, or the taker's refund locktime for a `claimed`.
fn retry_until(s: &Swap, step: &str) -> u64 {
    let Some(q) = &s.quote else {
        return s.created + QUOTE_SECONDS;
    };
    match step {
        "first" => q.exp.min(q.short.saturating_sub(q.cutoff)),
        "second" => q.short.saturating_sub(q.cutoff),
        "claimed" => q.long,
        _ => q.exp,
    }
}
/// Journal `e` for retry (keyed by event id: relay duplicates collapse) when its swap still
/// awaits it. Returns whether it is retained.
async fn retain(j: &Journal, e: &Event, msg: &Envelope) -> Result<bool> {
    let swap = j.get::<Swap>("swap", &msg.swap_id).await?;
    if swap.is_none() {
        if !request_retry_live(j, e, msg).await? { return Ok(false); }
        let rows = j.all::<Retained>("inbox_retry").await?;
        if !rows.iter().any(|r| r.event.id == e.id) {
            ensure!(rows.len() < 64 && rows.iter().filter(|r| r.event.pubkey == e.pubkey).count() < 4,
                "initial request retry queue full; retry after pending requests expire");
        }
    }
    if swap.as_ref().is_some_and(|s| terminal(s)
        || s.peer != e.pubkey.to_hex()
        || awaiting(&msg.step, &s.role) != Some(s.state.as_str())
        || now() >= retry_until(s, &msg.step))
    {
        return Ok(false);
    }
    let id = e.id.to_hex();
    if j.get::<Retained>("inbox_retry", &id).await?.is_none() {
        j.put(
            "inbox_retry",
            &id,
            &Retained {
                event: e.clone(),
                swap_id: msg.swap_id.clone(),
                step: msg.step.clone(),
                since: now(),
            },
        )
        .await?;
    }
    Ok(true)
}
/// [`retain`] for a message whose handling ran out of its time budget (no result at all).
async fn retain_timed_out(j: &Journal, m: &Market, e: &Event) -> Result<bool> {
    let msg = m.decode(e)?;
    retain(j, e, &msg).await
}
/// The verification step of `step`, as a claim-critical read: the mint it reads and the moment
/// after which the claim it enables can no longer be sent. Only `nostr://` mints: their outages
/// were silence for minutes, and a long backoff after them ate the claim margin.
fn verification_scope(q: &Quote, step: &str) -> Option<(u64, String)> {
    let mint = match step {
        "second" => &q.lot.give.asset.mint_url,
        "first" => &q.lot.want.asset.mint_url,
        _ => return None,
    };
    crate::transport::is_nostr(mint).then(|| (q.short.saturating_sub(q.cutoff), mint.clone()))
}
/// Run `f` with claim rounds for `scope` (see [`crate::backoff::with_claim`]), or plainly.
/// Boxed: the wrapped mint futures are large and these run inside already large state machines
/// (debug builds overflowed the test thread's stack without it).
async fn with_scope<F: std::future::Future>(scope: Option<(u64, String)>, f: F) -> F::Output {
    let f = Box::pin(f);
    match scope {
        Some((deadline, mint)) => crate::backoff::with_claim(deadline, &mint, f).await,
        None => f.await,
    }
}
/// Handle every retained message again (one bounded attempt each). An entry is dropped once its
/// swap is gone, terminal or past the step, once its deadline passed, or once handling it
/// succeeds or fails for any reason other than an unanswered mint (a refusal stays a refusal).
/// Returns whether any entry is still retained.
async fn replay_retained(home: &Path, j: &Journal, m: &Market) -> Result<bool> {
    let mut pending = false;
    let mut rows = j.all::<Retained>("inbox_retry").await?;
    rows.sort_by_key(|r| r.since);
    for r in rows {
        let id = r.event.id.to_hex();
        let s = j.get::<Swap>("swap", &r.swap_id).await?;
        let request_live = if s.is_none() {
            request_retry_live(j, &r.event, &m.decode(&r.event)?).await?
        } else { false };
        let live = request_live || s.as_ref().is_some_and(|s| {
            !terminal(s)
                && awaiting(&r.step, &s.role) == Some(s.state.as_str())
                && now() < retry_until(s, &r.step)
        });
        if !live {
            j.remove("inbox_retry", &id).await?;
            continue;
        }
        let budget = s
            .as_ref()
            .map_or(std::time::Duration::from_secs(120), item_budget);
        match tokio::time::timeout(budget, Box::pin(handle(home, j, m, &r.event))).await {
            Ok(Ok(())) => {
                j.remove("inbox_retry", &id).await?;
                eprintln!("swap {}: retained {} message handled", r.swap_id, r.step);
            }
            Ok(Err(e))
                if crate::backoff::unanswered(&e)
                    && (request_live || j.get::<Swap>("swap", &r.swap_id)
                        .await?
                        .is_some_and(|s| awaiting(&r.step, &s.role) == Some(s.state.as_str()))) =>
            {
                pending = true;
                eprintln!(
                    "swap {}: retained {} message still waits for its mint: {e}",
                    r.swap_id, r.step
                );
            }
            Ok(Err(e)) => {
                // A refusal, or the message was handled and a later step of the advance failed
                // (that step is retried by the swap's own recovery).
                j.remove("inbox_retry", &id).await?;
                eprintln!("swap {}: retained {} message done: {e}", r.swap_id, r.step);
            }
            Err(_) => {
                pending = true;
                eprintln!(
                    "swap {}: retained {} message timed out; kept",
                    r.swap_id, r.step
                );
            }
        }
    }
    Ok(pending)
}
async fn handle_message(home: &Path, j: &Journal, m: &Market, e: &Event) -> Result<()> {
    let msg = m.decode(e)?;
    let dedup = format!("{}_{}", e.pubkey, msg.request_id);
    let digest = hash(&msg)?;
    if let Some(prior) = j.get::<String>("inbox", &dedup).await? {
        ensure!(prior == digest, "conflicting request-id reuse");
    } else {
        ensure!(msg.exp > now(), "expired message");
        j.put("inbox", &dedup, &digest).await?;
    }
    if msg.step == "request" {
        if let Some(s) = j.get::<Swap>("swap", &msg.swap_id).await? {
            ensure!(
                s.peer == e.pubkey.to_hex()
                    && s.lot.id.to_hex() == msg.lot_id
                    && hash(&s.request)? == hash(&serde_json::from_value::<Request>(msg.body)?)?,
                "swap id conflict"
            );
            return send(m, j, &s, "quote", serde_json::to_value(&s.quote)?).await;
        }
        ensure!(
            msg.exp > now() && msg.quote_hash.is_empty(),
            "expired or pre-bound quote request"
        );
        let mut l: Listing = j
            .get("listing", &msg.lot_id)
            .await?
            .context("unknown lot")?;
        ensure!(
            !l.cancelled
                && l.published > 0
                && lifecycle(&l.event, &l.statuses)? == Status::Available
                && l.active.is_none(),
            "lot busy or terminal"
        );
        let lot = parse_lot(&l.event, now())?;
        let swaps = j.all::<Swap>("swap").await?;
        ensure!(
            swaps
                .iter()
                .filter(|s| s.role == "maker" && !terminal(s))
                .count()
                < 4,
            "maker quote limit"
        );
        ensure!(
            !swaps
                .iter()
                .any(|s| s.role == "maker" && s.peer == e.pubkey.to_hex() && !terminal(s)),
            "taker quote limit"
        );
        let req: Request = serde_json::from_value(msg.body)?;
        ensure!(
            req.hash.len() == 64 && hex::encode(hex::decode(&req.hash)?) == req.hash,
            "noncanonical hash"
        );
        canonical_key(&req.taker_key)?;
        let _: cashu::nuts::PublicKey = req.taker_key.parse()?;
        ensure!(
            req.min_receive <= lot.give.net && req.max_give >= req.funding.debit,
            "caps"
        );
        let (g, f) = gross(lot.want.net, req.funding.ppk)?;
        ensure!(
            req.funding.gross == g
                && req.funding.claim_fee == f
                && Some(req.funding.debit) == g.checked_add(req.funding.lock_fee)
                && req
                    .funding
                    .claim_fee
                    .checked_add(req.funding.lock_fee)
                    .is_some_and(|n| n <= req.max_fees),
            "fee terms"
        );
        ensure!(
            req.funding.claim_fee <= l.max_fees,
            "incoming claim fee exceeds maker admission cap"
        );
        preflight(&lot).await?;
        validate_terms(home, &lot.want, &req.funding).await?;
        validate_terms(home, &lot.give, &Terms::from(&l.plan)).await?;
        mint::unspent(&l.plan.mint, &l.plan.inputs).await?;
        // Reads may have occupied a full mint-aware budget. Recheck admission deadlines
        // immediately before the first durable quote, not only before those reads.
        ensure!(msg.exp > now(), "expired request after mint checks");
        parse_lot(&l.event, now())?;
        let key = secret();
        let (long, short, cutoff, margin) = timing(&lot)?;
        let issued = now();
        let q = Quote {
            lot,
            lot_id: msg.lot_id.clone(),
            swap_id: msg.swap_id.clone(),
            maker: m.keys.public_key().to_hex(),
            taker: e.pubkey.to_hex(),
            maker_key: pubkey(&key)?,
            request: req.clone(),
            give: Terms::from(&l.plan),
            issued,
            exp: issued + QUOTE_SECONDS,
            long: issued + long,
            short: issued + short,
            cutoff,
            margin,
        };
        let s = Swap {
            created: now(),
            id: msg.swap_id.clone(),
            lot: l.event.clone(),
            role: "maker".into(),
            peer: e.pubkey.to_hex(),
            key,
            preimage: None,
            request: req,
            quote: Some(q.clone()),
            plan: l.plan.clone(),
            refund_generation: 0,
            state: "quoted".into(),
            outgoing: vec![],
            incoming: vec![],
            max_fees: l.max_fees,
        };
        // Swap row is authoritative; home lock serializes admission. Recovery restores active index.
        save(j, &s).await?;
        l.active = Some(s.id.clone());
        j.put("listing", &msg.lot_id, &l).await?;
        send(m, j, &s, "quote", serde_json::to_value(q)?).await?;
        return Ok(());
    }
    let mut s: Swap = j.get("swap", &msg.swap_id).await?.context("unknown swap")?;
    ensure!(
        e.pubkey.to_hex() == s.peer && msg.lot_id == s.lot.id.to_hex(),
        "peer/lot mismatch"
    );
    if terminal(&s) {
        return Ok(());
    }
    if msg.step == "quote" && s.role == "taker" && s.state == "requested" && s.quote.is_none() {
        m.require_sent(j, s.peer.parse()?, &s.id, "request").await?;
        let q: Quote = serde_json::from_value(msg.body)?;
        let lot = parse_lot(&s.lot, now())?;
        let (long, short, cutoff, margin) = timing(&lot)?;
        ensure!(
            q.lot == lot
                && q.lot_id == s.lot.id.to_hex()
                && q.swap_id == s.id
                && q.maker == s.peer
                && q.taker == m.keys.public_key().to_hex()
                && hash(&q.request)? == hash(&s.request)?,
            "quote binding mismatch"
        );
        ensure!(
            q.issued <= now() + 60
                && q.exp == q.issued + 60
                && q.exp > now()
                && q.long == q.issued + long
                && q.short == q.issued + short
                && q.cutoff == cutoff
                && q.margin == margin,
            "quote deadlines"
        );
        ensure!(hash(&q)? == msg.quote_hash, "quote digest");
        let (g, f) = gross(lot.give.net, q.give.ppk)?;
        ensure!(q.give.gross == g && q.give.claim_fee == f, "maker net fees");
        ensure!(
            q.give.claim_fee <= s.max_fees,
            "incoming claim fee exceeds taker admission cap"
        );
        validate_terms(home, &lot.give, &q.give).await?;
        canonical_key(&q.maker_key)?;
        s.quote = Some(q);
        s.state = "accepted".into();
        save(j, &s).await?;
    } else {
        let q = s.quote.as_ref().context("missing quote")?;
        ensure!(msg.quote_hash == hash(q)?, "quote digest mismatch");
        match msg.step.as_str() {
            "first" if s.role == "maker" && s.state == "quoted" => {
                m.require_sent(j, s.peer.parse()?, &s.id, "quote").await?;
                ensure!(
                    now() < q.exp && now() + q.cutoff < q.short,
                    "late first lock"
                );
                let p: Proofs = serde_json::from_value(msg.body)?;
                let c =
                    mint::conditions(&q.request.hash, &q.maker_key, &q.request.taker_key, q.long)?;
                with_scope(
                    verification_scope(q, "first"),
                    mint::validate(
                        home,
                        &q.lot.want.asset.mint_url,
                        &p,
                        q.lot.want.net,
                        q.request.funding.gross,
                        q.request.funding.claim_fee,
                        &c,
                        q.request.funding.ppk,
                        q.request.funding.keyset,
                    ),
                )
                .await?;
                s.incoming = p;
                s.state = "first_validated".into();
                save(j, &s).await?;
            }
            "second" if s.role == "taker" && s.state == "first_locked" => {
                m.require_sent(j, s.peer.parse()?, &s.id, "first").await?;
                let p: Proofs = serde_json::from_value(msg.body)?;
                let c =
                    mint::conditions(&q.request.hash, &q.request.taker_key, &q.maker_key, q.short)?;
                // The claim this check enables must be sent before `short - cutoff`: a silent
                // `nostr://` mint gets claim rounds here, never a backoff past that margin.
                with_scope(
                    verification_scope(q, "second"),
                    mint::validate(
                        home,
                        &q.lot.give.asset.mint_url,
                        &p,
                        q.lot.give.net,
                        q.give.gross,
                        q.give.claim_fee,
                        &c,
                        q.give.ppk,
                        q.give.keyset,
                    ),
                )
                .await?;
                s.incoming = p;
                s.state = "second_validated".into();
                save(j, &s).await?;
            }
            "claimed" if s.role == "maker" && s.state == "second_locked" => {
                let pre = msg.body["preimage"]
                    .as_str()
                    .context("missing claimed preimage")?;
                ensure!(
                    mint::matches_preimage(pre, &q.request.hash),
                    "invalid claimed preimage"
                );
                s.preimage = Some(hex::encode(hex::decode(pre)?));
                s.state = "claiming".into();
                save(j, &s).await?;
            }
            _ => {}
        }
    }
    advance(home, j, m, &mut s).await
}
pub async fn advance(home: &Path, j: &Journal, m: &Market, s: &mut Swap) -> Result<()> {
    if terminal(s) {
        return Ok(());
    }
    // While a claim may have to land, its requests to the claim mint (only that mint) are
    // retried on a short cadence however long that mint's backoff (see `backoff`).
    let result = match claim_deadline(s).map(|(deadline, mint)| (deadline, mint.to_owned())) {
        Some((deadline, mint)) => {
            crate::backoff::with_claim(deadline, &mint, advance_inner(home, j, m, s)).await
        }
        None => advance_inner(home, j, m, s).await,
    };
    // Run even when redemption returned an error. Persist terminal manual-recovery
    // status before the next tick; never turn a quarantined claim into refund authority.
    for (suffix, state) in [
        ("claim".to_string(), "claim_quarantined"),
        (
            if s.refund_generation == 0 {
                "refund".into()
            } else {
                format!("refund-{}", s.refund_generation)
            },
            "refund_quarantined",
        ),
    ] {
        if state == "refund_quarantined"
            && s.role == "maker"
            && s.preimage.is_some()
            && s.state != "settling"
        {
            continue;
        }
        if j.get::<mint::Attempt>("attempt", &format!("{}-{suffix}", s.id))
            .await?
            .is_some_and(|a| a.quarantined)
        {
            if s.state != state {
                s.state = state.into();
                save(j, s).await?;
                println!(
                    "{}",
                    serde_json::json!({"swap_id":s.id,"state":s.state,"manual_recovery":true})
                );
            }
            break;
        }
    }
    result
}
/// The claim this swap may have to land, with the moment (mint clock) after which it can lose
/// funds, and the claim mint:
/// - maker (claiming the taker's lock on the want mint with the learned preimage, or still
///   watching its own lock's NUT-07 witness for it): `long`, the taker's refund locktime;
/// - taker (claiming the maker's lock on the give mint): `short - cutoff`, its send deadline.
///
/// Taker refunds (after `long + margin`) and maker refunds (after `short + margin`) have no
/// such moment: the refund path stays open, so a later refund loses nothing.
pub fn claim_deadline(s: &Swap) -> Option<(u64, &str)> {
    let q = s.quote.as_ref()?;
    match (s.role.as_str(), s.state.as_str()) {
        ("maker", "second_locked" | "claiming" | "lock_unforwardable") => {
            Some((q.long, q.lot.want.asset.mint_url.as_str()))
        }
        ("taker", "second_validated") => Some((
            q.short.saturating_sub(q.cutoff),
            q.lot.give.asset.mint_url.as_str(),
        )),
        _ => None,
    }
}
/// [`claim_deadline`] once the claim is possible now (a maker knows the preimage): such an item
/// is never paced. A maker still watching for the preimage is paced like any other item.
fn claim_possible(s: &Swap) -> Option<(u64, &str)> {
    claim_deadline(s).filter(|_| s.role == "taker" || s.preimage.is_some())
}
/// The mints one swap's recovery may contact.
fn swap_mints(s: &Swap) -> Vec<&str> {
    let mut mints = vec![s.plan.mint.as_str()];
    if let Some(q) = &s.quote {
        mints.push(&q.lot.give.asset.mint_url);
        mints.push(&q.lot.want.asset.mint_url);
    }
    mints.sort_unstable();
    mints.dedup();
    mints
}
async fn advance_inner(home: &Path, j: &Journal, m: &Market, s: &mut Swap) -> Result<()> {
    let (overall_budget, refund_budget) = advance_budgets(swap_is_nostr(s));
    let end = tokio::time::Instant::now() + overall_budget;
    if terminal(s) {
        return Ok(());
    }
    let Some(q) = s.quote.clone() else {
        return Ok(());
    };
    let own_mint = s.plan.mint.clone();
    let taker = s.role == "taker";
    let lock_id = format!("{}-lock", s.id);
    let claim_id = format!("{}-claim", s.id);
    if ["accepted", "first_validated", "lock_reconciling"].contains(&s.state.as_str()) {
        let result: Result<Proofs> = async {
            if j.get::<mint::Attempt>("attempt", &lock_id).await?.is_none() {
                ensure!(
                    now() < q.exp && now() + q.cutoff < q.short,
                    "quote expired before lock"
                );
                recovery_preflight(&q.lot).await?;
            }
            let c = if taker {
                mint::conditions(&q.request.hash, &q.maker_key, &q.request.taker_key, q.long)?
            } else {
                mint::conditions(&q.request.hash, &q.request.taker_key, &q.maker_key, q.short)?
            };
            let exp = if taker {
                q.exp.saturating_sub(20)
            } else {
                q.exp
            };
            mint::lock(home, j, &lock_id, &s.plan, &c, exp.min(q.short - q.cutoff)).await
        }
        .await;
        match result {
            Ok(proofs) => {
                s.outgoing = proofs;
                s.state = if taker {
                    "first_locked"
                } else {
                    "second_locked"
                }
                .into();
                save(j, s).await?;
            }
            Err(e) => {
                eprintln!("lock {}: {e}", s.id);
                if let Some(proofs) = mint::unforwardable(j, &lock_id).await? {
                    s.outgoing = proofs;
                    s.state = "lock_unforwardable".into();
                    save(j, s).await?;
                } else if j
                    .get::<mint::Attempt>("attempt", &lock_id)
                    .await?
                    .is_some_and(|a| a.abandoned)
                {
                    // Persist a retryable state before release; a late result is restored next tick.
                    s.state = "lock_reconciling".into();
                    save(j, s).await?;
                    if !mint::lock_not_landed(j, &lock_id).await? {
                        return Ok(());
                    }
                    if taker {
                        if let Err(e) = mint::release(home, &s.plan, &s.id).await {
                            eprintln!("lock release {}: {e}", s.id);
                            return Ok(());
                        }
                    } else {
                        let mut l: Listing = j
                            .get("listing", &s.lot.id.to_hex())
                            .await?
                            .context("missing maker listing")?;
                        // Keep the active index intact if a late lock races release. Recovery
                        // must reach execute again, not loop in cancelled-listing cleanup.
                        if let Err(e) = mint::release(home, &l.plan, &l.reservation).await {
                            eprintln!("maker lock release {}: {e}", s.id);
                            return Ok(());
                        }
                        l.cancelled = true;
                        l.active = None;
                        j.put("listing", &s.lot.id.to_hex(), &l).await?;
                        // The money is settled (fresh not-landed proof, inputs released): expire
                        // NOW. Publishing the cancelled status is market bookkeeping that listing
                        // recovery retries (`cancelled && Available`); a market outage must never
                        // keep this swap, and the per-taker guard on it, open forever.
                        s.state = "expired".into();
                        save(j, s).await?;
                        if let Err(e) = finish_listing(j, m, &mut l, Status::Cancelled).await {
                            eprintln!("expired lock {} listing publication: {e}", s.id);
                        }
                        return Ok(());
                    }
                    s.state = "expired".into();
                    save(j, s).await?;
                    return Ok(());
                }
            }
        }
    }
    if s.state == "lock_unforwardable" {
        // Do not forward even if a later restore repairs DLEQ. Settle owned change and
        // funding reservations independently of the normal timed refund path.
        if let Err(e) = mint::settle_unforwardable(home, j, &lock_id).await {
            eprintln!("unforwardable settle {}: {e}", s.id);
        }
    }
    if taker && s.state == "second_validated" {
        let result: Result<()> = async {
            let started = j
                .get::<mint::Attempt>("attempt", &claim_id)
                .await?
                .is_some();
            let time = wallet::action_time(&q.lot.give.asset.mint_url).await?;
            if started || time + q.cutoff < q.short {
                mint::redeem_claim(
                    home,
                    j,
                    &claim_id,
                    &q.lot.give.asset.mint_url,
                    &s.incoming,
                    &s.key,
                    s.preimage.as_ref().context("missing taker preimage")?,
                    q.give.claim_fee,
                    Some(q.short - q.cutoff),
                )
                .await?;
                s.state = "claimed".into();
                save(j, s).await?;
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            eprintln!("claim {}: {e}", s.id);
        }
    }
    if !taker
        && [
            "second_locked",
            "claiming",
            "settling",
            "lock_unforwardable",
        ]
        .contains(&s.state.as_str())
    {
        // One deadline for the whole advance. Own-mint refund work is capped so it can
        // never starve the claim; the counterparty claim inherits everything the refund
        // did not use. No own-mint failure propagates past the claim: an outage or black
        // hole on one mint must never starve or skip the other (live claims stay claimable).
        let refund_end = (tokio::time::Instant::now() + refund_budget).min(end);
        match tokio::time::timeout_at(refund_end, maker_refund(home, j, s, &q, &own_mint)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => eprintln!("maker refund phase {}: {e}", s.id),
            Err(_) => eprintln!("maker refund phase {}: budget exhausted", s.id),
        }
        match tokio::time::timeout_at(end, maker_claim(home, j, m, s, &q, &claim_id)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => eprintln!("maker claim {}: {e}", s.id),
            Err(_) => eprintln!("maker claim {}: budget exhausted", s.id),
        }
        let refund_id = refund_attempt_id(s);
        let all_spent = mint::states(&own_mint, &s.outgoing)
            .await?
            .states
            .iter()
            .all(|p| p.state == cashu::nuts::State::Spent);
        if all_spent && s.state == "settling" && mint::refund_settled(j, &refund_id).await? {
            s.state = "complete".into();
            save(j, s).await?;
        } else if all_spent
            && s.preimage.is_none()
            && mint::refunded_all(j, &refund_id, &s.outgoing).await?
        {
            s.state = "refunded".into();
            save(j, s).await?;
            let mut l: Listing = j
                .get("listing", &s.lot.id.to_hex())
                .await?
                .context("missing maker listing")?;
            l.cancelled = true;
            finish_listing(j, m, &mut l, Status::Cancelled).await?;
        }
    }
    if taker && s.state == "claimed" {
        // Terminal bookkeeping spends nothing. Local time alone is a sufficient lower
        // bound on max(local, mint) once long + margin has passed, even if info is offline.
        let local = now();
        let time = if local > q.long + q.margin {
            local
        } else {
            wallet::action_time(&own_mint).await?
        };
        if time > q.long + q.margin {
            // We received the maker's funds: our lock remains theirs forever, never refund it.
            s.state = "complete_unclaimed".into();
            save(j, s).await?;
        } else if mint::witness(&own_mint, &s.outgoing, &q.request.hash)
            .await?
            .is_some()
        {
            s.state = "complete".into();
            save(j, s).await?;
        }
    }
    if taker
        && ["first_locked", "second_validated", "lock_unforwardable"].contains(&s.state.as_str())
    {
        let time = wallet::refund_time(&own_mint).await?;
        // Errors, PENDING, matching claim witnesses or nonempty restores all fail closed.
        if time > q.long + q.margin
            && mint::claim_not_landed(
                j,
                &claim_id,
                &q.lot.give.asset.mint_url,
                &s.incoming,
                &q.request.hash,
            )
            .await?
        {
            let refund_id = format!("{}-refund", s.id);
            if j.get::<mint::Attempt>("attempt", &refund_id)
                .await?
                .is_some()
            {
                mint::execute(home, j, &refund_id).await?;
            } else {
                let remaining = mint::refundable(&own_mint, &s.outgoing).await?;
                ensure!(!remaining.is_empty(), "no unspent taker refund inputs");
                mint::redeem(
                    home, j, &refund_id, &own_mint, &remaining, &s.key, "", s.max_fees, None,
                )
                .await?;
            }
            s.state = "refunded".into();
            save(j, s).await?;
        }
    }
    match s.state.as_str() {
        "first_locked" => send(m, j, s, "first", serde_json::to_value(&s.outgoing)?).await?,
        "second_locked" => send(m, j, s, "second", serde_json::to_value(&s.outgoing)?).await?,
        "claimed" => {
            send(
                m,
                j,
                s,
                "claimed",
                serde_json::json!({"preimage":s.preimage}),
            )
            .await?
        }
        "complete" => {
            let _ = send(m, j, s, "done", serde_json::json!({})).await;
            println!(
                "{}",
                serde_json::json!({"swap_id":s.id,"state":"complete","role":s.role})
            );
        }
        _ => {}
    }
    Ok(())
}
pub async fn recover(home: &Path, j: &Journal, m: &Market) -> Result<()> {
    recover_pass(home, j, m).await.map(|_| ())
}
async fn recover_pass(home: &Path, j: &Journal, m: &Market) -> Result<bool> {
    let mut deferred = match crate::money::recover(home, j).await {
        Ok(pending) => pending,
        Err(error) => {
            eprintln!(
                "money recovery deferred; journal retained: {}",
                crate::redact_reason(&format!("{error:#}"))
            );
            true
        }
    };
    match replay_retained(home, j, m).await {
        Ok(pending) => deferred |= pending,
        Err(error) => {
            deferred = true;
            eprintln!("retained message replay deferred: {error}");
        }
    }
    let mut swaps = j.all::<Swap>("swap").await?;
    crate::backoff::order(&mut swaps, |s| &s.id);
    for mut s in swaps {
        // Pacing: a live swap whose mints are at their request budget waits (recover) or is
        // left for a later tick (serve). An item whose claim is possible is never paced.
        if !terminal(&s)
            && s.quote.is_some()
            && !crate::backoff::ready(&s.id, &swap_mints(&s), claim_possible(&s)).await
        {
            deferred = true;
            continue;
        }
        let budget = item_budget(&s);
        let item = async {
            if s.state == "requested" && now() > s.created + 60 {
                mint::release(home, &s.plan, &s.id).await?;
                s.state = "expired".into();
                save(j, &s).await?;
                return Ok::<(), anyhow::Error>(());
            }

            if terminal(&s) {
                if s.role == "maker" && ["complete", "refunded"].contains(&s.state.as_str()) {
                    let mut l: Listing = j
                        .get("listing", &s.lot.id.to_hex())
                        .await?
                        .context("missing maker listing")?;
                    let publication = finish_listing(
                        j,
                        m,
                        &mut l,
                        if s.state == "complete" {
                            Status::Sold
                        } else {
                            Status::Cancelled
                        },
                    )
                    .await;
                    if let Err(error) = publication {
                        deferred |= !error.is::<crate::market::PublicationAbandoned>();
                        eprintln!("terminal listing publication: {error}");
                    }
                }
                return Ok::<(), anyhow::Error>(());
            }
            if ["quoted", "accepted", "first_validated"].contains(&s.state.as_str())
                && s.quote.as_ref().is_some_and(|q| now() >= q.exp)
                && j.get::<mint::Attempt>("attempt", &format!("{}-lock", s.id))
                    .await?
                    .is_none()
            {
                if s.role == "taker" {
                    mint::release(home, &s.plan, &s.id).await?;
                }
                s.state = "expired".into();
                save(j, &s).await?;
                if s.role == "maker" {
                    let mut l: Listing = j
                        .get("listing", &s.lot.id.to_hex())
                        .await?
                        .context("missing maker listing")?;
                    l.active = None;
                    if l.cancelled {
                        mint::release(home, &l.plan, &l.reservation).await?;
                    }
                    j.put("listing", &s.lot.id.to_hex(), &l).await?;
                }
                return Ok::<(), anyhow::Error>(());
            }
            if s.role == "taker" && s.state == "requested" {
                mint::reserve(home, &s.plan, &s.id).await?;
                let _ = send(m, j, &s, "request", serde_json::to_value(&s.request)?).await;
            }
            if s.role == "maker" && s.state == "quoted" {
                let _ = send(m, j, &s, "quote", serde_json::to_value(&s.quote)?).await;
            }
            advance(home, j, m, &mut s).await
        };
        let item = tokio::time::timeout(budget, item).await;
        if !matches!(item, Ok(Ok(()))) {
            deferred = true;
            eprintln!(
                "swap {}: recovery deferred ({}); journal retained",
                s.id,
                crate::deferral_reason(&item, budget.as_secs())
            );
        }
    }
    let swaps = j.all::<Swap>("swap").await?;
    for mut l in j.all::<Listing>("listing").await? {
        let publication: Result<()> = async {
            if l.active
                .as_ref()
                .is_some_and(|id| swaps.iter().any(|s| &s.id == id && s.state == "expired"))
            {
                l.active = None;
                if l.cancelled {
                    mint::release(home, &l.plan, &l.reservation).await?;
                }
                j.put("listing", &l.event.id.to_hex(), &l).await?;
            }
            if l.cancelled && lifecycle(&l.event, &l.statuses)? == Status::Available {
                if l.active.is_none() {
                    mint::release(home, &l.plan, &l.reservation).await?;
                }
                finish_listing(j, m, &mut l, Status::Cancelled).await?;
            }
            if let Some(s) = swaps
                .iter()
                .find(|s| s.role == "maker" && s.lot.id == l.event.id && !terminal(s))
            {
                l.active = Some(s.id.clone());
                j.put("listing", &l.event.id.to_hex(), &l).await?;
            }
            if l.active.is_none()
                && !l.cancelled
                && lifecycle(&l.event, &l.statuses)? == Status::Available
            {
                mint::reserve(home, &l.plan, &l.reservation).await?;
                if l.published == 0 {
                    m.publish_durable(j, &l.event).await?;
                    m.publish_durable(j, &l.statuses[0]).await?;
                    l.published = 1;
                    j.put("listing", &l.event.id.to_hex(), &l).await?;
                }
            }
            Ok(())
        }
        .await;
        if let Err(error) = publication {
            deferred |= !error.is::<crate::market::PublicationAbandoned>();
            eprintln!("listing recovery: {error}");
        }
    }
    if m.retry_publications(j).await.is_err() {
        deferred = true;
    }
    Ok(deferred)
}
pub async fn run(home: &Path, j: &Journal, m: &mut Market, until: Option<&str>) -> Result<()> {
    run_with(home, j, m, until, None).await
}
/// [`run`], also executing requests handed to `serve` over its local socket. Every request
/// runs here, in this one loop, between inbox messages and recovery ticks: serve stays the
/// single writer and each request keeps journal-before-effect ordering.
pub async fn run_with(
    home: &Path,
    j: &Journal,
    m: &mut Market,
    until: Option<&str>,
    mut jobs: Option<tokio::sync::mpsc::Receiver<crate::serve::Job>>,
) -> Result<()> {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(3));
    let mut warned = crate::observe::Warned::default();
    loop {
        tokio::select! {e=m.inbox.recv()=>{if let Some(e)=e{match tokio::time::timeout(message_budget(j,m,&e).await, handle(home,j,m,&e)).await {
            Ok(Ok(())) => {},
            Ok(Err(err)) if retained(j, &e).await => eprintln!("trade message waits for its mint (no answer); retained, retried on the recovery tick: {err}"),
            Ok(Err(err)) => eprintln!("trade message rejected: {err}"),
            Err(_) => match retain_timed_out(j, m, &e).await {
                Ok(true) => eprintln!("trade message timed out; retained, retried on the recovery tick"),
                _ => eprintln!("trade message timed out; dropped; journal retained"),
            }
        }}},
        Some(job) = async { match jobs.as_mut() { Some(rx) => rx.recv().await, None => std::future::pending().await } } => crate::serve::execute(home, j, m, job).await,
        _=tick.tick()=>{
            recover(home,j,m).await?;
            // Display only: derived from the journal, never fed back into the state machine.
            let now = now();
            for u in warned.due(&crate::observe::unresponsive(home, now).unwrap_or_default()) {
                eprintln!("{}", u.json(now));
            }
        }}
        if let Some(id) = until {
            if let Some(s) = j.get::<Swap>("swap", id).await? {
                if terminal(&s) {
                    println!("{}", serde_json::json!({"swap_id":id,"state":s.state}));
                    ensure!(
                        ["complete", "complete_unclaimed"].contains(&s.state.as_str()),
                        "trade ended {}",
                        s.state
                    );
                    return Ok(());
                }
            }
        }
    }
}

async fn retained(j: &Journal, e: &Event) -> bool {
    j.get::<Retained>("inbox_retry", &e.id.to_hex())
        .await
        .is_ok_and(|r| r.is_some())
}

async fn validate_terms(home: &Path, leg: &Leg, t: &Terms) -> Result<()> {
    let w = wallet::wallet(home, &leg.asset.mint_url).await?;
    crate::wallet::bounded_for(&leg.asset.mint_url, w.refresh_keysets())
        .await
        .context("CDK wallet request not answered")??;
    let active = crate::wallet::bounded_for(&leg.asset.mint_url, w.fetch_active_keyset())
        .await
        .context("CDK wallet request not answered")??;
    ensure!(
        t.keyset == active.id && t.ppk == active.input_fee_ppk,
        "quote keyset/fee schedule mismatch"
    );
    let (gross, claim) = gross(leg.net, t.ppk)?;
    ensure!(
        t.gross == gross && t.claim_fee == claim && Some(t.debit) == gross.checked_add(t.lock_fee),
        "quote fee arithmetic"
    );
    Ok(())
}
/// A bounded pass left existing authorizations unresolved. CLI exit code: 3.
#[derive(Debug)]
pub struct RecoveryIncomplete;
impl std::fmt::Display for RecoveryIncomplete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "recovery incomplete; retained items require a later pass or serve"
        )
    }
}
impl std::error::Error for RecoveryIncomplete {}

/// Print only public identifiers and states, never recovery material.
pub async fn recovery_status(j: &Journal, mut unresolved: bool) -> Result<()> {
    let mut manual = false;
    for s in j.all::<Swap>("swap").await? {
        manual |= s.state.ends_with("_quarantined");
        unresolved |= !terminal(&s);
        println!(
            "{}",
            serde_json::json!({"swap_id":s.id,"state":s.state,"terminal":terminal(&s),"manual_recovery":s.state.ends_with("_quarantined")})
        );
    }
    for f in j.all::<crate::money::Funding>("funding").await? {
        unresolved |= !f.done && !f.expired_unpaid;
        println!(
            "{}",
            serde_json::json!({"funding_id":f.id,"terminal":f.done || f.expired_unpaid,"expired_unpaid":f.expired_unpaid})
        );
    }
    for a in j.all::<crate::money::Withdrawal>("withdrawal").await? {
        unresolved |= !a.terminal();
        println!("{}", a.summary());
    }
    for r in j.all::<crate::receive::Receipt>("receive").await? {
        unresolved |= r.unresolved();
        manual |= r.state == crate::receive::ReceiveState::Quarantined;
        println!("{}", r.summary());
    }
    for r in j.all::<crate::send::SendAttempt>("send").await? {
        unresolved |= !r.terminal();
        manual |= r.manual_recovery();
        println!("{}", r.summary());
    }
    println!(
        "{}",
        serde_json::json!({"status":if unresolved {"recovery_incomplete"} else {"recovery_complete"}})
    );
    if unresolved {
        return Err(RecoveryIncomplete.into());
    }
    if manual {
        return Err(ManualRecovery.into());
    }
    Ok(())
}
#[derive(Debug)]
pub struct ManualRecovery;
impl std::fmt::Display for ManualRecovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "manual recovery required; quarantined outputs retained")
    }
}
impl std::error::Error for ManualRecovery {}

/// One bounded pass, without admitting fresh requests or waiting for deadlines.
/// The historical function name is retained for library callers.
pub async fn recover_until_settled(home: &Path, j: &Journal, m: &mut Market) -> Result<()> {
    let deferred = recover_pass(home, j, m).await?;
    recovery_status(j, deferred).await
}

fn canonical_key(key: &str) -> Result<()> {
    let parsed: cashu::nuts::PublicKey = key.parse()?;
    ensure!(key == parsed.to_string(), "noncanonical Cashu key");
    Ok(())
}

/// Phase budgets. HTTP-only swaps keep the historical numbers: 60 s per inbound message (`run`
/// -> `handle`), 100 s per maker advance (refund + claim), of which own-mint refund work may use
/// at most 50 s, and 120 s per recovery item.
///
/// A swap with a `nostr://` leg gets budgets derived from the most `nostr://` mint calls each
/// phase can make, every call bounded by `transport::NOSTR_OUTER` (connector window 30 s + outer
/// margin 10 s = 40 s; CDK wallet calls get the same bound through `wallet::bounded_for`). CDK
/// reads after `refresh_keysets` hit the wallet's metadata cache, so only the calls below reach
/// the mint. A budget that is too short cuts a call mid-window; that stays ambiguous and never
/// costs money (journaled first, `exp` capped at `send_before`, restore before replay), but on a
/// slow relay it can repeat on every tick and nothing advances. So the budgets cover the worst
/// case:
///
/// - refund phase, [`NOSTR_REFUND_CALLS`] = 17: witness 1; resume a started refund (restore,
///   NUT-07, swap, NUT-07 = 4) and, if it failed, its evidence (restore, NUT-07, restore = 3);
///   refund time 1; refundable 1; a fresh refund (refresh_keysets: info + keysets + keys = 3,
///   then restore, NUT-07, swap, NUT-07 = 4) 7. 17 x 40 s = 680 s.
/// - claim phase, [`NOSTR_CLAIM_CALLS`] = 8: refresh_keysets 3; restore, NUT-07, swap, NUT-07 4;
///   plus one slot for the listing publication. 8 x 40 s = 320 s.
/// - advance = refund + claim = 25 calls = 1000 s, so the refund cap still never starves the claim.
/// - recovery item = advance + the trailing own-mint NUT-07 ([`NOSTR_TRAILING_CALLS`] = 1) =
///   1040 s. This also covers the largest single message step, the maker's `first` (validate 4,
///   preflight of both mints 4, lock 14 = [`NOSTR_FIRST_STEP_CALLS`] = 22 = 880 s), and the
///   abandoned-lock expiry (execute: restore, mint time, restore, NUT-07, restore;
///   `lock_not_landed`: restore, NUT-07, restore; release NUT-07 = 9 calls = 360 s; the old
///   140 s fit only ~15 s per call).
/// - message = item, since the maker's `claimed` step runs a whole advance.
///
/// Claims stay safe with these larger budgets because no budget decides WHEN a claim may land:
/// the taker's claim is sent only while mint time + cutoff < short and its `exp` is capped at
/// `short - cutoff`; the maker claims until `long`. [`nostr_timing_fits`] checks at list/take that
/// both claim chains fit their windows with every call at its bound.
///
/// Serve runs messages and recovery items one at a time, so on a slow relay one swap can hold
/// the loop for up to 1040 s. That delays other swaps (liveness); their deadlines stay
/// mint-enforced, so a late claim is never sent, and the trade refunds.
const ADVANCE_BUDGET_SECONDS: u64 = 100;
/// HTTP cap on own-mint refund work inside that deadline; the claim gets the remainder.
const REFUND_BUDGET_SECONDS: u64 = 50;
const HTTP_ITEM_BUDGET_SECONDS: u64 = 120;
const HTTP_MESSAGE_BUDGET_SECONDS: u64 = 60;
pub const NOSTR_REFUND_CALLS: u64 = 17;
pub const NOSTR_CLAIM_CALLS: u64 = 8;
pub const NOSTR_TRAILING_CALLS: u64 = 1;
pub const NOSTR_FIRST_STEP_CALLS: u64 = 22;
/// `nostr://` calls before the taker's claim swap is sent, counted from the `second` message:
/// validation (refresh_keysets 3, NUT-07 1) then the claim (mint time 1, refresh_keysets 3,
/// restore 1, mint time 1, NUT-07 1, mint time 1).
pub const NOSTR_TAKER_CLAIM_CALLS: u64 = 12;
/// Longest one market send may take before the counterparty can act on it (one 12 s relay ACK
/// wait, `Market::publish_durable`).
const MARKET_SEND_SECONDS: u64 = 12;
#[cfg(feature = "lab")]
static LAB_ADVANCE_BUDGETS_MS: std::sync::Mutex<Option<(u64, u64)>> = std::sync::Mutex::new(None);
/// Lab-only: scale the (overall, refund) advance budgets so slow-mint tests stay fast.
#[cfg(feature = "lab")]
pub fn lab_set_advance_budgets_ms(budgets: Option<(u64, u64)>) {
    *LAB_ADVANCE_BUDGETS_MS
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = budgets;
}
fn nostr_secs(calls: u64) -> u64 {
    calls * crate::transport::NOSTR_OUTER.as_secs()
}
/// (overall, refund) advance budgets for a swap with (`nostr`) or without a `nostr://` leg.
pub fn advance_budgets(nostr: bool) -> (std::time::Duration, std::time::Duration) {
    #[cfg(feature = "lab")]
    if let Some((overall, refund)) = *LAB_ADVANCE_BUDGETS_MS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
    {
        return (
            std::time::Duration::from_millis(overall),
            std::time::Duration::from_millis(refund),
        );
    }
    let (overall, refund) = if nostr {
        (
            nostr_secs(NOSTR_REFUND_CALLS + NOSTR_CLAIM_CALLS),
            nostr_secs(NOSTR_REFUND_CALLS),
        )
    } else {
        (ADVANCE_BUDGET_SECONDS, REFUND_BUDGET_SECONDS)
    };
    (
        std::time::Duration::from_secs(overall),
        std::time::Duration::from_secs(refund),
    )
}
/// Recovery budget for one swap item (see [`ADVANCE_BUDGET_SECONDS`] for the arithmetic).
pub fn item_budget_for(nostr: bool) -> std::time::Duration {
    if !nostr {
        return std::time::Duration::from_secs(HTTP_ITEM_BUDGET_SECONDS);
    }
    (advance_budgets(true).0 + std::time::Duration::from_secs(nostr_secs(NOSTR_TRAILING_CALLS)))
        .max(std::time::Duration::from_secs(nostr_secs(
            NOSTR_FIRST_STEP_CALLS,
        )))
}
/// Budget for one inbound trade message: 60 s, or the recovery item budget for a `nostr://` swap.
pub fn message_budget_for(nostr: bool) -> std::time::Duration {
    if nostr {
        item_budget_for(true)
    } else {
        std::time::Duration::from_secs(HTTP_MESSAGE_BUDGET_SECONDS)
    }
}
fn lot_is_nostr(content: &str) -> bool {
    serde_json::from_str::<Lot>(content)
        .is_ok_and(|l| l.give.asset.is_nostr() || l.want.asset.is_nostr())
}
fn swap_is_nostr(s: &Swap) -> bool {
    lot_is_nostr(&s.lot.content)
}
fn item_budget(s: &Swap) -> std::time::Duration {
    item_budget_for(swap_is_nostr(s))
}
/// The budget for the swap (or, for a fresh request, the listing) this message names.
async fn message_budget(j: &Journal, m: &Market, e: &Event) -> std::time::Duration {
    let lot = async {
        let msg = m.decode(e).ok()?;
        if let Some(s) = j.get::<Swap>("swap", &msg.swap_id).await.ok()? {
            return Some(s.lot.content);
        }
        Some(
            j.get::<Listing>("listing", &msg.lot_id)
                .await
                .ok()??
                .event
                .content,
        )
    }
    .await;
    message_budget_for(lot.as_deref().is_some_and(lot_is_nostr))
}
/// Whether both claims of a swap with a `nostr://` leg fit their windows with every mint call at
/// its bound (`long`, `short`, `cutoff` in seconds after the quote is issued):
/// - taker: the maker's lock is sent before `QUOTE_SECONDS` and answered within `NOSTR_OUTER`,
///   `second` reaches the taker within one market send, then [`NOSTR_TAKER_CLAIM_CALLS`] calls
///   precede the claim swap, which must be sent before `short - cutoff`. Production:
///   60 + 40 + 12 + 12 x 40 = 592 <= 900 - 180 = 720.
/// - maker: the preimage can be revealed as late as `short - cutoff`; the maker learns it within
///   one market send (or its next advance's NUT-07 witness) and claims within one full advance and
///   one recovery item, before the taker can refund at `long`. Production:
///   720 + 12 + 1040 + 1040 = 2812 <= 3600.
pub fn nostr_timing_fits(long: u64, short: u64, cutoff: u64) -> Result<()> {
    let outer = crate::transport::NOSTR_OUTER.as_secs();
    let taker = QUOTE_SECONDS + outer + MARKET_SEND_SECONDS + nostr_secs(NOSTR_TAKER_CLAIM_CALLS);
    ensure!(
        short > cutoff && taker <= short - cutoff,
        "refused: a nostr:// leg's worst-case claim ({taker} s of mint calls) does not fit before \
         the claim deadline ({} s); nothing was reserved or sent",
        short.saturating_sub(cutoff)
    );
    let maker = short - cutoff + MARKET_SEND_SECONDS + 2 * item_budget_for(true).as_secs();
    ensure!(
        maker <= long,
        "refused: a nostr:// leg's worst-case maker claim ({maker} s) does not fit before the \
         taker's refund locktime ({long} s); nothing was reserved or sent"
    );
    Ok(())
}
/// [`nostr_timing_fits`] for a lot with a `nostr://` leg (no-op otherwise). Lab short deadlines
/// are exempt: they deliberately overrun to exercise the refund paths.
fn check_nostr_timing(l: &Lot) -> Result<()> {
    if !(l.give.asset.is_nostr() || l.want.asset.is_nostr()) {
        return Ok(());
    }
    #[cfg(feature = "lab")]
    if std::env::var("TRADE_LAB_SECONDS").is_ok() {
        return Ok(());
    }
    let (long, short, cutoff, _) = timing(l)?;
    nostr_timing_fits(long, short, cutoff)
}
fn refund_attempt_id(s: &Swap) -> String {
    if s.refund_generation == 0 {
        format!("{}-refund", s.id)
    } else {
        format!("{}-refund-{}", s.id, s.refund_generation)
    }
}
/// Own-mint refund phase. Only journal errors propagate; every own-mint RPC failure
/// is logged so the caller always proceeds to the claim phase.
async fn maker_refund(
    home: &Path,
    j: &Journal,
    s: &mut Swap,
    q: &Quote,
    own_mint: &str,
) -> Result<()> {
    let mut refund_id = refund_attempt_id(s);
    if s.preimage.is_none() {
        if let Some(pre) = mint::refund_preimage(j, &refund_id, &q.request.hash).await? {
            s.preimage = Some(pre);
            s.state = "claiming".into();
            save(j, s).await?;
        }
    }
    if s.preimage.is_none() {
        // The witness is how the maker learns the preimage it must claim with before `long`.
        // On a `nostr://` own mint, poll it in claim rounds (this mint only; refunds keep backoff).
        let scope = crate::transport::is_nostr(own_mint).then(|| (q.long, own_mint.to_owned()));
        match with_scope(scope, mint::witness(own_mint, &s.outgoing, &q.request.hash)).await {
            Ok(Some(pre)) => {
                s.preimage = Some(pre);
                s.state = "claiming".into();
                save(j, s).await?;
            }
            Ok(None) => {}
            Err(e) => eprintln!("witness {}: {e}", s.id),
        }
    }
    let mut refund_started = j
        .get::<mint::Attempt>("attempt", &refund_id)
        .await?
        .is_some();
    if refund_started {
        if let Err(e) = mint::execute(home, j, &refund_id).await {
            eprintln!("refund recovery {}: {e}", s.id);
            let retired = match mint::failed_refund(j, &refund_id, &q.request.hash).await {
                Ok(pre) => pre.is_some(),
                Err(e) => {
                    eprintln!("refund evidence {}: {e}", s.id);
                    false
                }
            };
            // Positive witness knowledge is useful even when absence is ambiguous.
            // Never let a restore outage discard a preimage already observed this tick.
            if s.preimage.is_none() {
                if let Some(pre) = mint::refund_preimage(j, &refund_id, &q.request.hash).await? {
                    s.preimage = Some(pre);
                    s.state = "claiming".into();
                    save(j, s).await?;
                }
            }
            if retired {
                // Preserve the failed attempt forever; a durable generation selects fresh inputs.
                s.refund_generation = s
                    .refund_generation
                    .checked_add(1)
                    .context("refund generation overflow")?;
                save(j, s).await?;
                refund_id = format!("{}-refund-{}", s.id, s.refund_generation);
                refund_started = false;
            }
        }
    }
    let time = wallet::refund_time(own_mint).await?;
    if !refund_started && time > q.short + q.margin {
        // A NUT-07 failure on our own mint is logged, never propagated: the claim
        // phase runs next on the same tick regardless.
        match mint::refundable(own_mint, &s.outgoing).await {
            Ok(remaining) if !remaining.is_empty() => {
                if let Err(e) = mint::redeem(
                    home, j, &refund_id, own_mint, &remaining, &s.key, "", s.max_fees, None,
                )
                .await
                {
                    eprintln!("maker refund {}: {e}", s.id);
                }
            }
            Ok(_) => {}
            Err(e) => eprintln!("refundable {}: {e}", s.id),
        }
    }
    Ok(())
}
async fn maker_claim(
    home: &Path,
    j: &Journal,
    m: &Market,
    s: &mut Swap,
    q: &Quote,
    claim_id: &str,
) -> Result<()> {
    let Some(pre) = s.preimage.as_ref() else {
        return Ok(());
    };
    if s.state == "settling" {
        return Ok(());
    }
    mint::redeem_claim(
        home,
        j,
        claim_id,
        &q.lot.want.asset.mint_url,
        &s.incoming,
        &s.key,
        pre,
        q.request.funding.claim_fee,
        None,
    )
    .await?;
    s.state = "settling".into();
    save(j, s).await?;
    let mut l: Listing = j
        .get("listing", &s.lot.id.to_hex())
        .await?
        .context("missing listing")?;
    finish_listing(j, m, &mut l, Status::Sold).await
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    use std::time::Duration;
    #[test]
    fn http_budgets_unchanged() {
        assert_eq!(
            advance_budgets(false),
            (Duration::from_secs(100), Duration::from_secs(50))
        );
        assert_eq!(item_budget_for(false), Duration::from_secs(120));
        assert_eq!(message_budget_for(false), Duration::from_secs(60));
    }
    #[test]
    fn nostr_budgets_cover_worst_case_calls() {
        let outer = crate::transport::NOSTR_OUTER;
        assert_eq!(outer, Duration::from_secs(40));
        let (advance, refund) = advance_budgets(true);
        assert_eq!(refund, outer * 17, "refund phase: 17 calls");
        assert_eq!(advance, outer * 25, "refund 17 + claim 8 calls");
        assert!(
            advance - refund >= outer * NOSTR_CLAIM_CALLS as u32,
            "the claim always keeps its own worst case"
        );
        let item = item_budget_for(true);
        assert_eq!(item, Duration::from_secs(1040));
        assert!(
            item >= advance + outer,
            "item covers advance + trailing NUT-07"
        );
        assert!(item >= outer * 22, "item covers the maker's first step");
        assert!(
            item >= outer * 9,
            "item covers the abandoned-lock expiry chain"
        );
        assert_eq!(message_budget_for(true), item);
    }
    #[test]
    fn production_nostr_claims_fit_and_short_windows_are_refused() {
        nostr_timing_fits(LONG_SECONDS, SHORT_SECONDS, CLAIM_CUTOFF_SECONDS).unwrap();
        // Taker: 60 + 40 + 12 + 480 = 592 s must fit before short - cutoff.
        assert!(nostr_timing_fits(LONG_SECONDS, 771, 180).is_err());
        nostr_timing_fits(LONG_SECONDS, 772, 180).unwrap();
        // Maker: (short - cutoff) + 12 + 2 x 1040 must fit before long.
        let e = nostr_timing_fits(2811, SHORT_SECONDS, CLAIM_CUTOFF_SECONDS).unwrap_err();
        assert!(e.to_string().contains("refused"), "{e}");
        nostr_timing_fits(2812, SHORT_SECONDS, CLAIM_CUTOFF_SECONDS).unwrap();
    }
}
