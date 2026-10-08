//! Read-only, secret-free accounting and NUT-07 witness inspection of an existing home.
use anyhow::{Context, Result, ensure};
use cashu::nuts::{State, Witness};
use fs2::FileExt;
use maxplayer_trade::{coordinator::Swap, journal::Journal, mint, money, wallet};
#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let home = std::path::PathBuf::from(args.next().context("usage: real_audit HOME MINT...")?);
    let urls = args.collect::<Vec<_>>();
    ensure!(!urls.is_empty(), "at least one exact mint URL required");
    maxplayer_trade::real_money::configure(urls.clone())?;
    ensure!(
        home.join("wallet.seed").exists() && home.join("trade.sqlite").exists(),
        "existing funded home required"
    );
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(home.join("owner.lock"))?;
    lock.try_lock_exclusive()
        .context("stop the home owner before audit")?;
    let j = Journal::open(&home).await?;
    for url in &urls {
        let w = wallet::wallet(&home, url).await?;
        println!(
            "{}",
            serde_json::json!({"mint":url,"balance":u64::from(w.total_balance().await?)})
        );
    }
    for f in j.all::<money::Funding>("funding").await? {
        println!(
            "{}",
            serde_json::json!({"funding":f.id,"mint":f.mint,"amount":f.amount,"quote":f.quote,"done":f.done})
        );
    }
    for a in j.all::<money::Withdrawal>("withdrawal").await? {
        println!("{}", a.summary());
    }
    for s in j.all::<Swap>("swap").await? {
        if !urls.contains(&s.plan.mint) {
            println!(
                "{}",
                serde_json::json!({"swap":s.id,"mint":s.plan.mint,"audit":"not requested; remote witness unverified"})
            );
            continue;
        }
        let mut witness_shapes = Vec::new();
        let mut claimed = 0;
        let mut refunded = 0;
        let mut missing = 0;
        let mut pending = 0;
        let mut unspent = 0;
        if !s.outgoing.is_empty() {
            for st in mint::states(&s.plan.mint, &s.outgoing).await?.states {
                if let Some(Witness::HTLCWitness(ref w)) = st.witness {
                    witness_shapes.push(serde_json::json!({"type":"HTLCWitness","preimage":if w.preimage.is_empty(){"empty"}else{"redacted"},"signature_count":w.signatures.as_ref().map_or(0,Vec::len),"signatures":"redacted"}));
                }
                match st.state {
                    State::Spent => match st.witness {
                        Some(Witness::HTLCWitness(w))
                            if mint::matches_preimage(&w.preimage, &s.request.hash) =>
                        {
                            claimed += 1
                        }
                        Some(Witness::HTLCWitness(w)) if w.preimage.is_empty() => refunded += 1,
                        _ => missing += 1,
                    },
                    State::Unspent => unspent += 1,
                    _ => pending += 1,
                }
            }
        }
        println!(
            "{}",
            serde_json::json!({"swap":s.id,"state":s.state,"role":s.role,"mint":s.plan.mint,
            "outgoing_proofs":s.outgoing.len(),"claimed_witnesses":claimed,"refund_witnesses":refunded,
            "missing_or_invalid_witnesses":missing,"pending":pending,"unspent":unspent,
            "short":s.quote.as_ref().map(|q| q.short),"long":s.quote.as_ref().map(|q| q.long),
            "margin":s.quote.as_ref().map(|q| q.margin),"preimages":"redacted","witness_shapes":witness_shapes})
        );
        ensure!(
            missing == 0,
            "missing/invalid spent-proof witness; stop live testing"
        );
    }
    Ok(())
}
