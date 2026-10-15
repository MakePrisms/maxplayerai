import { buyerHex, type Challenge } from './model.js';
export interface Job { id: string; seller: string; start: number; deadline: number; end: number|null; running: boolean; usedMinutes: number; }
export interface Seller { pubkey: string; npub: string; name: string; minutesOffered: number; minutesUsed: number; minutesRemaining: number; state: string; eligible: boolean; decision: string; currentJobStart: number|null; }
export interface Snapshot {
  asOf: number;
  challenge: { id: string; buyer: string; buyerNpub: string; title: string; goal: string; starts: number; ends: number; joinOpens: number; joinCloses: number; status: Challenge['status']; result: string }|null;
  sellers: Seller[]; jobs: Job[]; updates: string[];
  budget: {totalSats: number; committedSats: number; paidSats: number; estimated: boolean; unknownAmounts: number; note: string}|null;
  relay?: { connected: boolean; authenticated: boolean; historyComplete: boolean; error: string|null };
}
const finite = (v: unknown): v is number => typeof v==='number' && Number.isFinite(v);
// Fail closed on malformed service responses; never put remote text into HTML.
export function parseSnapshot(value: unknown): Snapshot {
  if(!value||typeof value!=='object')throw Error('Invalid snapshot');
  const s=value as Snapshot;
  if(!finite(s.asOf)||!Array.isArray(s.sellers)||!Array.isArray(s.jobs)||!Array.isArray(s.updates)||!s.updates.every(v=>typeof v==='string'))throw Error('Invalid snapshot');
  if(s.challenge) {
    const c=s.challenge;
    if(![c.id,c.title,c.goal,c.result,c.buyer,c.buyerNpub].every(v=>typeof v==='string')||buyerHex(c.buyerNpub)!==c.buyer||![c.starts,c.ends,c.joinOpens,c.joinCloses].every(finite)||!['scheduled','in_progress','completed','cancelled'].includes(c.status))throw Error('Invalid challenge');
    if(!s.budget||![s.budget.totalSats,s.budget.committedSats,s.budget.paidSats,s.budget.unknownAmounts].every(finite)||typeof s.budget.note!=='string')throw Error('Invalid budget');
  } else if(s.challenge!==null)throw Error('Missing challenge');
  for(const a of s.sellers)if(![a.pubkey,a.npub,a.name,a.state,a.decision].every(v=>typeof v==='string')||![a.minutesOffered,a.minutesUsed,a.minutesRemaining].every(finite)||typeof a.eligible!=='boolean')throw Error('Invalid seller');
  for(const j of s.jobs)if(![j.id,j.seller].every(v=>typeof v==='string')||![j.start,j.deadline,j.usedMinutes].every(finite)||typeof j.running!=='boolean')throw Error('Invalid job');
  return s;
}
export function sellerMinutes(s: Snapshot,a: Seller,now: number): number {
  // Advance only the running component, capped at each offer deadline. No server/client clock skew jump.
  const elapsed=Math.max(0,now-s.asOf);
  return a.minutesUsed+s.jobs.filter(j=>j.seller===a.pubkey&&j.running).reduce((sum,j)=>sum+(Math.max(0,Math.min(s.asOf+elapsed,j.deadline)-j.start)/60-j.usedMinutes),0);
}
export function sellerLine(s: Snapshot,a: Seller,now: number): string {
  const running=s.jobs.filter(j=>j.seller===a.pubkey&&j.running&&now<j.deadline).length;
  return `${Math.floor(sellerMinutes(s,a,now))} of ${a.minutesOffered} min · ${running?`${running} running`:a.state}`;
}
export function renderSellers(doc: Document,s: Snapshot,now: number): void {
  const list=doc.getElementById('challenge-sellers')!;const fragment=doc.createDocumentFragment();
  for(const a of s.sellers) {
    const row=doc.createElement('li');const name=doc.createElement('strong');name.textContent=a.name||a.npub;
    const identity=doc.createElement('span');identity.className='challenge-meta';identity.textContent=a.npub;
    const progress=doc.createElement('span');progress.textContent=sellerLine(s,a,now);
    const state=doc.createElement('span');state.className='challenge-meta';state.textContent=`${a.eligible?'Accepted · eligible for work':a.state} · ${Math.ceil(a.minutesOffered-sellerMinutes(s,a,now))} min remaining`;
    row.append(name,identity,progress,state);fragment.append(row);
  }
  list.replaceChildren(fragment);
  doc.getElementById('seller-summary')!.textContent=s.sellers.length?`${s.sellers.filter(a=>a.eligible).length} accepted · ${s.jobs.filter(j=>j.running&&now<j.deadline).length} running jobs`:'No applicants yet.';
}
