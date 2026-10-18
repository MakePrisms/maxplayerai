#!/usr/bin/env node
// Consume the shared relay projection: deliberately do NOT implement a second minute tally.
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
const nonnegative = n => Number.isFinite(n) && n >= 0;
// Exactly the relay_not_ready condition: connected, authenticated, complete, no error/failed relay.
export const relayReady = snapshot => !!(snapshot.relay?.connected && snapshot.relay?.authenticated && snapshot.relay?.historyComplete && !snapshot.relay?.error && !snapshot.relay?.failedRelays?.length);
// Freshness of a snapshot at `now`: the exact stale_snapshot rule (taken at or before now, at most 5 s old).
export const snapshotFresh = (snapshot, now) => Number.isSafeInteger(snapshot?.asOf) && now >= snapshot.asOf && now - snapshot.asOf <= 5;
// Reasons that only say "this read cannot be trusted yet", never anything about the assignment.
export const TRANSIENT_REASONS = ['relay_not_ready','stale_snapshot'];
// R3-2: is the day waiting on a seller that a harness restore ALONE would make a candidate? True
// only when the roster-wide reasons are empty, and some seller's sole reason is seller_not_serving
// and a minimum task (its deadlineUnix) still ends inside the work window. A not-serving seller
// that is also short of minutes, or whose task cannot finish before `ends`, does not hold the day
// open. A separate clause on purpose: it must compose with other exhaustion suppressors (#1172's
// benched sellers), not replace them.
export function waitingOnHarnessProbe(decision, ends) {
  return (decision?.reasons ?? []).length === 0 && (decision?.sellers ?? []).some(s =>
    s.reasons?.length === 1 && s.reasons[0] === 'seller_not_serving' && s.deadlineUnix <= ends);
}
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
    // R3-2: a seller whose newest heartbeat says it can run nothing (status=not_serving or
    // serving=0/N) is not assigned work. Absent/unknown stays eligible, exactly as before.
    const notServing = s.servingState === 'not_serving' || s.serving?.available === 0;
    const own = [...(fits ? [] : ['insufficient_minutes']), ...(notServing ? ['seller_not_serving'] : [])];
    return { seller_pubkey:s.pubkey, remainingMinutes:s.minutesRemaining, deadlineUnix, postingAllowed:reasons.length === 0 && own.length === 0, reasons:[...reasons, ...own] };
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
