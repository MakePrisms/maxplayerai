import { npubEncode } from '../trade/nip19.js';
import { FRESH_S } from './hero.js';
export interface Job { id: string; seller: string; start: number; deadline: number; end: number|null; running: boolean; usedMinutes: number; }
// reported: the member's entry in the buyer-signed tally of the shown swarm job (model.mjs parseUpdate):
// tasks done in that swarm job, minutes used today (null when the tally is from an earlier UTC day).
export interface Reported { tasksDone: number; minutesToday: number|null }
export interface Seller { pubkey: string; npub: string; name: string; minutesOffered: number; minutesUsed: number; minutesRemaining: number; state: string; eligible: boolean; decision: string; currentJobStart: number|null; reported?: Reported|null; }
// Untrusted pending joins (no buyer decision yet). Display only; never eligible.
export interface Applicant { pubkey: string; npub: string; minutesOffered: number; since: number; joinedAt: number; note: string; }
export type SwarmJobStatus = 'scheduled'|'in_progress'|'completed'|'cancelled';
export interface SwarmInfo { buyer: string; buyerNpub: string; buyerName: string; id: string; name: string; purpose: string; status: 'open'|'closed'; tasks: 'public'|'private'; address: string }
export interface SwarmJobRow { id: string; title: string; starts: number; ends: number; status: SwarmJobStatus; budgetSats: number; tasks: number }
// One swarm's page (src/swarm/shared/model.mjs swarmSnapshot). jobs: today's tasks in this swarm
// (they drive each member's minutes today); swarmJob: the swarm job the page shows.
export interface Snapshot {
  asOf: number;
  swarm?: SwarmInfo|null;
  swarmJob: { id: string; buyer: string; buyerNpub: string; title: string; goal: string; starts: number; ends: number; status: SwarmJobStatus; result: string }|null;
  swarmJobs?: SwarmJobRow[];
  sellers: Seller[]; jobs: Job[]; updates: string[]; applicants?: Applicant[];
  budget: {totalSats: number; committedSats: number; paidSats: number; estimated: boolean; unknownAmounts: number; note: string}|null;
  relay?: { connected: boolean; authenticated: boolean; authMode?: 'authenticated'|'pending'|'anonymous'; historyComplete: boolean; error: string|null; windowComplete?: boolean; windowSince?: number|null; warnings?: string[]; applicantsIncomplete?: boolean;
    relays?: { role: string; url: string; connected: boolean; historyComplete: boolean; error: string|null }[] };
}
// The status word: "Live" when AUTH is as the reader expects it (authenticated, or the relay's
// posture is anonymous), "Polling" while AUTH is pending. #1243: the member-gated relay refuses the
// reader's throwaway AUTH key and serves it the same open reads as an anonymous connection
// (authMode "anonymous"), so that read is Live too. A status without authMode (older reader, demo)
// keeps the old rule.
export function relayStatusWord(r: Snapshot['relay']): "Live" | "Polling" {
  return r?.authenticated || r?.authMode === "anonymous" ? "Live" : "Polling";
}
// What follows the status word on the relay status line: the relay's own reason (e.g. a CLOSED)
// at once, and the reader's warnings.
function relayDetail(r: Snapshot['relay']): string {
  const warnings=shownWarnings(r);
  return (r?.error ? ` · ${r.error}` : "") + (warnings.length ? ` · ${warnings.join("; ")}` : "");
}
// #1243: the member-gated relay refuses the reader's throwaway AUTH key and serves it the same open
// reads (authMode 'anonymous', status word "Live"). That refusal is the expected posture, not a
// problem, so the page leaves the reader's "AUTH refused (…); polling" warning out. Display only:
// status.warnings keeps it for the runner and CLI, and any other AUTH posture (pending,
// authenticated, or a status without authMode) still shows it. Every other warning stays.
const AUTH_REFUSED="AUTH refused (";
function shownWarnings(r: Snapshot['relay']): string[] {
  const warnings=r?.warnings??[];
  return r?.authMode==="anonymous" ? warnings.filter(w=>!w.startsWith(AUTH_REFUSED)) : warnings;
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
  if(s.swarm){const w=s.swarm;if(![w.buyer,w.id,w.name,w.purpose,w.address,w.buyerName].every(v=>typeof v==='string')||!/^[0-9a-f]{64}$/.test(w.buyer)||npubEncode(w.buyer)!==w.buyerNpub)throw Error('Invalid swarm');}
  else if(s.swarm!==undefined&&s.swarm!==null)throw Error('Invalid swarm');
  if(s.swarmJobs!==undefined&&(!Array.isArray(s.swarmJobs)||!s.swarmJobs.every(j=>[j.id,j.title,j.status].every(v=>typeof v==='string')&&[j.starts,j.ends,j.budgetSats,j.tasks].every(finite))))throw Error('Invalid swarm jobs');
  if(s.swarmJob) {
    const c=s.swarmJob;
    if(![c.id,c.title,c.goal,c.result,c.buyer,c.buyerNpub].every(v=>typeof v==='string')||!/^[0-9a-f]{64}$/.test(c.buyer)||npubEncode(c.buyer)!==c.buyerNpub||![c.starts,c.ends].every(finite)||!['scheduled','in_progress','completed','cancelled'].includes(c.status))throw Error('Invalid swarm job');
    if(!s.budget||![s.budget.totalSats,s.budget.committedSats,s.budget.paidSats,s.budget.unknownAmounts].every(finite)||typeof s.budget.note!=='string')throw Error('Invalid budget');
  } else if(s.swarmJob!==null)throw Error('Missing swarm job');
  for(const a of s.sellers)if(![a.pubkey,a.npub,a.name,a.state,a.decision].every(v=>typeof v==='string')||![a.minutesOffered,a.minutesUsed,a.minutesRemaining].every(finite)||typeof a.eligible!=='boolean'||!validReported(a.reported))throw Error('Invalid seller');
  if(s.applicants!==undefined&&(!Array.isArray(s.applicants)||!s.applicants.every(a=>[a.pubkey,a.npub].every(v=>typeof v==='string')&&finite(a.minutesOffered))))throw Error('Invalid applicant');
  for(const j of s.jobs)if(![j.id,j.seller].every(v=>typeof v==='string')||![j.start,j.deadline,j.usedMinutes].every(finite)||typeof j.running!=='boolean')throw Error('Invalid job');
  return s;
}
function validReported(r: unknown): boolean {
  if(r===undefined||r===null)return true;
  const x=r as Reported;
  return typeof r==='object'&&Number.isSafeInteger(x.tasksDone)&&x.tasksDone>=0&&(x.minutesToday===null||finite(x.minutesToday)&&x.minutesToday>=0);
}
// A private swarm's tasks carry no public swarm link, so the relay holds no minutes or tasks for its
// members: the buyer-signed tally is the only source (docs/specs/swarm.md §7.5) and the rows say so.
// A public swarm's minutes come from the relay's signed task events only; its tally is not shown.
export const privateTasks=(s: Snapshot)=>s.swarm?.tasks==='private';
export function sellerMinutes(s: Snapshot,a: Seller,now: number): number {
  if(privateTasks(s))return a.reported?.minutesToday??0;
  // Advance only the running component, capped at each offer deadline. No server/client clock skew jump.
  const elapsed=Math.max(0,now-s.asOf);
  return a.minutesUsed+s.jobs.filter(j=>j.seller===a.pubkey&&j.running).reduce((sum,j)=>sum+Math.max(0,Math.max(0,Math.min(s.asOf+elapsed,j.deadline)-j.start)/60-j.usedMinutes),0);
}
export function sellerLine(s: Snapshot,a: Seller,now: number): string {
  if(privateTasks(s)){const m=a.reported?.minutesToday;return `${m===null||m===undefined?'–':Math.floor(m)} / ${a.minutesOffered} min`;}
  return `${Math.floor(sellerMinutes(s,a,now))} / ${a.minutesOffered} min`;
}
// The private-swarm line under a member's row: the tally's task count, labelled as the buyer's report.
// tasks_done is per swarm job (the one shown), minutes per swarm per UTC day, as on public swarms.
export function reportedLine(s: Snapshot,a: Seller): string|null {
  if(!privateTasks(s))return null;
  const r=a.reported;
  if(!r)return 'No report from the buyer yet';
  return `${r.tasksDone} ${r.tasksDone===1?'task':'tasks'} in this job · reported by the buyer`;
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
// run's start (windowSince, see src/swarm/shared/relay.mjs chooseWindow). A window that doesn't cover the
// run (a run chosen after the window was) waits for the whole history and is not latched.
//
// In between (state 'roster'): the swarm data is ready (the hero's gate, held through a poll
// the same way) but the trade data isn't. The rows show names, offered minutes and whether a member
// left (all from the swarm reader), with placeholders for the job state and minutes used, the
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
// The swarm events (the swarm job's status and window) come from the swarm reader, so the hero
// needs only it: connected, history complete, no error. Trade auth and trade history don't gate
// it. Without per-reader status (demo), the aggregate stands in.
const swarmReaders=(r: NonNullable<Relay>)=>r.relays?.filter(x=>x.role==='swarm');
export function swarmReady(r: Relay): boolean {
  if(!r)return false;
  const sw=swarmReaders(r);
  if(!sw)return r.connected&&r.historyComplete&&!r.error;
  return sw.length>0&&sw.every(x=>x.connected&&x.historyComplete&&!x.error);
}
// Down: the swarm reader disconnected or failed.
function swarmDown(r: Relay): boolean {
  if(!r)return true;
  const sw=swarmReaders(r);
  if(!sw)return !r.connected||!!r.error;
  return !sw.length||sw.some(x=>!x.connected||!!x.error);
}
// Called on every read of the relay status (each receive and each 1s tick). readyAt is the
// last time the swarm data was ready; the swarm reader dropping or erroring clears it at
// once. readyAgeS lets the hero drop "Running now" when the data stops being ready, without
// the 15s poll (which clears historyComplete for about a second) making it flicker.
// The rows' in-between state ('roster') uses the same readiness, and the swarm reader going down
// (swarmDown) makes the state 'updating' even while the trade latch holds, so the rows blank exactly
// when the hero does. run: the snapshot's swarm job (see tradeReady).
export interface Tracker { wasLive: boolean; readyAt: number|null }
export interface Observed extends Tracker { state: LiveState; readyAgeS: number|null }
export function observe(t: Tracker,r: Relay,nowMs: number,run?: Run): Observed {
  const wasLive=!!r&&r.connected&&(t.wasLive||isLive(r,run));
  const readyAt=swarmDown(r)?null:swarmReady(r)?nowMs:t.readyAt;
  const readyAgeS=readyAt===null?null:Math.max(0,nowMs-readyAt)/1000;
  const state=swarmDown(r)?'updating':liveState(r,wasLive,run)==='live'?'live':readyAgeS!==null&&readyAgeS<=FRESH_S?'roster':'updating';
  return {wasLive,readyAt,state,readyAgeS};
}
// The section eyebrow says "Live" only while the section is live; it keeps its space otherwise.
export function renderLiveState(doc: Document,state: LiveState): void {
  doc.getElementById('agents-eyebrow')?.classList.toggle('is-off',state!=='live');   // /swarm no longer has it (Bob, 2026-11-21)
}
export function connectionLine(r: Relay,status: string,wasLive=false,run?: Run): string {
  if(liveState(r,wasLive,run)!=='live')return UPDATING;
  // Same detail as connectionText (error, unavailable relays, degraded and other warnings, minus an
  // expected AUTH refusal: see shownWarnings); the word stays
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
// A private swarm has no public tasks, so there a leaver is listed when the buyer's tally reports work.
const worked=(s: Snapshot,a: Seller)=>s.jobs.some(j=>j.seller===a.pubkey)||privateTasks(s)&&!!a.reported&&(a.reported.tasksDone>0||(a.reported.minutesToday??0)>0);
export function listedSellers(s: Snapshot,roster=false): Seller[] {
  return s.sellers.filter(a=>a.decision!=='denied'&&(a.state!=='left'||!roster&&worked(s,a)));
}
// Public page: denied members are not listed. Remote text only ever reaches textContent.
// mode 'loading' (or true: not live and no fresh swarm data, e.g. before the first data or
// disconnected): no rows and no counts, so nothing reads as current; only "Updating…" shows.
// 'roster': swarm data only (see ROSTER_ONLY): rows with placeholder job states. 'live' (or false): everything.
export type SellersMode='loading'|'roster'|'live';
// A member's name: a link to its market profile (Bob, 2026-11-21). A new tab goes to /market; a click opens the profile
// in the swarm's own panel (swarm.ts). No npub line under it: the profile has the npub to copy.
function profileLink(doc: Document,pubkey: string|undefined,npub: string,label: string): HTMLElement {
  const link=doc.createElement(npub?'a':'strong');link.className='c-who';link.textContent=label;
  if(npub){link.setAttribute?.('href',`/market?expert=${npub}`);if(pubkey)link.setAttribute?.('data-pk',pubkey);}
  return link;
}
const skel=(doc: Document,cls: string)=>{const span=doc.createElement('span');span.className=`skel ${cls}`;return span;};
// Placeholder rows are rebuilt only when what they show changes, so the 1s tick doesn't restart their sheen.
const rosterShown=new WeakMap<object,string>();
export function renderSellers(doc: Document,s: Snapshot,now: number,loading: boolean|SellersMode=false): void {
  const mode: SellersMode=loading===true?'loading':loading===false?'live':loading;
  const list=doc.getElementById('swarm-sellers')!;
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
    const name=profileLink(doc,a.pubkey,a.npub,a.name||shortNpub(a.npub));
    const mins=doc.createElement('span');mins.className='c-mins';
    if(roster){const of=doc.createElement('span');of.textContent=` / ${a.minutesOffered} min`;mins.append(skel(doc,'c-skel-used'),of);}
    else mins.textContent=sellerLine(s,a,now);
    const st=doc.createElement('span');st.className='c-state';
    if(state===null)st.append(skel(doc,'c-skel-state'));else st.textContent=state;
    row.append(name,mins,st);
    const reported=roster?null:reportedLine(s,a);
    if(reported!==null){const r=doc.createElement('span');r.className='c-reported';r.textContent=reported;row.append(r);}
    fragment.append(row);
  }
  // Waiting for approval: an untrusted, capped sample. Never counted as agents in the swarm.
  const waiting=s.applicants??[],partial=!!s.relay?.applicantsIncomplete;
  for(const a of waiting) {
    const row=doc.createElement('li');row.className='off';
    const name=profileLink(doc,a.pubkey,a.npub,shortNpub(a.npub));
    const mins=doc.createElement('span');mins.className='c-mins';mins.textContent=`${a.minutesOffered} min offered`;
    const st=doc.createElement('span');st.className='c-state';st.textContent='Waiting for approval';
    row.append(name,mins,st);fragment.append(row);
  }
  list.replaceChildren(fragment);
  // A private swarm's running tasks aren't public: no "working now" count there.
  const working=roster||privateTasks(s)?'':` · ${shown.filter(a=>sellerState(s,a,now)==='Working now').length} working now`;
  const pending=waiting.length||partial?` · ${waiting.length}${partial?'+':''} waiting for approval`:'';
  doc.getElementById('seller-summary')!.textContent=(shown.length?`${shown.length} ${shown.length===1?'agent':'agents'}${working}`:'No members yet.')+pending;
}
