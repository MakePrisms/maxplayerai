//! Standalone fixed-lot protocol primitives. No job or daemon integration.
use anyhow::{Context, Result, bail, ensure};
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
pub const LOT: u16 = 3410;
pub const STATUS: u16 = 3411;
pub const TRADE: u16 = 23412;
pub const LONG_SECONDS: u64 = 3600;
pub const SHORT_SECONDS: u64 = 900;
pub const QUOTE_SECONDS: u64 = 60;
pub const CLAIM_CUTOFF_SECONDS: u64 = 180;
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Asset {
    pub mint_url: String,
    pub unit: String,
}
/// Canonical `nostr://<lowercase bech32 npub>` for a CLI-supplied nostr mint: accepts any scheme
/// case, a hex or bech32 key in any case, and one trailing slash. Never used for peer/lot data.
fn canonical_nostr(mint: &str) -> Result<String> {
    ensure!(transport::is_nostr(mint), "not a nostr:// mint");
    let key = mint[mint_wire_scheme_len()..].trim_end_matches('/');
    ensure!(
        !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric()),
        "nostr mint must be nostr://<npub or 64-hex key> with no path, query or fragment"
    );
    let key = key.to_ascii_lowercase();
    let pk = if key.len() == 64 {
        PublicKey::from_hex(&key)?
    } else {
        PublicKey::from_bech32(&key)?
    };
    Ok(format!("nostr://{}", pk.to_bech32()?))
}
fn mint_wire_scheme_len() -> usize {
    maxplayer_core::mint_wire::NOSTR_MINT_SCHEME.len()
}
impl Asset {
    /// Canonicalize a mint the operator typed: `nostr://` input may use hex or uppercase. Lots and
    /// peer messages go through [`Asset::new`]/[`Asset::validate`], which accept canonical only.
    pub fn from_cli(mint: &str) -> Result<Self> {
        if transport::is_nostr(mint) {
            return Self::new(&canonical_nostr(mint)?);
        }
        Self::new(mint)
    }
    pub fn new(mint: &str) -> Result<Self> {
        if transport::is_nostr(mint) {
            // Strict: exactly `nostr://<lowercase npub>` (what the market site also accepts).
            ensure!(
                maxplayer_core::mint_wire::nostr_mint_npub(mint)
                    .is_some_and(|npub| mint.len() == mint_wire_scheme_len() + npub.len())
                    && canonical_nostr(mint).is_ok_and(|c| c == mint),
                "noncanonical nostr mint (expected nostr://<lowercase npub>)"
            );
            return Ok(Self {
                mint_url: mint.into(),
                unit: "sat".into(),
            });
        }
        let u = url::Url::parse(mint)?;
        ensure!(
            u.username().is_empty()
                && u.password().is_none()
                && u.query().is_none()
                && u.fragment().is_none(),
            "ambiguous mint URL"
        );
        ensure!(
            ["http", "https"].contains(&u.scheme()),
            "unsupported mint scheme"
        );
        ensure!(u.host_str().is_some(), "missing mint host");
        let canonical = u.as_str().trim_end_matches('/').to_string();
        ensure!(
            mint.trim_end_matches('/') == canonical,
            "noncanonical mint URL"
        );
        Ok(Self {
            mint_url: canonical,
            unit: "sat".into(),
        })
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(self.unit == "sat", "unsupported unit");
        ensure!(Self::new(&self.mint_url)? == *self, "noncanonical asset");
        Ok(())
    }
    pub fn is_nostr(&self) -> bool {
        transport::is_nostr(&self.mint_url)
    }
    pub fn fence(&self) -> Result<()> {
        self.validate()?;
        if self.is_nostr() {
            // Authenticated by the npub; relays are fenced in `transport::check_mint_relay`.
            return Ok(());
        }
        let u = url::Url::parse(&self.mint_url)?;
        let h = u.host_str().context("missing mint host")?;
        let local = h == "localhost"
            || h.trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .map(|x| x.is_loopback())
                .unwrap_or(false);
        ensure!(
            u.scheme() == "https" || (local && u.scheme() == "http"),
            "HTTP allowed only on loopback"
        );
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Leg {
    #[serde(flatten)]
    pub asset: Asset,
    pub net: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeadlinePolicy {
    pub long_seconds: u64,
    pub short_seconds: u64,
    pub min_gap_seconds: u64,
}
impl Default for DeadlinePolicy {
    fn default() -> Self {
        Self {
            long_seconds: 3600,
            short_seconds: 900,
            min_gap_seconds: 2700,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Lot {
    pub trade_v: u8,
    pub give: Leg,
    pub want: Leg,
    pub maker_trade_pubkey: String,
    pub expires_at: u64,
    pub deadline_policy: DeadlinePolicy,
    pub fee_policy: String,
}
fn tag(event: &Event, name: &str) -> Result<String> {
    let tags: Vec<_> = event
        .tags
        .iter()
        .filter(|t| t.as_slice().first().map(|s| s.as_str()) == Some(name))
        .collect();
    ensure!(
        tags.len() == 1 && tags[0].as_slice().len() == 2,
        "missing or duplicate tag {name}"
    );
    Ok(tags[0].as_slice()[1].clone())
}
fn common(e: &Event, kind: u16) -> Result<()> {
    e.verify()?;
    ensure!(e.kind == Kind::Custom(kind), "wrong event kind");
    ensure!(
        e.content.len() <= 8192 && e.tags.len() <= 16,
        "oversized event"
    );
    ensure!(
        e.tags
            .iter()
            .all(|t| t.as_slice().iter().all(|s| s.len() <= 512)),
        "oversized tag"
    );
    ensure!(
        tag(e, "t")? == "maxplayer" && tag(e, "v")? == "1",
        "wrong protocol tags"
    );
    Ok(())
}
pub fn parse_lot(e: &Event, now: u64) -> Result<Lot> {
    common(e, LOT)?;
    let l: Lot = serde_json::from_str(&e.content)?;
    ensure!(l.trade_v == 1, "unknown protocol version");
    l.give.asset.validate()?;
    l.want.asset.validate()?;
    ensure!(l.give.asset != l.want.asset, "same-asset swap");
    ensure!(
        l.give.net > 0 && l.want.net > 0 && l.give.net <= 1_000_000 && l.want.net <= 1_000_000,
        "invalid or oversized lot"
    );
    ensure!(
        l.expires_at
            == e.created_at
                .as_secs()
                .checked_add(86400)
                .ok_or_else(|| anyhow::anyhow!("expiry overflow"))?
            && now < l.expires_at
            && e.created_at.as_secs() <= now.saturating_add(60),
        "expired or future listing"
    );
    ensure!(
        l.deadline_policy == DeadlinePolicy::default(),
        "unsupported deadline policy"
    );
    ensure!(
        l.fee_policy == "sender-funds-net-v1",
        "unsupported fee policy"
    );
    ensure!(
        l.maker_trade_pubkey == e.pubkey.to_hex(),
        "maker identity mismatch"
    );
    for (name, value) in [
        ("g", l.give.asset.mint_url.clone()),
        ("w", l.want.asset.mint_url.clone()),
        ("u", l.give.asset.unit.clone()),
        ("x", l.want.asset.unit.clone()),
        ("expiration", l.expires_at.to_string()),
    ] {
        ensure!(tag(e, name)? == value, "content/tag disagreement");
    }
    Ok(l)
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Available,
    Sold,
    Cancelled,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Revision {
    pub trade_v: u8,
    pub lot_id: String,
    pub seq: u64,
    pub prev: String,
    pub status: Status,
}
pub fn lifecycle(lot: &Event, events: &[Event]) -> Result<Status> {
    ensure!(events.len() <= 256, "status history exceeds bound");
    let mut revisions = BTreeMap::new();
    let mut seen = HashSet::new();
    for e in events {
        if !seen.insert(e.id) {
            continue;
        }
        common(e, STATUS)?;
        ensure!(e.pubkey == lot.pubkey, "unauthorized status signer");
        let r: Revision = serde_json::from_str(&e.content)?;
        ensure!(
            r.trade_v == 1 && r.lot_id == lot.id.to_hex() && tag(e, "e")? == r.lot_id,
            "wrong status binding"
        );
        ensure!(
            revisions.insert(r.seq, (e, r)).is_none(),
            "forked status history"
        );
    }
    let mut prev = lot.id.to_hex();
    let mut status = Status::Available;
    let mut seq = 0;
    for (n, (e, r)) in revisions {
        ensure!(n == seq + 1 && r.prev == prev, "incomplete status history");
        ensure!(
            status == Status::Available,
            "terminal status cannot reopen or revise"
        );
        ensure!(
            seq != 0 || r.status == Status::Available,
            "initial status must be available"
        );
        prev = e.id.to_hex();
        status = r.status;
        seq = n;
    }
    ensure!(seq > 0, "initial available revision missing");
    Ok(status)
}
pub fn fee(ppk: impl IntoIterator<Item = u64>) -> Result<u64> {
    let sum = ppk.into_iter().try_fold(0u64, |a, b| {
        a.checked_add(b)
            .ok_or_else(|| anyhow::anyhow!("fee overflow"))
    })?;
    Ok(sum / 1000 + u64::from(sum % 1000 != 0))
}
/// Fixed-point gross amount using binary denominations. Never deduct fees from the stated net.
pub fn gross(net: u64, ppk: u64) -> Result<(u64, u64)> {
    ensure!(net > 0, "zero net");
    for extra in 0..=4096u64 {
        let n = net
            .checked_add(extra)
            .ok_or_else(|| anyhow::anyhow!("amount overflow"))?;
        let f = fee(std::iter::repeat_n(ppk, n.count_ones() as usize))?;
        if extra == f {
            return Ok((n, f));
        }
    }
    bail!("cannot solve bounded fee-inclusive split")
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn asset_identity_includes_mint() {
        assert_ne!(
            Asset::new("http://127.0.0.1:1").unwrap(),
            Asset::new("http://127.0.0.1:2").unwrap()
        );
    }
    #[test]
    fn fake_mints_allowed() {
        for u in [
            "https://testnut.cashudevkit.org",
            "https://testnut.cashu.space",
            "http://127.0.0.1:123",
            "http://[::1]:123",
        ] {
            Asset::new(u).unwrap().fence().unwrap();
        }
    }
    #[test]
    fn real_mint_transport_allowed() {
        assert!(
            Asset::new("https://mint.minibits.cash/Bitcoin")
                .unwrap()
                .fence()
                .is_ok()
        );
    }
    #[test]
    fn aliases_denied() {
        for u in [
            "https://TESTNUT.cashu.space",
            "https://testnut.cashu.space?mint=evil",
            "https://user@testnut.cashu.space",
        ] {
            assert!(Asset::new(u).is_err());
        }
    }
    #[test]
    fn nostr_mint_canonical_form() {
        let npub = "npub10xlxvlhemja6c4dqv22uapctqupfhlxm9h8z3k2e72q4k9hcz7vqpkge6d";
        let canonical = format!("nostr://{npub}");
        let a = Asset::new(&canonical).unwrap();
        a.fence().unwrap();
        assert_eq!(a.mint_url, canonical);
        let hex = PublicKey::from_bech32(npub).unwrap().to_hex();
        // CLI input canonicalizes hex, uppercase and a trailing slash.
        for input in [
            format!("nostr://{hex}"),
            format!("NOSTR://{}", npub.to_uppercase()),
            format!("nostr://{}", hex.to_uppercase()),
            format!("{canonical}/"),
        ] {
            assert_eq!(Asset::from_cli(&input).unwrap(), a, "{input}");
        }
        // Lots and peers are strict: only the canonical spelling validates.
        for bad in [
            format!("nostr://{hex}"),
            format!("NOSTR://{npub}"),
            format!("nostr://{}", npub.to_uppercase()),
            format!("{canonical}/"),
            format!("{canonical}/path"),
            format!("{canonical}?relay=wss://x"),
            format!("{canonical}#f"),
            "nostr://npub1invalid".to_string(),
        ] {
            assert!(Asset::new(&bad).is_err(), "SAFETY: {bad} must be refused");
            let forged = Asset {
                mint_url: bad.clone(),
                unit: "sat".into(),
            };
            assert!(forged.validate().is_err(), "SAFETY: lot asset {bad}");
        }
        for bad in [
            format!("{canonical}/path"),
            format!("{canonical}?x=1"),
            "nostr://".into(),
        ] {
            assert!(Asset::from_cli(&bad).is_err(), "{bad}");
        }
    }
    #[test]
    fn nonloopback_http_denied() {
        assert!(Asset::new("http://mint.example").unwrap().fence().is_err());
    }
    #[test]
    fn same_unit_different_fee() {
        assert_eq!(gross(64, 0).unwrap(), (64, 0));
        assert_eq!(gross(64, 100).unwrap(), (65, 1));
    }
    #[test]
    fn fee_rounding() {
        assert_eq!(fee([100, 100]).unwrap(), 1);
        assert_eq!(fee([500, 500]).unwrap(), 1);
        assert_eq!(fee([501, 500]).unwrap(), 2);
    }
    #[test]
    fn fee_overflow() {
        assert!(fee([u64::MAX, 1]).is_err());
    }
    #[test]
    fn zero_gross_rejected() {
        assert!(gross(0, 0).is_err());
    }
    #[test]
    fn production_deadlines() {
        assert_eq!(
            (
                LONG_SECONDS,
                SHORT_SECONDS,
                QUOTE_SECONDS,
                CLAIM_CUTOFF_SECONDS
            ),
            (3600, 900, 60, 180)
        );
    }
}

pub fn lot_event(keys: &Keys, give: Leg, want: Leg) -> Result<Event> {
    let now = Timestamp::now();
    let l = Lot {
        trade_v: 1,
        give,
        want,
        maker_trade_pubkey: keys.public_key().to_hex(),
        expires_at: now.as_secs() + 86400,
        deadline_policy: DeadlinePolicy::default(),
        fee_policy: "sender-funds-net-v1".into(),
    };
    let pairs = [
        ("t", "maxplayer".to_string()),
        ("v", "1".to_string()),
        ("g", l.give.asset.mint_url.clone()),
        ("w", l.want.asset.mint_url.clone()),
        ("u", l.give.asset.unit.clone()),
        ("x", l.want.asset.unit.clone()),
        ("expiration", l.expires_at.to_string()),
    ];
    let tags = pairs
        .into_iter()
        .map(|(a, b)| Tag::parse([a.to_string(), b]))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let event = EventBuilder::new(Kind::Custom(LOT), serde_json::to_string(&l)?)
        .custom_created_at(now)
        .tags(tags)
        .sign_with_keys(keys)?;
    parse_lot(&event, now.as_secs())?;
    Ok(event)
}
pub fn status_event(
    keys: &Keys,
    lot: EventId,
    seq: u64,
    prev: EventId,
    status: Status,
) -> Result<Event> {
    let r = Revision {
        trade_v: 1,
        lot_id: lot.to_hex(),
        seq,
        prev: prev.to_hex(),
        status,
    };
    Ok(
        EventBuilder::new(Kind::Custom(STATUS), serde_json::to_string(&r)?)
            .tags([
                Tag::hashtag("maxplayer"),
                Tag::parse(["v", "1"])?,
                Tag::event(lot),
            ])
            .sign_with_keys(keys)?,
    )
}
#[cfg(test)]
mod listing_tests {
    use super::*;
    fn fixture() -> (Keys, Event, Event) {
        let k = Keys::generate();
        let l = lot_event(
            &k,
            Leg {
                asset: Asset::new("http://127.0.0.1:1").unwrap(),
                net: 64,
            },
            Leg {
                asset: Asset::new("http://127.0.0.1:2").unwrap(),
                net: 32,
            },
        )
        .unwrap();
        let s = status_event(&k, l.id, 1, l.id, Status::Available).unwrap();
        (k, l, s)
    }
    #[test]
    fn signed_listing_roundtrip() {
        let (_, l, s) = fixture();
        assert!(parse_lot(&l, Timestamp::now().as_secs()).is_ok());
        assert_eq!(lifecycle(&l, &[s]).unwrap(), Status::Available);
    }
    #[test]
    fn expired_listing_rejected() {
        let (_, l, _) = fixture();
        assert!(parse_lot(&l, l.created_at.as_secs() + 86400).is_err());
    }
    #[test]
    fn tampered_listing_rejected() {
        let (_, mut l, _) = fixture();
        l.content.push(' ');
        assert!(parse_lot(&l, Timestamp::now().as_secs()).is_err());
    }
    #[test]
    fn missing_initial_status_rejected() {
        let (_, l, _) = fixture();
        assert!(lifecycle(&l, &[]).is_err());
    }
    #[test]
    fn cancellation_terminal() {
        let (k, l, s) = fixture();
        let end = status_event(&k, l.id, 2, s.id, Status::Cancelled).unwrap();
        assert_eq!(lifecycle(&l, &[end, s]).unwrap(), Status::Cancelled);
    }
    #[test]
    fn sold_terminal() {
        let (k, l, s) = fixture();
        let end = status_event(&k, l.id, 2, s.id, Status::Sold).unwrap();
        assert_eq!(lifecycle(&l, &[end, s]).unwrap(), Status::Sold);
    }
    #[test]
    fn terminal_cannot_reopen() {
        let (k, l, s) = fixture();
        let end = status_event(&k, l.id, 2, s.id, Status::Sold).unwrap();
        let reopen = status_event(&k, l.id, 3, end.id, Status::Available).unwrap();
        assert!(lifecycle(&l, &[s, end, reopen]).is_err());
    }
    #[test]
    fn forks_quarantined() {
        let (k, l, s) = fixture();
        let a = status_event(&k, l.id, 2, s.id, Status::Sold).unwrap();
        let b = status_event(&k, l.id, 2, s.id, Status::Cancelled).unwrap();
        assert!(lifecycle(&l, &[s, a, b]).is_err());
    }
    #[test]
    fn history_gap_quarantined() {
        let (k, l, s) = fixture();
        let a = status_event(&k, l.id, 3, s.id, Status::Sold).unwrap();
        assert!(lifecycle(&l, &[s, a]).is_err());
    }
    #[test]
    fn foreign_status_rejected() {
        let (_, l, s) = fixture();
        let k = Keys::generate();
        let a = status_event(&k, l.id, 2, s.id, Status::Sold).unwrap();
        assert!(lifecycle(&l, &[s, a]).is_err());
    }
    #[test]
    fn duplicate_event_idempotent() {
        let (_, l, s) = fixture();
        assert_eq!(lifecycle(&l, &[s.clone(), s]).unwrap(), Status::Available);
    }
}

pub mod coordinator;
pub mod journal;
pub mod market;
pub mod mint;
pub mod wallet;

pub mod money;
pub mod real_money;
pub mod receive;
pub mod transport;
