#!/usr/bin/env node
// Consume the shared relay projection: deliberately do NOT implement a second minute tally.
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
const nonnegative = n => Number.isFinite(n) && n >= 0;
// Our trade relay alone: connected, authenticated, history complete, no error. Required for every
// read the runner trusts and before every challenge publish, whatever the challenge quorum says.
export const tradeReady = snapshot => {
  const r = snapshot.relay, t = r?.relays?.find(x => x.role === 'trade') ?? r;
  return !!(t?.connected && t?.authenticated && t?.historyComplete && !t?.error);
};
// Exactly the relay_not_ready condition: connected, authenticated, complete, no error, our trade
// relay ready, and the challenge relay quorum met (run 2 #13). Quorum met with a relay missing
// (degraded) is ready. A snapshot without quorum fields (older reader) needs every relay.
export const relayReady = snapshot => {
  const r = snapshot.relay;
  if (!(r?.connected && r?.authenticated && r?.historyComplete && !r?.error && tradeReady(snapshot))) return false;
  if (r.relays && r.failedRelays?.includes(r.relays.find(x => x.role === 'trade')?.url)) return false;
  if (r.quorum === undefined) return !r.failedRelays?.length;
  return Number.isSafeInteger(r.quorum?.required) && r.quorum.required >= 1 && Number.isSafeInteger(r.quorum.complete) && r.quorum.complete >= r.quorum.required;
};
// Freshness of a snapshot at `now`: the exact stale_snapshot rule (taken at or before now, at most 5 s old).
export const snapshotFresh = (snapshot, now) => Number.isSafeInteger(snapshot?.asOf) && now >= snapshot.asOf && now - snapshot.asOf <= 5;
// Reasons that only say "this read cannot be trusted yet", never anything about the assignment.
export const TRANSIENT_REASONS = ['relay_not_ready','stale_snapshot'];
export function coordinate(snapshot, { now = snapshot.asOf, durationSeconds = 300, amountSats = 0, budgetState = null, pendingSellers = [] } = {}) {
  if (!Number.isSafeInteger(now) || !Number.isSafeInteger(durationSeconds) || durationSeconds <= 0 || !Number.isSafeInteger(amountSats) || amountSats < 0) throw Error('now, durationSeconds and amountSats must be valid integers');
  const c = snapshot.challenge, b = budgetState ?? snapshot.budget, reasons = [];
  if (!c) reasons.push('no_challenge');
  if (c && c.status !== 'in_progress') reasons.push('not_in_progress');
  if (c && !(now >= c.starts && now <= c.ends)) reasons.push('outside_work_window');
  if (!snapshotFresh(snapshot, now)) reasons.push('stale_snapshot');
  if (!relayReady(snapshot)) reasons.push('relay_not_ready');
  const validBudget = b && [b.totalSats,b.committedSats,b.paidSats].every(n => Number.isSafeInteger(n) && nonnegative(n)) && !snapshot.budget?.unknownAmounts;
  const headroom = validBudget ? b.totalSats - b.committedSats - b.paidSats : null;
  if (!validBudget) reasons.push('unknown_budget');
  if (headroom !== null && amountSats > 0 && amountSats > headroom) reasons.push('budget_exceeded');
  // Public ACCEPT is not settlement. Paid assignment requires a reconciled buyer ledger.
  if (amountSats > 0 && (!budgetState || budgetState.challengeId !== c?.id || budgetState.asOf !== now || budgetState.totalSats !== snapshot.budget?.totalSats)) reasons.push('buyer_budget_required');
  // running/currentJobStart include delivered jobs awaiting buyer ACCEPT.
  const pending = new Set(pendingSellers);
  const sellers = (snapshot.sellers ?? []).filter(s => s.eligible === true && s.decision === 'accepted' && !['left','denied'].includes(s.state) && nonnegative(s.minutesRemaining) && s.minutesRemaining > 0 && !pending.has(s.pubkey) && !(snapshot.jobs ?? []).some(j => j.seller === s.pubkey && j.running) && s.currentJobStart == null).map(s => {
    const deadlineUnix = now + durationSeconds;
    const fits = durationSeconds <= s.minutesRemaining * 60;
    return { seller_pubkey:s.pubkey, remainingMinutes:s.minutesRemaining, deadlineUnix, postingAllowed:reasons.length === 0 && fits, reasons:fits ? [...reasons] : [...reasons,'insufficient_minutes'] };
  }).sort((a,b) => a.seller_pubkey.localeCompare(b.seller_pubkey));
  return { asOf:now, challengeId:c?.id ?? null, budgetHeadroomSats:headroom, budgetSource:budgetState ? 'buyer' : 'public_estimate', amountSats, durationSeconds, postingAllowed:reasons.length === 0 && sellers.some(s => s.postingAllowed), reasons, sellers };
}
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const args = process.argv.slice(2), options = { now: Math.floor(Date.now()/1000) };
    const file = args.shift();
    if (!file) throw Error('Usage: node scripts/challenges/coordinator.mjs SNAPSHOT.json|- [--now UNIX] [--duration-seconds N] [--amount-sats N] [--budget-state FILE] [--pending-sellers FILE]');
    while (args.length) {
      const flag=args.shift(), value=args.shift();
      if (value === undefined) throw Error(`Missing value for ${flag}`);
      if (flag === '--budget-state') options.budgetState=JSON.parse(readFileSync(value,'utf8'));
      else if (flag === '--pending-sellers') options.pendingSellers=JSON.parse(readFileSync(value,'utf8'));
      else { const key={'--now':'now','--duration-seconds':'durationSeconds','--amount-sats':'amountSats'}[flag]; if (!key) throw Error(`Unknown option ${flag}`); options[key]=Number(value); }
    }
    console.log(JSON.stringify(coordinate(JSON.parse(readFileSync(file === '-' ? 0 : file,'utf8')),options),null,2));
  } catch(e) { console.error(e.message); process.exitCode=1; }
}
