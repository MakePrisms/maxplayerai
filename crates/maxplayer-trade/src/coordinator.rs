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
}
fn terminal(s: &Swap) -> bool {
    ["complete", "refunded", "expired"].contains(&s.state.as_str())
}
fn timing(l: &Lot) -> Result<(u64, u64, u64, u64)> {
    #[cfg(feature = "lab")]
    if std::env::var("TRADE_LAB_SECONDS").is_ok() {
        for a in [&l.give.asset, &l.want.asset] {
            let u = url::Url::parse(&a.mint_url)?;
            ensure!(
                u.host_str() == Some("127.0.0.1"),
                "lab timing requires loopback mints"
            );
        }
        return Ok((24, 8, 2, 1));
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
    m.publish(&row.event).await?;
    m.publish(&row.statuses[0]).await?;
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
    l.cancelled = true;
    j.put("listing", id, &l).await?;
    if l.active.is_none() {
        mint::release(home, &l.plan, &l.reservation).await?;
    }
    finish_listing(j, m, &mut l, Status::Cancelled).await
}
async fn finish_listing(j: &Journal, m: &Market, l: &mut Listing, status: Status) -> Result<()> {
    if lifecycle(&l.event, &l.statuses)? == Status::Available {
        let prev = l.statuses.last().unwrap();
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
    for e in &l.statuses {
        m.publish(e).await?;
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
        ensure!(hex::decode(&req.hash)?.len() == 32, "invalid hash");
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
    if msg.step == "quote" && s.role == "taker" && s.quote.is_none() {
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
        validate_terms(home, &lot.give, &q.give).await?;
        let _: cashu::nuts::PublicKey = q.maker_key.parse()?;
        s.quote = Some(q);
        s.state = "accepted".into();
        save(j, &s).await?;
    } else {
        let q = s.quote.as_ref().context("missing quote")?;
        ensure!(msg.quote_hash == hash(q)?, "quote digest mismatch");
        match msg.step.as_str() {
            "first" if s.role == "maker" && s.state == "quoted" => {
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
                let p: Proofs = serde_json::from_value(msg.body)?;
                let c =
                    mint::conditions(&q.request.hash, &q.request.taker_key, &q.maker_key, q.short)?;
                mint::validate(
                    home,
                    &q.lot.give.asset.mint_url,
                    &p,
                    q.lot.give.net,
                    &c,
                    q.give.ppk,
                    q.give.keyset,
                )
                .await?;
                s.incoming = p;
                s.state = "second_validated".into();
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
    let Some(q) = s.quote.clone() else {
        return Ok(());
    };
    let own_mint = s.plan.mint.clone();
    let long = s.role == "taker";
    if s.state == "accepted" || s.state == "first_validated" {
        let attempt = format!("{}-lock", s.id);
        let started = j.get::<mint::Attempt>("attempt", &attempt).await?.is_some();
        if !started {
            ensure!(
                now() < q.exp && now() + q.cutoff < q.short,
                "quote expired before lock"
            );
            preflight(&q.lot).await?;
        }
        let c = if long {
            mint::conditions(&q.request.hash, &q.maker_key, &q.request.taker_key, q.long)?
        } else {
            mint::conditions(&q.request.hash, &q.request.taker_key, &q.maker_key, q.short)?
        };
        s.outgoing = mint::lock(
            home,
            j,
            &attempt,
            &s.plan,
            &c,
            q.exp.min(q.short - q.cutoff),
        )
        .await?;
        s.state = if long {
            "first_locked"
        } else {
            "second_locked"
        }
        .into();
        save(j, s).await?;
    }
    if s.state == "second_validated"
        && (now() + q.cutoff < q.short
            || j.get::<mint::Attempt>("attempt", &format!("{}-claim", s.id))
                .await?
                .is_some())
    {
        mint::redeem(
            home,
            j,
            &format!("{}-claim", s.id),
            &q.lot.give.asset.mint_url,
            &s.incoming,
            &s.key,
            s.preimage.as_ref().unwrap(),
            s.max_fees,
            Some(q.short - q.cutoff),
        )
        .await?;
        s.state = "claimed".into();
        save(j, s).await?;
    }
    if !long && s.state == "second_locked" {
        let refund_id = format!("{}-refund", s.id);
        let refund_started = j
            .get::<mint::Attempt>("attempt", &refund_id)
            .await?
            .is_some();
        let mut refunded = false;
        if refund_started {
            // Restore our refund before interpreting an empty-preimage SPENT witness.
            // If the claim won the race, execution fails and its witness drives the other claim.
            refunded = mint::execute(home, j, &refund_id).await.is_ok();
        }
        if !refunded {
            if let Some(pre) = mint::witness(&own_mint, &s.outgoing, &q.request.hash).await? {
                s.preimage = Some(pre);
                s.state = "claiming".into();
                save(j, s).await?;
            } else if now() > q.short + q.margin {
                mint::redeem(
                    home,
                    j,
                    &refund_id,
                    &own_mint,
                    &s.outgoing,
                    &s.key,
                    "",
                    s.max_fees,
                    None,
                )
                .await?;
                refunded = true;
            }
        }
        if refunded {
            s.state = "refunded".into();
            save(j, s).await?;
            let mut l: Listing = j.get("listing", &s.lot.id.to_hex()).await?.unwrap();
            l.cancelled = true;
            finish_listing(j, m, &mut l, Status::Cancelled).await?;
        }
    }
    if !long && s.state == "claiming" {
        mint::redeem(
            home,
            j,
            &format!("{}-claim", s.id),
            &q.lot.want.asset.mint_url,
            &s.incoming,
            &s.key,
            s.preimage.as_ref().unwrap(),
            s.max_fees,
            Some(q.long),
        )
        .await?;
        s.state = "complete".into();
        save(j, s).await?;
        let mut l: Listing = j.get("listing", &s.lot.id.to_hex()).await?.unwrap();
        finish_listing(j, m, &mut l, Status::Sold).await?;
    }
    if long && s.state == "claimed" {
        if mint::witness(&own_mint, &s.outgoing, &q.request.hash)
            .await?
            .is_some()
        {
            s.state = "complete".into();
            save(j, s).await?;
        }
    }
    if long
        && ["first_locked", "second_validated"].contains(&s.state.as_str())
        && now() > q.long + q.margin
    {
        mint::redeem(
            home,
            j,
            &format!("{}-refund", s.id),
            &own_mint,
            &s.outgoing,
            &s.key,
            "",
            s.max_fees,
            None,
        )
        .await?;
        s.state = "refunded".into();
        save(j, s).await?;
    }
    match s.state.as_str() {
        "first_locked" => send(m, j, s, "first", serde_json::to_value(&s.outgoing)?).await?,
        "second_locked" => send(m, j, s, "second", serde_json::to_value(&s.outgoing)?).await?,
        "claimed" => send(m, j, s, "claimed", serde_json::json!({})).await?,
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
    let swaps = j.all::<Swap>("swap").await?;
    for mut l in j.all::<Listing>("listing").await? {
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
                m.publish(&l.event).await?;
                m.publish(&l.statuses[0]).await?;
                l.published = 1;
                j.put("listing", &l.event.id.to_hex(), &l).await?;
            }
        }
    }
    for mut s in swaps {
        if s.state == "requested" && now() > s.created + 60 {
            mint::release(home, &s.plan, &s.id).await?;
            s.state = "expired".into();
            save(j, &s).await?;
            continue;
        }

        if terminal(&s) {
            if s.role == "maker" && ["complete", "refunded"].contains(&s.state.as_str()) {
                let mut l: Listing = j.get("listing", &s.lot.id.to_hex()).await?.unwrap();
                finish_listing(
                    j,
                    m,
                    &mut l,
                    if s.state == "complete" {
                        Status::Sold
                    } else {
                        Status::Cancelled
                    },
                )
                .await?;
            }
            continue;
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
                let mut l: Listing = j.get("listing", &s.lot.id.to_hex()).await?.unwrap();
                l.active = None;
                if l.cancelled {
                    mint::release(home, &l.plan, &l.reservation).await?;
                }
                j.put("listing", &s.lot.id.to_hex(), &l).await?;
            }
            continue;
        }
        if s.role == "taker" && s.state == "requested" {
            mint::reserve(home, &s.plan, &s.id).await?;
            let _ = send(m, j, &s, "request", serde_json::to_value(&s.request)?).await;
        }
        if s.role == "maker" && s.state == "quoted" {
            let _ = send(m, j, &s, "quote", serde_json::to_value(&s.quote)?).await;
        }
        if let Err(e) = advance(home, j, m, &mut s).await {
            eprintln!("recovery {}: {e}", s.id);
        }
    }
    Ok(())
}
pub async fn run(home: &Path, j: &Journal, m: &mut Market, until: Option<&str>) -> Result<()> {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(3));
    loop {
        tokio::select! {e=m.inbox.recv()=>{if let Some(e)=e{if let Err(err)=handle(home,j,m,&e).await{eprintln!("trade message rejected: {err}");}}},_=tick.tick()=>recover(home,j,m).await?}
        if let Some(id) = until {
            if let Some(s) = j.get::<Swap>("swap", id).await? {
                if terminal(&s) {
                    println!("{}", serde_json::json!({"swap_id":id,"state":s.state}));
                    ensure!(s.state == "complete", "trade ended {}", s.state);
                    return Ok(());
                }
            }
        }
    }
}

async fn validate_terms(home: &Path, leg: &Leg, t: &Terms) -> Result<()> {
    let w = wallet::wallet(home, &leg.asset.mint_url).await?;
    w.refresh_keysets().await?;
    let active = w.fetch_active_keyset().await?;
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
/// Recover only existing authorizations; do not admit fresh quote requests.
pub async fn recover_until_settled(home: &Path, j: &Journal, m: &mut Market) -> Result<()> {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(3));
    loop {
        if j.all::<Swap>("swap").await?.iter().all(terminal) {
            return Ok(());
        }
        tokio::select! {
         _=tick.tick()=>recover(home,j,m).await?,
         event=m.inbox.recv()=>if let Some(event)=event {
          if m.decode(&event).is_ok_and(|msg|msg.step!="request") {
            if let Err(error)=handle(home,j,m,&event).await{eprintln!("recovery message rejected: {error}");}
          }
         }
        }
    }
}
