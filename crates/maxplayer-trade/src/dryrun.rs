//! Fee previews: `list --dry-run`, `take --dry-run` and the per-lot fees in `discover`.
//!
//! Read-only by construction: published keysets come from the mint's `/v1/keysets`, input
//! selection reads the wallet SQLite file with `SQLITE_OPEN_READ_ONLY`, and the lot comes from a
//! relay query. Nothing is journaled, reserved, locked, created, published or sent to a maker.
//! The arithmetic is the coordinator's own (`gross`, `fee`, `check_lock_gross`) applied in the
//! same order as `mint::plan`, so the preview equals the plan the real command would build
//! against the same wallet and keysets.
use crate::{Asset, Lot, fee, gross, observe};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::Path, time::Duration};

/// One mint's published sat keysets: id -> (active, input_fee_ppk).
#[derive(Clone, Debug, Default)]
pub struct Keysets(pub BTreeMap<String, (bool, u64)>);
impl Keysets {
    /// Active sat keyset with the lowest input fee, as `Wallet::fetch_active_keyset` picks it.
    pub fn active(&self) -> Option<(&str, u64)> {
        self.0
            .iter()
            .filter(|(_, (active, _))| *active)
            .min_by_key(|(_, (_, ppk))| *ppk)
            .map(|(id, (_, ppk))| (id.as_str(), *ppk))
    }
}

pub async fn keysets(mint: &str, timeout: Duration) -> Result<Keysets> {
    Asset::new(mint)?.fence()?;
    let v: serde_json::Value = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .build()?
        .get(format!("{}/v1/keysets", mint.trim_end_matches('/')))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let mut out = BTreeMap::new();
    for k in v["keysets"].as_array().context("no keysets")? {
        if k["unit"] != "sat" {
            continue;
        }
        let (Some(id), Some(active)) = (k["id"].as_str(), k["active"].as_bool()) else {
            continue;
        };
        out.insert(
            id.to_string(),
            (active, k["input_fee_ppk"].as_u64().unwrap_or(0)),
        );
    }
    Ok(Keysets(out))
}

/// Fee-inclusive amounts for one leg, sender-funds-net: the sender locks `gross` so the
/// receiver's claim (input fee `claim_fee`) nets exactly `net`.
pub fn leg(net: u64, ppk: u64) -> serde_json::Value {
    match gross(net, ppk) {
        Ok((g, claim)) => serde_json::json!({"net":net,"ppk":ppk,"gross":g,"claim_fee":claim}),
        Err(e) => serde_json::json!({"net":net,"ppk":ppk,"error":unsolvable(net, ppk, &e)}),
    }
}
fn unsolvable(net: u64, ppk: u64, e: &anyhow::Error) -> String {
    format!(
        "{e}: no gross amount covers exactly {net} net sats plus its own claim fee at {ppk} ppk; choose a different amount"
    )
}

/// What `mint::plan` would select, computed from a read-only wallet snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    pub keyset: String,
    pub ppk: u64,
    pub net: u64,
    pub gross: u64,
    pub claim_fee: u64,
    pub lock_fee: Option<u64>,
    pub debit: Option<u64>,
    pub inputs: usize,
    pub error: Option<String>,
}
impl Preview {
    pub fn json(&self, mint: &str) -> serde_json::Value {
        serde_json::json!({"mint":mint,"keyset":self.keyset,"ppk":self.ppk,"net":self.net,
            "gross":self.gross,"lock_fee":self.lock_fee,"claim_fee":self.claim_fee,
            "debit":self.debit,"inputs":self.inputs,"error":self.error})
    }
}

