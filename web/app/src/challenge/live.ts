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
  relay?: { connected: boolean; authenticated: boolean; historyComplete: boolean; error: string|null; failedRelays?: string[]; missingRelays?: string[]; degraded?: boolean; quorum?: {required: number; complete: number; total: number}; warnings?: string[]; applicantsIncomplete?: boolean };
}
// The page's relay status line. The relay's own reason (e.g. a CLOSED) is shown at once, not only
// after a reconnect copies it. Degraded (run 2 #13: quorum met, a challenge relay missing) stays
// live; the reader's "Degraded: … missing: …" warning on this line says so.
export function connectionText(r: Snapshot['relay'], status: string): string {
  const connection = r && !r.connected ? "Reconnecting · showing last known" : r && !r.historyComplete ? "Incomplete / catching up · totals may be low" : status;
  return connection + (r?.error ? ` · ${r.error}` : "") + (r?.failedRelays?.length ? ` · Unavailable: ${r.failedRelays.join(", ")}` : "") + (r?.warnings?.length ? ` · ${r.warnings.join("; ")}` : "");
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
const shortNpub=(npub: string)=>npub.length>20?`${npub.slice(0,12)}…${npub.slice(-6)}`:npub;
// Public page: denied members are not listed. Remote text only ever reaches textContent.
export function renderSellers(doc: Document,s: Snapshot,now: number): void {
  const list=doc.getElementById('challenge-sellers')!;const fragment=doc.createDocumentFragment();
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
  const pending=waiting.length||partial?` · ${waiting.length}${partial?'+':''} waiting for approval${partial?' (list may be incomplete)':''}`:'';
  doc.getElementById('seller-summary')!.textContent=(shown.length?`${shown.length} ${shown.length===1?'agent':'agents'} · ${working} working now`:'No members yet.')+pending;
}
