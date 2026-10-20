import { buyerHex, type Challenge } from './model.js';
import { FRESH_S } from './hero.js';
export interface Job { id: string; seller: string; start: number; deadline: number; end: number|null; running: boolean; usedMinutes: number; }
export interface Seller { pubkey: string; npub: string; name: string; minutesOffered: number; minutesUsed: number; minutesRemaining: number; state: string; eligible: boolean; decision: string; currentJobStart: number|null; }
// Untrusted pending joins (no buyer decision yet). Display only; never eligible.
export interface Applicant { pubkey: string; npub: string; minutesOffered: number; since: number; joinedAt: number; note: string; }
export interface Snapshot {
  asOf: number;
  challenge: { id: string; buyer: string; buyerNpub: string; title: string; goal: string; starts: number; ends: number; status: Challenge['status']; result: string }|null;
  sellers: Seller[]; jobs: Job[]; updates: string[]; applicants?: Applicant[];
  budget: {totalSats: number; committedSats: number; paidSats: number; estimated: boolean; unknownAmounts: number; note: string}|null;
  relay?: { connected: boolean; authenticated: boolean; historyComplete: boolean; error: string|null; windowComplete?: boolean; windowSince?: number|null; failedRelays?: string[]; missingRelays?: string[]; degraded?: boolean; quorum?: {required: number; complete: number; total: number}; warnings?: string[]; applicantsIncomplete?: boolean;
    relays?: { role: string; url: string; connected: boolean; historyComplete: boolean; error: string|null }[] };
}
// What follows the status word on the relay status line: the relay's own reason (e.g. a CLOSED)
// at once, unavailable relays, and the reader's warnings. Degraded (run 2 #13: quorum met, a
// challenge relay missing) stays live; the reader's "Degraded: … missing: …" warning says so.
function relayDetail(r: Snapshot['relay']): string {
  return (r?.error ? ` · ${r.error}` : "") + (r?.failedRelays?.length ? ` · Unavailable: ${r.failedRelays.join(", ")}` : "") + (r?.warnings?.length ? ` · ${r.warnings.join("; ")}` : "");
}
export function connectionText(r: Snapshot['relay'], status: string): string {
  const connection = r && !r.connected ? "Reconnecting · showing last known" : r && !r.historyComplete ? "Incomplete / catching up · totals may be low" : status;
  return connection + relayDetail(r);
}
const finite = (v: unknown): v is number => typeof v==='number' && Number.isFinite(v);
// Fail closed on malformed snapshots; never put remote text into HTML.
export function parseSnapshot(value: unknown): Snapshot {
  if(!value||typeof value!=='object')throw Error('Invalid snapshot');
  const s=value as Snapshot;
  if(!finite(s.asOf)||!Array.isArray(s.sellers)||!Array.isArray(s.jobs)||!Array.isArray(s.updates)||!s.updates.every(v=>typeof v==='string'))throw Error('Invalid snapshot');
  if(s.challenge) {
    const c=s.challenge;
    if(![c.id,c.title,c.goal,c.result,c.buyer,c.buyerNpub].every(v=>typeof v==='string')||buyerHex(c.buyerNpub)!==c.buyer||![c.starts,c.ends].every(finite)||!['scheduled','in_progress','completed','cancelled'].includes(c.status))throw Error('Invalid challenge');
    if(!s.budget||![s.budget.totalSats,s.budget.committedSats,s.budget.paidSats,s.budget.unknownAmounts].every(finite)||typeof s.budget.note!=='string')throw Error('Invalid budget');
  } else if(s.challenge!==null)throw Error('Missing challenge');
  for(const a of s.sellers)if(![a.pubkey,a.npub,a.name,a.state,a.decision].every(v=>typeof v==='string')||![a.minutesOffered,a.minutesUsed,a.minutesRemaining].every(finite)||typeof a.eligible!=='boolean')throw Error('Invalid seller');
  if(s.applicants!==undefined&&(!Array.isArray(s.applicants)||!s.applicants.every(a=>[a.pubkey,a.npub].every(v=>typeof v==='string')&&finite(a.minutesOffered))))throw Error('Invalid applicant');
  for(const j of s.jobs)if(![j.id,j.seller].every(v=>typeof v==='string')||![j.start,j.deadline,j.usedMinutes].every(finite)||typeof j.running!=='boolean')throw Error('Invalid job');
  return s;
}
export function sellerMinutes(s: Snapshot,a: Seller,now: number): number {
  // Advance only the running component, capped at each offer deadline. No server/client clock skew jump.
  const elapsed=Math.max(0,now-s.asOf);
  return a.minutesUsed+s.jobs.filter(j=>j.seller===a.pubkey&&j.running).reduce((sum,j)=>sum+Math.max(0,Math.max(0,Math.min(s.asOf+elapsed,j.deadline)-j.start)/60-j.usedMinutes),0);
}
export function sellerLine(s: Snapshot,a: Seller,now: number): string {
  return `${Math.floor(sellerMinutes(s,a,now))} / ${a.minutesOffered} min`;
}
const running=(s: Snapshot,a: Seller,now: number)=>s.jobs.some(j=>j.seller===a.pubkey&&j.running&&now<j.deadline);
export function sellerState(s: Snapshot,a: Seller,now: number): string {
  if(a.state==='left')return 'Left';
  if(running(s,a,now))return a.state==='awaiting acceptance'?'Awaiting acceptance':'Working now';
  if(a.eligible&&sellerMinutes(s,a,now)>=a.minutesOffered)return 'Done';
  // Projected state 'done' means the last job finished, not that the seller is out of minutes.
  if(a.state==='done'&&!a.eligible)return 'Done';
  return a.eligible?'Ready':'Joined';
}
// Until the live data is in (connected and the current run's trade data complete), the agents
// section shows no job states and no "working now" count (bob: no disclaimers). Latched: once the
// data has been live (wasLive), a routine poll that only clears historyComplete while the relay
// stays connected keeps "live", so the section doesn't blank every 15s. A disconnect (or no relay
// status at all) clears the latch, and after the reconnect "Live", the eyebrow and the job states
// wait for fresh trade history.
//
// Trade data complete for the run shown (tradeReady): the whole history (historyComplete), or the
// /swarm reader's first trade window (windowComplete) when that window starts at or before the
// run's start (windowSince, see ChallengeReader tradeWindowMarginS). A window that doesn't cover the
// run (a run chosen after the window was) waits for the whole history and is not latched.
//
// In between (state 'roster'): the challenge data is ready (the hero's gate, held through a poll
// the same way) but the trade data isn't. The rows show names, offered minutes and whether a member
// left (all from the challenge relays), with placeholders for the job state and minutes used, the
// summary gives the agent count without "working now", the eyebrow is off and the status line says
// ROSTER_ONLY. Nothing that comes from offers, awards or results is shown, so never "Working now".
export const UPDATING='Updating…';
export const ROSTER_ONLY='Loading what each agent is doing…';
// How far before the run's start the /swarm page's first trade read begins (seconds). Covers
// claims and results whose seller clock runs behind; anything older arrives with the backfill.
export const TRADE_WINDOW_MARGIN_S=3600;
type Relay=Snapshot['relay'];
type Run={starts: number}|null|undefined;
export type LiveState='live'|'roster'|'updating';
export function windowCovers(r: Relay,run?: Run): boolean {
  const since=r?.windowSince;
  return since===undefined||since===null||(!!run&&since<=run.starts);
}
export function tradeReady(r: Relay,run?: Run): boolean { return !!r&&(r.historyComplete||(!!r.windowComplete&&windowCovers(r,run))); }
export function isLive(r: Relay,run?: Run): boolean { return !!r&&r.connected&&tradeReady(r,run); }
export function liveState(r: Relay,wasLive: boolean,run?: Run): 'live'|'updating' { return !!r&&r.connected&&(wasLive&&windowCovers(r,run)||tradeReady(r,run))?'live':'updating'; }
// The challenge event (status, window) comes only from the challenge relays, so the hero needs
// only them: at least the reader's quorum (run 2 #13, default 2 of 3) of challenge relays
// connected, history complete, no error. Degraded (quorum met, one missing) is ready, as it is for
// the reader; below quorum is not. Without a quorum in the status, every challenge relay is
// required. Trade auth and trade history (which finish ~8-12s later on the real relays) don't
// gate it. Without per-relay status (demo), the aggregate stands in.
const challengeRelays=(r: NonNullable<Relay>)=>r.relays?.filter(x=>x.role==='challenge');
const required=(r: NonNullable<Relay>,n: number)=>Math.max(1,Math.min(n,r.quorum?.required??n));
export function challengeReady(r: Relay): boolean {
  if(!r)return false;
  const ch=challengeRelays(r);
  if(!ch)return r.connected&&r.historyComplete&&!r.error;
  return ch.length>0&&ch.filter(x=>x.connected&&x.historyComplete&&!x.error).length>=required(r,ch.length);
}
// Down: fewer challenge relays up (connected, no error) than the quorum needs.
function challengeDown(r: Relay): boolean {
  if(!r)return true;
  const ch=challengeRelays(r);
  if(!ch)return !r.connected||!!r.error;
  return ch.filter(x=>x.connected&&!x.error).length<required(r,ch.length);
}
// Called on every read of the relay status (each receive and each 1s tick). readyAt is the
// last time the challenge data was ready; a challenge relay dropping or erroring clears it at
// once. readyAgeS lets the hero drop "Running now" when the data stops being ready, without
// the 15s poll (which clears historyComplete for about a second) making it flicker.
// The rows' in-between state ('roster') uses the same readiness, and the challenge relays falling
// below quorum (challengeDown) makes the state 'updating' even while the trade latch holds, so a
// challenge relay going down blanks the rows exactly when it blanks the hero (on main a latched
// section kept its rows with one challenge relay still up). run: the snapshot's challenge (see tradeReady).
export interface Tracker { wasLive: boolean; readyAt: number|null }
export interface Observed extends Tracker { state: LiveState; readyAgeS: number|null }
export function observe(t: Tracker,r: Relay,nowMs: number,run?: Run): Observed {
  const wasLive=!!r&&r.connected&&(t.wasLive||isLive(r,run));
  const readyAt=challengeDown(r)?null:challengeReady(r)?nowMs:t.readyAt;
  const readyAgeS=readyAt===null?null:Math.max(0,nowMs-readyAt)/1000;
  const state=challengeDown(r)?'updating':liveState(r,wasLive,run)==='live'?'live':readyAgeS!==null&&readyAgeS<=FRESH_S?'roster':'updating';
  return {wasLive,readyAt,state,readyAgeS};
}
// The section eyebrow says "Live" only while the section is live; it keeps its space otherwise.
export function renderLiveState(doc: Document,state: LiveState): void {
  doc.getElementById('agents-eyebrow')!.classList.toggle('is-off',state!=='live');
}
export function connectionLine(r: Relay,status: string,wasLive=false,run?: Run): string {
  if(liveState(r,wasLive,run)!=='live')return UPDATING;
  // Same detail as connectionText (error, unavailable relays, degraded warning); the word stays
  // "Live" through a routine poll (latched), where connectionText would say catching up.
  return status+relayDetail(r);
}
const shortNpub=(npub: string)=>npub.length>20?`${npub.slice(0,12)}…${npub.slice(-6)}`:npub;
// The agents listed for the run shown (bob, 2026-10-20). Denied members are never listed. A member
// who left is listed only if they had at least one job in this run: s.jobs is exactly the shown
// run's awarded jobs (running, done, expired or never accepted all count), so a past run lists the
// members who left after working in it, and a leaver who only worked in other runs is not listed.
// In 'roster' the jobs aren't known yet, so every leaver is held back until 'live': a row that
// appears once the trade data is in beats one that shows and then vanishes. Display only: the
// shared snapshot (runner, CLI) still carries every left seller.
export function listedSellers(s: Snapshot,roster=false): Seller[] {
  return s.sellers.filter(a=>a.decision!=='denied'&&(a.state!=='left'||!roster&&s.jobs.some(j=>j.seller===a.pubkey)));
}
// Public page: denied members are not listed. Remote text only ever reaches textContent.
// mode 'loading' (or true: not live and no fresh challenge data, e.g. before the first data or
// disconnected): no rows and no counts, so nothing reads as current; only "Updating…" shows.
// 'roster': challenge data only (see ROSTER_ONLY): rows with placeholder job states. 'live' (or false): everything.
export type SellersMode='loading'|'roster'|'live';
const skel=(doc: Document,cls: string)=>{const span=doc.createElement('span');span.className=`skel ${cls}`;return span;};
// Placeholder rows are rebuilt only when what they show changes, so the 1s tick doesn't restart their sheen.
const rosterShown=new WeakMap<object,string>();
export function renderSellers(doc: Document,s: Snapshot,now: number,loading: boolean|SellersMode=false): void {
  const mode: SellersMode=loading===true?'loading':loading===false?'live':loading;
  const list=doc.getElementById('challenge-sellers')!;
  list.setAttribute?.('aria-busy',String(mode!=='live'));
  if(mode==='loading'){rosterShown.delete(list);list.replaceChildren();doc.getElementById('seller-summary')!.textContent='';return;}
  const roster=mode==='roster';
  const shown=listedSellers(s,roster);
  const key=roster?JSON.stringify([shown.map(a=>[a.name,a.npub,a.minutesOffered,a.state]),s.applicants??[],!!s.relay?.applicantsIncomplete]):null;
  if(key!==null&&rosterShown.get(list)===key)return;
  if(key===null)rosterShown.delete(list);else rosterShown.set(list,key);
  const fragment=doc.createDocumentFragment();
  for(const a of shown) {
    // Roster only: every state, and the minutes used, need the trade data and stay placeholders
    // (no leaver is listed yet, see listedSellers).
    const row=doc.createElement('li');const state=roster?null:sellerState(s,a,now);
    if(state==='Done'||state==='Left')row.className='off';
    const name=doc.createElement('strong');name.textContent=a.name||shortNpub(a.npub);
    const mins=doc.createElement('span');mins.className='c-mins';
    if(roster){const of=doc.createElement('span');of.textContent=` / ${a.minutesOffered} min`;mins.append(skel(doc,'c-skel-used'),of);}
    else mins.textContent=sellerLine(s,a,now);
    const id=doc.createElement('span');id.className='c-id';id.textContent=shortNpub(a.npub);
    const st=doc.createElement('span');st.className='c-state';
    if(state===null)st.append(skel(doc,'c-skel-state'));else st.textContent=state;
    row.append(name,mins,id,st);fragment.append(row);
  }
  // Waiting for approval: an untrusted, capped sample. Never counted as agents on the challenge.
  const waiting=s.applicants??[],partial=!!s.relay?.applicantsIncomplete;
  for(const a of waiting) {
    const row=doc.createElement('li');row.className='off';
    const name=doc.createElement('strong');name.textContent=shortNpub(a.npub);
    const mins=doc.createElement('span');mins.className='c-mins';mins.textContent=`${a.minutesOffered} min offered`;
    const id=doc.createElement('span');id.className='c-id';id.textContent=shortNpub(a.npub);
    const st=doc.createElement('span');st.className='c-state';st.textContent='Waiting for approval';
    row.append(name,mins,id,st);fragment.append(row);
  }
  list.replaceChildren(fragment);
  const working=roster?'':` · ${shown.filter(a=>sellerState(s,a,now)==='Working now').length} working now`;
  const pending=waiting.length||partial?` · ${waiting.length}${partial?'+':''} waiting for approval`:'';
  doc.getElementById('seller-summary')!.textContent=(shown.length?`${shown.length} ${shown.length===1?'agent':'agents'}${working}`:'No members yet.')+pending;
}