/// Unreserved plain proofs `(amount, keyset)` in the order `mint::plan` sorts them.
pub fn spendable(home: &Path, mint: &str) -> Result<Vec<(u64, String)>> {
    let asset = hex::encode(Sha256::digest(format!("{mint}|sat")));
    let Some(db) = observe::open(&home.join(format!("{asset}.sqlite")))? else {
        return Ok(vec![]);
    };
    let mut stmt = db.prepare(
        "SELECT amount, keyset_id FROM proof WHERE mint_url=?1 AND unit='sat' AND state='UNSPENT' \
         AND used_by_operation IS NULL AND spending_condition IS NULL ORDER BY rowid",
    )?;
    let mut rows = stmt
        .query_map([mint], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?
        .map(|r| {
            let (a, k) = r?;
            Ok((u64::try_from(a)?, k))
        })
        .collect::<Result<Vec<_>>>()?;
    rows.sort_by_key(|(a, _)| *a);
    Ok(rows)
}

/// `mint::plan` without the wallet: same gross, same greedy input walk, same fee cap.
pub fn plan(proofs: &[(u64, String)], ks: &Keysets, net: u64, max_fee: u64) -> Result<Preview> {
    let (keyset, ppk) = ks.active().context("mint has no active sat keyset")?;
    let (g, claim_fee) = gross(net, ppk).map_err(|e| anyhow::anyhow!(unsolvable(net, ppk, &e)))?;
    let mut p = Preview {
        keyset: keyset.into(),
        ppk,
        net,
        gross: g,
        claim_fee,
        lock_fee: None,
        debit: None,
        inputs: 0,
        error: None,
    };
    if let Err(e) = crate::real_money::check_lock_gross(g) {
        p.error = Some(e.to_string());
        return Ok(p);
    }
    let mut total = 0u64;
    let mut ppks = vec![];
    for (amount, id) in proofs {
        total = total.checked_add(*amount).context("overflow")?;
        let (_, kppk) =
            ks.0.get(id)
                .with_context(|| format!("unknown keyset {id} for a wallet proof"))?;
        ppks.push(*kppk);
        let lock_fee = fee(ppks.clone())?;
        if total >= g + lock_fee {
            p.inputs = ppks.len();
            p.lock_fee = Some(lock_fee);
            p.debit = Some(g + lock_fee);
            if ppks.len() > 128 {
                p.error = Some("too many inputs".into());
            } else if claim_fee + lock_fee > max_fee {
                p.error = Some(format!(
                    "fee cap exceeded: lock fee {lock_fee} + claim fee {claim_fee} > --max-fees {max_fee}"
                ));
            }
            return Ok(p);
        }
    }
    p.inputs = ppks.len();
    p.error = Some("insufficient unreserved balance (overlisting refused)".into());
    Ok(p)
}

/// Non-blocking per-lot fee estimate for `discover`: `"unknown"` when a mint is unreachable.
pub async fn discover_fees(lots: &[Lot]) -> Vec<serde_json::Value> {
    let mut mints: BTreeMap<String, Option<Keysets>> = BTreeMap::new();
    for l in lots {
        mints.insert(l.give.asset.mint_url.clone(), None);
        mints.insert(l.want.asset.mint_url.clone(), None);
    }
    let mut tasks = tokio::task::JoinSet::new();
    for mint in mints.keys().cloned() {
        tasks.spawn(async move {
            let ks = tokio::time::timeout(
                Duration::from_secs(6),
                keysets(&mint, Duration::from_secs(5)),
            )
            .await;
            (mint, ks.ok().and_then(Result::ok))
        });
    }
    while let Some(Ok((mint, ks))) = tasks.join_next().await {
        mints.insert(mint, ks);
    }
    let side = |mint: &str, net: u64| match mints
        .get(mint)
        .and_then(Option::as_ref)
        .and_then(Keysets::active)
    {
        Some((_, ppk)) => leg(net, ppk),
        None => serde_json::json!("unknown"),
    };
    lots.iter()
        .map(|l| {
            let mut want = side(&l.want.asset.mint_url, l.want.net);
            if let Some(o) = want.as_object_mut() {
                if o.contains_key("gross") {
                    o.insert(
                        "taker_lock_fee".into(),
                        "depends on the taker's inputs; use take --dry-run".into(),
                    );
                }
            }
            serde_json::json!({
                // Maker sends `give` (funds the taker's claim fee); taker sends `want`.
                "give": side(&l.give.asset.mint_url, l.give.net),
                "want": want,
            })
        })
        .collect()
}

/// Lot terms from a validated lot event's content.
pub fn lot_of(content: &str) -> Result<Lot> {
    let l: Lot = serde_json::from_str(content)?;
    ensure!(l.trade_v == 1, "unknown protocol version");
    Ok(l)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ks(entries: &[(&str, bool, u64)]) -> Keysets {
        Keysets(
            entries
                .iter()
                .map(|(i, a, p)| (i.to_string(), (*a, *p)))
                .collect(),
        )
    }
    #[test]
    fn active_is_lowest_fee_active() {
        let k = ks(&[("a", true, 500), ("b", true, 100), ("c", false, 0)]);
        assert_eq!(k.active(), Some(("b", 100)));
    }
    #[test]
    fn plan_matches_coordinator_arithmetic() {
        let k = ks(&[("old", false, 1000), ("new", true, 100)]);
        let proofs = vec![
            (8, "old".to_string()),
            (16, "new".into()),
            (32, "new".into()),
        ];
        let p = plan(&proofs, &k, 24, 16).unwrap();
        let (g, c) = gross(24, 100).unwrap();
        assert_eq!((p.gross, p.claim_fee), (g, c));
        // 8+16 = 24 < 25 + fee; 8+16+32 covers it with fee(1000,100,100) = 2.
        assert_eq!((p.inputs, p.lock_fee, p.debit), (3, Some(2), Some(g + 2)));
        assert!(p.error.is_none());
        let capped = plan(&proofs, &k, 24, 2).unwrap();
        assert!(capped.error.unwrap().contains("fee cap exceeded"));
        let poor = plan(&proofs[..1], &k, 24, 16).unwrap();
        assert!(poor.error.unwrap().contains("insufficient"));
    }
    #[test]
    fn unsolvable_amount_is_reported() {
        let k = ks(&[("x", true, 1000)]);
        let e = plan(&[], &k, 2, 16).unwrap_err().to_string();
        assert!(
            e.contains("cannot solve bounded fee-inclusive split"),
            "{e}"
        );
        assert!(
            leg(2, 1000)["error"]
                .as_str()
                .unwrap()
                .contains("choose a different amount")
        );
    }
}
