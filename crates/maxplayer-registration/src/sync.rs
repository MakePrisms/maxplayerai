//! Member-list sync: the outbox worker that sends NIP-43 commands until the relay
//! confirms them, and the reconcile that compares the relay's member list (13534)
//! with `registrations` (spec §4, §5).

use std::collections::HashSet;

use anyhow::Result;
use chrono::{DateTime, TimeDelta, Utc};
use nostr::{PublicKey, Timestamp};
use tracing::{info, warn};

use crate::AppState;
use crate::db::{self, OutboxItem};
use crate::relay::MemberAction;

fn ts(now: DateTime<Utc>) -> Timestamp {
    Timestamp::from(now.timestamp().max(0) as u64)
}

/// Retry delay after `attempts` failures: 30 s doubling, capped at 1 hour.
pub fn backoff(attempts: i32) -> TimeDelta {
    let secs = 30i64.saturating_mul(1i64 << attempts.clamp(0, 7));
    TimeDelta::seconds(secs.min(3600))
}

/// Send one leased command. `Ok(true)` = the relay confirmed it.
pub async fn process_item(state: &AppState, item: &OutboxItem) -> Result<bool> {
    let now = state.now();
    let Ok(target) = PublicKey::from_hex(&item.pubkey) else {
        db::mark_outbox(&state.db, item.id, "superseded", now).await?;
        return Ok(false);
    };
    // A fresh created_at for every attempt: the relay refuses commands older than ±120 s.
    match state
        .relay
        .send_member_command(item.action, &target, ts(now))
        .await
    {
        Ok(_applied_or_already_done) => {
            db::mark_outbox(&state.db, item.id, "done", now).await?;
            match item.action {
                MemberAction::Add => db::add_managed(&state.db, &item.pubkey, now).await?,
                MemberAction::Remove => db::remove_managed(&state.db, &item.pubkey).await?,
            }
            info!(pubkey = %item.pubkey, action = item.action.as_str(), "relay member command confirmed");
            Ok(true)
        }
        Err(e) => {
            let next = now + backoff(item.attempts);
            db::mark_outbox_failed(&state.db, item.id, &e, next, now).await?;
            warn!(pubkey = %item.pubkey, action = item.action.as_str(), error = %e, "relay member command failed; will retry");
            Ok(false)
        }
    }
}

/// Try one specific queued command right now. `Ok(true)` = confirmed (now or earlier).
pub async fn deliver(state: &AppState, outbox_id: i64) -> Result<bool> {
    let now = state.now();
    match db::claim_one(&state.db, outbox_id, now).await? {
        Some(item) => process_item(state, &item).await,
        None => Ok(db::outbox_status(&state.db, outbox_id).await?.as_deref() == Some("done")),
    }
}

/// Send every due command once. Returns how many the relay confirmed.
pub async fn run_outbox_once(state: &AppState) -> Result<usize> {
    let items = db::claim_due(&state.db, state.now(), 50).await?;
    let mut done = 0;
    for item in &items {
        if process_item(state, item).await? {
            done += 1;
        }
    }
    Ok(done)
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    pub queued_adds: usize,
    pub queued_removes: usize,
}

/// Compare the relay's member list with `registrations` and queue the difference:
/// registered keys missing from the list are added; `member`-role keys that are not
/// registered are removed if this service added them (operator removal by SQL) or,
/// when `sync.reconcile_removals` is on, whoever added them. Admins, owners, the
/// admin key itself and `sync.keep_members` are never removed. `Ok(None)` = no
/// `relay.relay_pubkey` configured, or the relay has no list yet.
pub async fn reconcile_once(state: &AppState) -> Result<Option<ReconcileReport>> {
    let Some(relay_pk) = state.cfg.relay.relay_pubkey.as_deref() else {
        return Ok(None);
    };
    let relay_pk = PublicKey::parse(relay_pk)?;
    let now = state.now();
    let members = match state.relay.fetch_member_list(&relay_pk, ts(now)).await {
        Ok(Some(m)) => m,
        Ok(None) => return Ok(None),
        Err(e) => anyhow::bail!(e),
    };
    // Read registrations AFTER the list so a key linked meanwhile counts as registered.
    let registered: HashSet<String> = db::registered_pubkeys(&state.db)
        .await?
        .into_iter()
        .collect();
    let managed: HashSet<String> = db::managed_pubkeys(&state.db).await?.into_iter().collect();
    let mut keep: HashSet<String> = state
        .cfg
        .sync
        .keep_members
        .iter()
        .filter_map(|k| PublicKey::parse(k).ok().map(|p| p.to_hex()))
        .collect();
    keep.insert(state.relay.admin_pubkey().to_hex());
    keep.insert(relay_pk.to_hex());

    let on_relay: HashSet<&str> = members.iter().map(|(pk, _)| pk.as_str()).collect();
    let mut report = ReconcileReport::default();
    for pk in &registered {
        if !on_relay.contains(pk.as_str()) {
            db::enqueue_pool(&state.db, pk, MemberAction::Add, now).await?;
            report.queued_adds += 1;
        }
    }
    for (pk, role) in &members {
        let removable = role == "member"
            && !registered.contains(pk)
            && !keep.contains(pk)
            && (managed.contains(pk) || state.cfg.sync.reconcile_removals);
        if removable {
            db::enqueue_pool(&state.db, pk, MemberAction::Remove, now).await?;
            report.queued_removes += 1;
        }
    }
    // A managed key that is gone from the relay and unregistered needs nothing more.
    for pk in managed
        .iter()
        .filter(|pk| !on_relay.contains(pk.as_str()) && !registered.contains(*pk))
    {
        db::remove_managed(&state.db, pk).await?;
    }
    if report != ReconcileReport::default() {
        info!(
            adds = report.queued_adds,
            removes = report.queued_removes,
            "reconcile queued member changes"
        );
    }
    Ok(Some(report))
}

/// Operator removal (ban): delete the registration and send the 9031 now (it stays
/// queued for retry if the relay does not confirm).
pub async fn remove_key(state: &AppState, target: &PublicKey) -> Result<(bool, bool)> {
    let (existed, id) = db::remove_registration(&state.db, &target.to_hex(), state.now()).await?;
    let confirmed = deliver(state, id).await?;
    Ok((existed, confirmed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(backoff(0), TimeDelta::seconds(30));
        assert_eq!(backoff(1), TimeDelta::seconds(60));
        assert_eq!(backoff(6), TimeDelta::seconds(1920));
        assert_eq!(backoff(7), TimeDelta::seconds(3600));
        assert_eq!(backoff(100), TimeDelta::seconds(3600));
        assert_eq!(backoff(-1), TimeDelta::seconds(30));
    }
}
