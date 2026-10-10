//! Human-readable `status`: one line per lot, swap, funding, withdrawal and receive.
//!
//! Read-only (see [`observe`]); prints only public identifiers, mints, states and amounts —
//! never keys, preimages, proofs, tokens, quotes' secrets or invoices.
use crate::{
    coordinator::{Listing, Swap, now, terminal},
    lifecycle,
    money::{Funding, Withdrawal},
    observe,
    receive::{Receipt, ReceiveState},
};
use anyhow::Result;
use std::path::Path;

fn lot_terms(e: &crate::Event) -> Option<crate::Lot> {
    serde_json::from_str(&e.content).ok()
}
fn lot_status(home: &Path, lot: &str) -> Option<String> {
    observe::journal_rows(home, "listing")
        .ok()?
        .into_iter()
        .find(|(k, _, _)| k == lot)
        .and_then(|(_, v, _)| serde_json::from_slice::<Listing>(&v).ok())
        .and_then(|l| lifecycle(&l.event, &l.statuses).ok())
        .map(|s| format!("{s:?}").to_lowercase())
}

fn swap_line(home: &Path, s: &Swap) -> String {
    let lot = s.lot.id.to_hex();
    let mut line = match (s.role.as_str(), lot_status(home, &lot)) {
        ("maker", Some(status)) => format!("lot {lot} {status}; swap {} {} (maker)", s.id, s.state),
        (role, _) => format!("swap {} {} ({role}) on lot {lot}", s.id, s.state),
    };
    let Some(l) = lot_terms(&s.lot) else {
        return line;
    };
    // Incoming net is fixed by the lot (the sender funds our claim fee); outgoing is our debit.
    let (incoming, outgoing) = if s.role == "taker" {
        (&l.give, &l.want)
    } else {
        (&l.want, &l.give)
    };
    let fees = s.plan.debit.saturating_sub(s.plan.net);
    match s.state.as_str() {
        "complete" | "complete_unclaimed" => line.push_str(&format!(
            "; +{} {} / -{} {}; fees {fees}",
            incoming.net, incoming.asset.mint_url, s.plan.debit, outgoing.asset.mint_url
        )),
        "refunded" => line.push_str(&format!(
            "; own lock of {} {} refunded (refund mint fees not itemized)",
            s.plan.debit, outgoing.asset.mint_url
        )),
        "expired" => line.push_str("; nothing locked; reservation released"),
        _ => line.push_str(&format!(
            "; expects +{} {} / -{} {}; fees {fees}",
            incoming.net, incoming.asset.mint_url, s.plan.debit, outgoing.asset.mint_url
        )),
    }
    if s.state.ends_with("_quarantined") {
        line.push_str("; MANUAL RECOVERY: quarantined outputs retained");
    }
    line
}

