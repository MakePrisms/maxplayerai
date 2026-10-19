import { buyerHex, type Challenge } from './model.js';
export interface Job { id: string; seller: string; start: number; deadline: number; end: number|null; running: boolean; usedMinutes: number; }
export interface Seller { pubkey: string; npub: string; name: string; minutesOffered: number; minutesUsed: number; minutesRemaining: number; state: string; eligible: boolean; decision: string; currentJobStart: number|null; }
// Untrusted pending joins (no buyer decision yet). Display only; never eligible.
export interface Applicant { pubkey: string; npub: string; minutesOffered: number; since: number; joinedAt: number; note: string; }
export interface Snapshot {
  asOf: number;
  challenge: { id: string; buyer: string; buyerNpub: string; title: string; goal: string; starts: number; ends: number; status: Challenge['status']; result: string }|null;
  sellers: Seller[]; jobs: Job[]; updates: string[]; applicants?: Applicant[];
  budget: {totalSats: number; committedSats: number; paidSats: number; estimated: boolean; unknownAmounts: number; note: string}|null;
  relay?: { connected: boolean; authenticated: boolean; historyComplete: boolean; error: string|null; failedRelays?: string[]; warnings?: string[]; applicantsIncomplete?: boolean;
    relays?: { role: string; url: string; connected: boolean; historyComplete: boolean; error: string|null }[] };
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
// Until the live data is in (connected and history complete), the agents section says only
// "Updating…" (bob: no disclaimers). Latched: once the data has been live (wasLive), a routine
// poll that only clears historyComplete while the relay stays connected keeps "live", so the
// section doesn't blank every 15s. A disconnect (or no relay status at all) clears the latch:
// the section shows "Updating…" and, after the reconnect, waits for a fresh historyComplete
// before "Live", the eyebrow and the rows come back.
export const UPDATING='Updating…';
type Relay=Snapshot['relay'];
export type LiveState='live'|'updating';
export function isLive(r: Relay): boolean { return !!r&&r.connected&&r.historyComplete; }
export function liveState(r: Relay,wasLive: boolean): LiveState { return !!r&&r.connected&&(wasLive||r.historyComplete)?'live':'updating'; }
// The challenge event (status, window) comes only from the challenge relays, so the hero needs
// only them: every challenge relay connected, history complete, no error. Trade auth and trade
// history (which finish ~8-12s later on the real relays) don't gate it. Without per-relay status
// (demo), the aggregate stands in.
const challengeRelays=(r: NonNullable<Relay>)=>r.relays?.filter(x=>x.role==='challenge');
export function challengeReady(r: Relay): boolean {
  if(!r)return false;
  const ch=challengeRelays(r);
  return ch?ch.length>0&&ch.every(x=>x.connected&&x.historyComplete&&!x.error):r.connected&&r.historyComplete&&!r.error;
}
function challengeDown(r: Relay): boolean {
  if(!r)return true;
  const ch=challengeRelays(r);
  return ch?ch.some(x=>!x.connected||!!x.error):!r.connected||!!r.error;
}
// Called on every read of the relay status (each receive and each 1s tick). readyAt is the
// last time the challenge data was ready; a challenge relay dropping or erroring clears it at
// once. readyAgeS lets the hero drop "Running now" when the data stops being ready, without
// the 15s poll (which clears historyComplete for about a second) making it flicker.
export interface Tracker { wasLive: boolean; readyAt: number|null }
export interface Observed extends Tracker { state: LiveState; readyAgeS: number|null }
export function observe(t: Tracker,r: Relay,nowMs: number): Observed {
  const wasLive=!!r&&r.connected&&(t.wasLive||isLive(r));
  const readyAt=challengeDown(r)?null:challengeReady(r)?nowMs:t.readyAt;
  return {wasLive,readyAt,state:liveState(r,wasLive),readyAgeS:readyAt===null?null:Math.max(0,nowMs-readyAt)/1000};
}
// The section eyebrow says "Live" only while the section is live; it keeps its space otherwise.
export function renderLiveState(doc: Document,state: LiveState): void {
  doc.getElementById('agents-eyebrow')!.classList.toggle('is-off',state!=='live');
}
export function connectionLine(r: Relay,status: string,wasLive=false): string {
  if(liveState(r,wasLive)!=='live')return UPDATING;
  // The relay's own reason (e.g. a CLOSED) is shown at once, not only after the reconnect copies it into a warning.
  return status+(r?.error?` · ${r.error}`:'')+(r?.failedRelays?.length?` · Unavailable: ${r.failedRelays.join(', ')}`:'')+(r?.warnings?.length?` · ${r.warnings.join('; ')}`:'');
}
const shortNpub=(npub: string)=>npub.length>20?`${npub.slice(0,12)}…${npub.slice(-6)}`:npub;
// Public page: denied members are not listed. Remote text only ever reaches textContent.
// loading=true (not live: before the first live data, or disconnected): no rows and no counts,
// so nothing reads as current; only "Updating…" shows.
export function renderSellers(doc: Document,s: Snapshot,now: number,loading=false): void {
  const list=doc.getElementById('challenge-sellers')!;
  if(loading){list.replaceChildren();doc.getElementById('seller-summary')!.textContent='';return;}
  const fragment=doc.createDocumentFragment();
  const shown=s.sellers.filter(a=>a.decision!=='denied');
  for(const a of shown) {
    const row=doc.createElement('li');const state=sellerState(s,a,now);
    if(state==='Done'||state==='Left')row.className='off';
    const name=doc.createElement('strong');name.textContent=a.name||shortNpub(a.npub);
    const mins=doc.createElement('span');mins.className='c-mins';mins.textContent=sellerLine(s,a,now);
    const id=doc.createElement('span');id.className='c-id';id.textContent=shortNpub(a.npub);
    const st=doc.createElement('span');st.className='c-state';st.textContent=state;
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
  const working=shown.filter(a=>sellerState(s,a,now)==='Working now').length;
  const pending=waiting.length||partial?` · ${waiting.length}${partial?'+':''} waiting for approval`:'';
  doc.getElementById('seller-summary')!.textContent=(shown.length?`${shown.length} ${shown.length===1?'agent':'agents'} · ${working} working now`:'No members yet.')+pending;
}
