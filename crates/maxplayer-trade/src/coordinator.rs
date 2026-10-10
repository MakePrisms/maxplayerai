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
fn terminal(s: &Swap) -> bool {
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
fn timing(l: &Lot) -> Result<(u64, u64, u64, u64)> {
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
    for a in [&l.give.asset, &l.want.asset] {
        a.fence()?;
        wallet::preflight(&a.mint_url).await?;
    }
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
    let event = lot_event(&m.keys, give, want)?;
    let lot = parse_lot(&event, now())?;
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
        anyhow::bail!("recorded rejection: {reason}");
    }
    let result = handle_message(home, j, m, e).await;
    if let Err(ref error) = result {
        if message.step == "request" && j.get::<Swap>("swap", &message.swap_id).await?.is_none() {
            j.put("rejected", &key, &(hash(&message)?, error.to_string()))
                .await?;
        }
    }
    result
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
    let result = advance_inner(home, j, m, s).await;
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
async fn advance_inner(home: &Path, j: &Journal, m: &Market, s: &mut Swap) -> Result<()> {
    let (overall_budget, refund_budget) = advance_budgets();
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
                preflight(&q.lot).await?;
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
                        finish_listing(j, m, &mut l, Status::Cancelled).await?;
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
            eprintln!("money recovery deferred; journal retained: {error}");
            true
        }
    };
    let swaps = j.all::<Swap>("swap").await?;
    for mut s in swaps {
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
        if !matches!(tokio::time::timeout(budget, item).await, Ok(Ok(()))) {
            deferred = true;
            eprintln!(
                "swap {}: recovery deferred (error or timeout); journal retained",
                s.id
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
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(3));
    loop {
        tokio::select! {e=m.inbox.recv()=>{if let Some(e)=e{match tokio::time::timeout(std::time::Duration::from_secs(60), handle(home,j,m,&e)).await {
            Ok(Ok(())) => {}, Ok(Err(err)) => eprintln!("trade message rejected: {err}"), Err(_) => eprintln!("trade message timed out; dropped; journal retained")
        }}},_=tick.tick()=>recover(home,j,m).await?}
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

async fn validate_terms(home: &Path, leg: &Leg, t: &Terms) -> Result<()> {
    let w = wallet::wallet(home, &leg.asset.mint_url).await?;
    crate::wallet::bounded_for(&leg.asset.mint_url, w.refresh_keysets())
        .await
        .context("CDK wallet request timed out")??;
    let active = crate::wallet::bounded_for(&leg.asset.mint_url, w.fetch_active_keyset())
        .await
        .context("CDK wallet request timed out")??;
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
        unresolved |= !r.terminal();
        manual |= r.state == crate::receive::ReceiveState::Quarantined;
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

/// Deadline for the maker refund+claim phases of one advance, measured from the start of
/// the advance. Two budgets enclose an advance: the 120 s recovery item budget
/// (`recover_pass`) and the 60 s inbound message budget (`run` -> `handle`). 100 s leaves
/// the trailing own-mint `states()` call (<= 20 s) inside the 120 s item budget. On the
/// 60 s message path the outer timeout may cut the claim first; `claiming` and the
/// preimage are saved before that, so the next recovery tick claims with the full budget.
const ADVANCE_BUDGET_SECONDS: u64 = 100;
/// Cap on own-mint refund work inside that deadline; the claim gets the remainder.
const REFUND_BUDGET_SECONDS: u64 = 50;
#[cfg(feature = "lab")]
static LAB_ADVANCE_BUDGETS_MS: std::sync::Mutex<Option<(u64, u64)>> = std::sync::Mutex::new(None);
/// Lab-only: scale the (overall, refund) advance budgets so slow-mint tests stay fast.
#[cfg(feature = "lab")]
pub fn lab_set_advance_budgets_ms(budgets: Option<(u64, u64)>) {
    *LAB_ADVANCE_BUDGETS_MS
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = budgets;
}
fn advance_budgets() -> (std::time::Duration, std::time::Duration) {
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
    (
        std::time::Duration::from_secs(ADVANCE_BUDGET_SECONDS),
        std::time::Duration::from_secs(REFUND_BUDGET_SECONDS),
    )
}
/// Recovery budget for one swap item: 120 s (HTTP), or room for the full advance deadline plus
/// one trailing `nostr://` call (the own-mint `states()` after the claim phase) when either leg
/// is on a `nostr://` mint. The enclosing budgets (this one, the 100 s advance, the 50 s refund
/// phase, the 60 s message path) are not sized per nostr call and can drop one mid-window. A drop
/// is only ever ambiguous: every swap is journaled before it is sent, its `exp` is capped at
/// `send_before`, recovery restores the same outputs before replaying the identical request, and
/// abandonment needs `send_before + NOSTR_ABANDON_GRACE_SECONDS` on the mint clock plus a fresh
/// not-landed check. The cost is liveness (deferred to the next tick), not money.
fn item_budget(s: &Swap) -> std::time::Duration {
    let http = std::time::Duration::from_secs(120);
    let Ok(lot) = serde_json::from_str::<Lot>(&s.lot.content) else {
        return http;
    };
    if lot.give.asset.is_nostr() || lot.want.asset.is_nostr() {
        http.max(advance_budgets().0 + crate::transport::NOSTR_OUTER)
    } else {
        http
    }
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
        match mint::witness(own_mint, &s.outgoing, &q.request.hash).await {
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