pub fn render(home: &Path) -> Result<String> {
    let now = now();
    let mut unresolved = vec![];
    let mut settled = vec![];
    let mut manual = 0usize;
    let mut swap_lots = std::collections::BTreeSet::new();
    for (_, v, _) in observe::journal_rows(home, "swap")? {
        let Ok(s) = serde_json::from_slice::<Swap>(&v) else {
            unresolved.push("swap record unreadable by this version".to_string());
            continue;
        };
        if s.role == "maker" {
            swap_lots.insert(s.lot.id.to_hex());
        }
        manual += usize::from(s.state.ends_with("_quarantined"));
        let line = swap_line(home, &s);
        if terminal(&s) && !s.state.ends_with("_quarantined") {
            settled.push(line)
        } else {
            unresolved.push(line)
        }
    }
    for (id, v, _) in observe::journal_rows(home, "listing")? {
        if swap_lots.contains(&id) {
            continue;
        }
        let Ok(l) = serde_json::from_slice::<Listing>(&v) else {
            continue;
        };
        let status = lifecycle(&l.event, &l.statuses)
            .map(|s| format!("{s:?}").to_lowercase())
            .unwrap_or_else(|_| "invalid".into());
        let mut line = format!("lot {id} {status}");
        if l.cancelled && status == "available" {
            line.push_str(" (cancel pending publication)");
        }
        if let Some(t) = lot_terms(&l.event) {
            line.push_str(&format!(
                "; gives {} {} for {} {}",
                t.give.net, t.give.asset.mint_url, t.want.net, t.want.asset.mint_url
            ));
        }
        if status == "available" && !l.cancelled {
            line.push_str(&format!(
                "; reserves debit {} {}",
                l.plan.debit, l.plan.mint
            ));
        }
        settled.push(line);
    }
    for (_, v, _) in observe::journal_rows(home, "funding")? {
        let Ok(f) = serde_json::from_slice::<Funding>(&v) else {
            continue;
        };
        let state = if f.done {
            "done"
        } else if f.expired_unpaid {
            "expired_unpaid"
        } else {
            "pending (invoice not yet paid or not yet issued)"
        };
        let line = format!("funding {} {} {} sats: {state}", f.id, f.mint, f.amount);
        if f.done || f.expired_unpaid {
            settled.push(line)
        } else {
            unresolved.push(line)
        }
    }
    for (_, v, _) in observe::journal_rows(home, "withdrawal")? {
        let Ok(a) = serde_json::from_slice::<Withdrawal>(&v) else {
            continue;
        };
        let s = a.summary();
        let n = |k: &str| s[k].as_u64().map_or("?".to_string(), |n| n.to_string());
        let line = format!(
            "withdrawal {} {} {}: invoice {} sats; mint fee {}; lightning fee {}; change {}",
            a.id,
            a.mint,
            s["state"].as_str().unwrap_or("?"),
            n("amount"),
            n("mint_fee"),
            n("lightning_fee"),
            n("change")
        );
        if a.terminal() {
            settled.push(line)
        } else {
            unresolved.push(line)
        }
    }
    for (_, v, _) in observe::journal_rows(home, "receive")? {
        let Ok(r) = serde_json::from_slice::<Receipt>(&v) else {
            continue;
        };
        let quarantined = r.state == ReceiveState::Quarantined;
        manual += usize::from(quarantined);
        let s = r.summary();
        let mut line = format!(
            "receive {} {} {}: token {} sats; fee {}; credited +{}",
            r.id,
            r.mint,
            s["state"].as_str().unwrap_or("?"),
            r.amount,
            r.fee,
            s["credited"]
        );
        if quarantined {
            line.push_str("; MANUAL RECOVERY: quarantined outputs retained");
        }
        if r.terminal() && !quarantined {
            settled.push(line)
        } else {
            unresolved.push(line)
        }
    }
    let warnings = observe::unresponsive(home, now)?;
    let mut out = String::new();
    for w in &warnings {
        out.push_str(&format!(
            "WARNING counterparty_unresponsive: swap {} ({} {}) waiting for {} for {}",
            w.swap_id,
            w.role,
            w.state,
            w.waiting_for,
            observe::span(w.elapsed_seconds)
        ));
        match w.refund_available_at {
            Some(t) if t > now => out.push_str(&format!(
                "; refund available at {} (in {}); keep serve running",
                observe::utc(t),
                observe::span(t - now)
            )),
            Some(t) => out.push_str(&format!(
                "; refund available since {}; keep serve running or run recover",
                observe::utc(t)
            )),
            None => out.push_str("; nothing of ours is locked yet"),
        }
        out.push('\n');
    }
    if unresolved.is_empty() {
        out.push_str("UNRESOLVED: none\n");
    } else {
        out.push_str(&format!("UNRESOLVED ({}):\n", unresolved.len()));
        for l in &unresolved {
            out.push_str(&format!("  {l}\n"));
        }
    }
    for l in &settled {
        out.push_str(&format!("{l}\n"));
    }
    out.push_str(&format!(
        "summary: {} unresolved, {} need manual recovery. Exit codes: recover/take exit 3 while unresolved items remain (keep serve running or rerun recover), 4 when manual recovery is needed, 0 when everything is settled. status itself always exits 0; use status --json for machine-readable records.",
        unresolved.len(),
        manual
    ));
    Ok(out)
}
