import { CHALLENGE, MEMBERSHIP, DECISION, OFFER, CLAIM, RESULT, AWARD, ACCEPT, HEARTBEAT, PROFILE } from '../../model/kinds.ts';
import { schnorr } from '@noble/curves/secp256k1';
import { sha256 } from '@noble/hashes/sha256';
import { bytesToHex } from '@noble/hashes/utils';
import { signAuth, verify, mentions, validMembership, tag, number } from './events.mjs';
import { acceptedSellers, buyerChallenges, currentChallenge } from './model.mjs';

// Per-REQ page size, applicant sample size, metadata budget and authors chunk size.
export const PAGE_LIMIT=500, APPLICANT_LIMIT=256, METADATA_CAP=256, AUTHOR_CHUNK=100;
const APPLICANTS='applicants',BOUND='bound-';
const CHALLENGE_KINDS=[CHALLENGE,MEMBERSHIP,DECISION];
// Fixed filters. Memberships are deliberately absent: the runner reads them only through
// rosterFilters (authors = buyer-accepted sellers); the untrusted applicantFilter is display-only.
export function challengeFilters(buyer) {
  return [CHALLENGE,DECISION,OFFER,CLAIM,AWARD,RESULT,ACCEPT].map(kind =>
    [CLAIM,RESULT].includes(kind) ? {kinds:[kind],'#p':[buyer]} : {kinds:[kind],authors:[buyer]});
}
export function rosterFilters(buyer,accepted) {
  const sorted=[...accepted].sort(),out=[];
  for(let i=0;i<sorted.length;i+=AUTHOR_CHUNK)out.push({kinds:[MEMBERSHIP],authors:sorted.slice(i,i+AUTHOR_CHUNK),'#p':[buyer]});
  return out;
}
export const applicantFilter=buyer=>({kinds:[MEMBERSHIP],'#p':[buyer]});
export const matches = (e,f) => f.kinds.includes(e.kind) && (!f.authors || f.authors.includes(e.pubkey)) && (!f['#p'] || f['#p'].some(pk=>mentions(e,pk))) &&
  (!f['#e'] || f['#e'].some(id=>e.tags.some(t=>t[0]==='e'&&t[1]===id))) &&
  (f.since===undefined || e.created_at>=f.since) && (f.until===undefined || e.created_at<=f.until);
const metadataId=id=>id.startsWith(`${PROFILE}-`)||id.startsWith(`${HEARTBEAT}-`);
const soft=id=>id===APPLICANTS||metadataId(id);
const RECOVERING='Recovering after relay CLOSED';
// R3-6: a history page (or, in a strict round, a live REQ) with no EOSE within PAGE_TIMEOUT_MS fails
// the read ("Relay history timed out"). After a restart the runner spent minutes CPU-bound verifying
// and projecting history, so frames that had ARRIVED waited in the socket buffer behind that work and
// both public relays timed out after 34 s. The deadline is therefore credited with the time THIS
// process spent handling relay frames since the request was sent (busyMs: every reader's message
// handler, process-wide, since one busy reader delays every other reader's frames), capped at
// PAGE_CPU_CREDIT_MAX_MS. Wall time the process was idle is never credited, so a relay that sends
// nothing still times out at PAGE_TIMEOUT_MS, and while any page is pending history is not complete
// (fail closed); the cap bounds how long a flood of frames can hold a read open.
//
// P-3: frame-handling time is not the only way this thread falls behind. Any other main-thread work
// (the runner's projection tick, a cold signature cache, a large backfill, a throttled browser tab)
// leaves arrived frames unread, and after the block the deadline timer runs BEFORE the socket's
// queued frames (timers precede I/O in each event-loop turn), so a relay whose EOSE was already
// waiting was declared dead: recover() -> reconnect -> full history re-read -> more CPU. So each
// reader also measures its own event-loop lag by timer drift (performance.now, browser and Node; no
// perf_hooks): a queue tick more than lagThresholdMs (LAG_THRESHOLD_MS) later than its interval
// means the thread was blocked, and that lost time (minus frame handling already counted in busyMs)
// is credited to every pending page and live REQ deadline, inside the same PAGE_CPU_CREDIT_MAX_MS cap
// as busyMs. The one-shot deadlines (connect, AUTH answer, AUTH challenge hold) use lagTimeout, which
// re-arms for the time it lost when it fires late instead of failing, within the same cap. An idle
// loop gains no credit, so a silent relay still fails at its plain deadline, and a relay that is
// silent while this thread is blocked over and over still fails within deadline + cap.
export const PAGE_TIMEOUT_MS=20000, PAGE_CPU_CREDIT_MAX_MS=120000, LAG_THRESHOLD_MS=250;
let busy=0;
export const relayBusyMs=()=>busy;
const now=()=>globalThis.performance?.now?.()??Date.now();
// P-3: a setTimeout whose deadline does not expire because of our own lag (see above). When it runs
// more than thresholdMs past its due time, the deadline moves back by the time it lost (so it is
// due now) and it re-checks on a later turn, after the frames that arrived meanwhile were handled,
// instead of calling fn. Moves spend from maxCreditMs; a check whose lost time no longer fits in it
// calls fn, so fn runs no later than ms + maxCreditMs plus the lateness of its last check.
// clear() cancels it like clearTimeout.
export function lagTimeout(fn,ms,{thresholdMs=LAG_THRESHOLD_MS,maxCreditMs=PAGE_CPU_CREDIT_MAX_MS}={}) {
  let credit=0,handle=null,done=false,due=now()+ms;
  const arm=()=>{handle=setTimeout(()=>{
    if(done)return;
    const lost=now()-due;
    if(lost>thresholdMs&&credit+lost<=maxCreditMs){credit+=lost;due+=lost;arm();return;}
    done=true;fn();},Math.max(0,due-now()));};
  arm();
  return {clear(){done=true;clearTimeout(handle);}};
}
// Read-only transport. The only signed frame is NIP-42 AUTH, using an in-memory
// throwaway key (one key per relay connection). Never loads a wallet or publishes EVENT.
//
// Trusted state (historyComplete, error) covers only the fixed filters and the roster
// membership filters. The applicant sample never affects it: a flood there can only
// truncate the "waiting for approval" list (status.applicantsIncomplete).
//
// A relay CLOSED on a trusted read is never a dead end. A CLOSED on an authenticated live
// subscription, a refused trusted history page, an auth-required refusal after AUTH, or AUTH
// that does not complete in authWaitMs all close the socket. The backoff reconnect then runs
// one full fresh round (AUTH, trusted history, live). CLOSED-driven reconnects keep doubling
// the backoff (up to closedMaxRetryMs, 5 min) until a recovered connection has stayed ready
// for stableMs.
//
// Strict rounds: while recovering from a CLOSED, and whenever AUTH is expected (the relay sent
// a challenge that was not refused), historyComplete also needs AUTH OK and an EOSE on a live
// REQ for every trusted filter on this connection, sent after AUTH OK when AUTH is expected.
// A trusted live CLOSED in a strict round recovers, even before AUTH OK. A relay that closes
// each live REQ therefore stays not ready instead of flapping.
//
// Recovery on a relay whose current posture is AUTH (it accepted this reader's AUTH and has not
// since refused it or stopped challenging outside recovery) is ready only with AUTH OK: AUTH
// refused, even with every live REQ still installed, or no AUTH challenge within authWaitMs on a
// connection opened expecting one, recovers again like an unanswered challenge. A relay that never
// challenges, or refuses AUTH, outside recovery keeps the anonymous path, in recovery too.
//
// Poll rounds (R3-4): once a trusted filter's history has been read completely on this connection
// (`read`), a later poll page for it (and that page's pagination) is a re-read of history already
// held, so it no longer drops historyComplete; status.pollInProgress says a trusted poll round is in
// flight instead. Every fail-closed rule still applies to poll pages: a silent page times out and
// closes the socket, a truncated page sets an error, a refused page recovers (and its filter must be
// read again in full), and a filter never read on this connection (initial read, a new roster chunk,
// a re-read after AUTH) keeps historyComplete false until it completes.
//
// Live REQ ids: `live-<id>` for a filter's first live REQ on a connection, `live-<id>~<n>` for
// a replacement, so an EOSE, EVENT or CLOSED of a replaced REQ never counts for the current one.
//
// Window mode (`window:true`; only the /swarm page turns it on, through ChallengeReader's
// tradeWindowMarginS. The runner and CLI never set it and read exactly as before). Trusted history
// is read in two phases, so the current run is complete before the whole history is:
// - Until setWindow(since) is called, no trusted history page or live REQ is sent and nothing is
//   complete. setWindow is called once per reader; reconnects reuse the same window.
// - Window phase: each trusted filter is read from `since` (inclusive), with the usual pagination
//   and every fail-closed rule (a silent page times out and reconnects, a truncated page is an
//   error, a refused page recovers and is read again in full). Live REQs never reach below `since`.
// - Backfill phase: once a filter's window is read, its older history is read with `until: since`.
//   Backfill pages are sent after every other frame (window and live REQs, then profiles, then other
//   display reads) and fail closed in the same way.
// - Bound reads: a claim or result carries its seller's clock, and an award or accept could carry a
//   skewed buyer clock, so one dated before `since` can still belong to an offer in the window. Once
//   the offer window is read, every claim, result, award and accept that references an offer the
//   window holds (`#e`, chunks of AUTHOR_CHUNK offer ids, ids `bound-…`) is read in full, whatever
//   its date, and the window is complete only with those reads. A run's jobs are therefore the same
//   from the window as from the whole history; only older runs wait for the backfill.
// status.windowComplete is historyComplete for the window alone: every trusted filter's window read,
// with all other conditions (connection, no error, AUTH and live EOSE in a strict round) unchanged.
// status.historyComplete still means the WHOLE history, so it waits for the backfill too.
// setWindow(null) reads the whole history in one phase; both flags are then the same.
export class RelayReader {
  constructor({url,buyer,store,onChange=()=>{},WebSocketImpl=WebSocket,pollMs=15000,queueMs=300,overlap=900,
    retryMs=1000,maxRetryMs=30000,closedMaxRetryMs=300000,authWaitMs=15000,stableMs=60000,connectMs=20000,pageTimeoutMs=PAGE_TIMEOUT_MS,pageCreditMaxMs=PAGE_CPU_CREDIT_MAX_MS,lagThresholdMs=LAG_THRESHOLD_MS,filters=challengeFilters(buyer),
    roster=filters.some(f=>f.kinds.includes(DECISION)),metadata=filters.some(f=>f.kinds.includes(OFFER)),applicants=roster,onRoster=null,key=schnorr.utils.randomPrivateKey(),window=false}) {
    Object.assign(this,{url,buyer,store,onChange,WebSocketImpl,pollMs,queueMs,overlap,retryMs,maxRetryMs,closedMaxRetryMs,authWaitMs,stableMs,connectMs,pageTimeoutMs,pageCreditMaxMs,lagThresholdMs,readsRoster:roster,readsMetadata:metadata,applicants});
    // Window mode (see above). windowSince: undefined = not chosen yet, null = whole history, else unix seconds.
    this.windowMode=!!window; this.windowSince=undefined;
    this.onRoster=onRoster??(()=>this.setRoster(acceptedSellers(this.store.events.values(),this.buyer)));
    this.initialFilters=filters; this.extra=new Map(); this.members=new Set(); this.rosterKey=null; this.metaWarnings=[];
    this.key=key; this.closedStreak=0; this.lastClosed=null; this.expectAuth=false; // expectAuth: an earlier connection was challenged
    this.authAccepted=false; // the relay accepted our AUTH and has not since refused it or stopped challenging outside recovery
    this.status=this.freshStatus();
    this.stopped=false; this.retry=retryMs; this.lag=0; // this reader's observed event-loop lag (P-3), ms
  }
  freshStatus(){return {connected:false,authenticated:false,historyComplete:false,windowComplete:false,pollInProgress:false,error:null,
    warnings:[...this.metaWarnings,...(this.closedStreak?[`${RECOVERING} (${this.lastClosed}); reconnect ${this.closedStreak}`]:[])],applicantsIncomplete:false};}
  // Applicant and profile/heartbeat reads are display-only: they never gate readiness.
  trustedPending(){for(const id of this.pages.keys())if(!soft(id))return true;return false;}
  // A pending trusted page that must complete before history counts as complete (R3-4): any page of
  // a filter not yet read completely on this connection. A poll of a read filter does not block.
  blockingPending(){for(const [id,p] of this.pages)if(!soft(id)&&!(p.poll&&this.read.has(id)))return true;return false;}
  // Window mode: the same, except a backfill page (history older than the window) does not block the window.
  windowBlockingPending(){for(const [id,p] of this.pages)if(!soft(id)&&p.phase!=='backfill'&&!(p.poll&&this.read.has(id)))return true;return false;}
  // Window mode, before setWindow: trusted filters send nothing yet.
  deferred(id){return this.windowMode&&this.windowSince===undefined&&!soft(id);}
  // A trusted filter's first history page on a connection: from the window when there is one.
  firstPage(id,filter){
    if(soft(id)||id.startsWith(BOUND)||!this.windowMode||this.windowSince===null)return this.page(id,filter);
    this.page(id,{...filter,since:this.windowSince},{phase:'window'});
  }
  // Window mode only: called once, with the window's lower bound (unix seconds) or null for the
  // whole history. Pages and subscribes the trusted filters that were waiting for it.
  setWindow(since){
    if(!this.windowMode||this.windowSince!==undefined)return;
    if(since!==null&&!(Number.isSafeInteger(since)&&since>=0))throw Error('Invalid history window');
    this.windowSince=since;
    if(this.filters&&this.opened&&!this.dead)for(const [id,f] of this.filters)if(!soft(id)){this.firstPage(id,f);this.subscribe(id);}
    if(this.pages)this.updateComplete();
    this.onChange();
  }
  // Window mode send order (lower first, first in first out within a rank): profile pages (one
  // small page per agent: the names the early rows show); trusted window, bound and live REQs and
  // anything else; other display reads; backfill pages. Without window mode the queue stays first
  // in, first out.
  rank(frame){
    if(frame[0]!=='REQ')return 1;
    const p=this.pages.get(frame[1]);
    if(p?.phase==='backfill')return 3;
    if(p&&frame[1].startsWith(`${PROFILE}-`))return 0;
    const id=p?frame[1]:this.live.get(frame[1])?.id;
    return id!==undefined&&soft(id)?2:1;
  }
  // Window mode: the offer window is read; read everything that references its offers (see above).
  bindWindow(){
    const offers=[...this.store.events.values()].filter(e=>e.kind===OFFER&&e.pubkey===this.buyer&&e.created_at>=this.windowSince)
      .sort((a,b)=>a.created_at-b.created_at||a.id.localeCompare(b.id)).map(e=>e.id),entries=new Map();
    for(let i=0;i<offers.length;i+=AUTHOR_CHUNK){
      const ids=offers.slice(i,i+AUTHOR_CHUNK),key=bytesToHex(sha256(ids.join(','))).slice(0,16);
      entries.set(`${BOUND}s-${key}`,{kinds:[CLAIM,RESULT],'#p':[this.buyer],'#e':ids});
      entries.set(`${BOUND}b-${key}`,{kinds:[AWARD,ACCEPT],authors:[this.buyer],'#e':ids});
    }
    this.replaceExtra(id=>id.startsWith(BOUND),entries);
  }
  next(held){
    if(!this.windowMode)return this.queue.findIndex(f=>!held(f));
    let best=-1,rank=Infinity;
    for(let i=0;i<this.queue.length&&rank>0;i++){const f=this.queue[i];if(held(f))continue;const r=this.rank(f);if(r<rank){best=i;rank=r;}}
    return best;
  }
  softFailure(id){this.pages.delete(id);if(id===APPLICANTS)this.status.applicantsIncomplete=true;else this.metaIncomplete();}
  metaIncomplete(){const w='Profile/heartbeat read incomplete';if(!this.status.warnings.includes(w))this.status.warnings.push(w);}
  authExpected(){return !!this.challenged&&this.authRefused===null;}
  strict(){return this.closedStreak>0||this.authExpected();}
  // Recovering on a relay whose posture is AUTH: only AUTH OK can complete the round. Such a
  // connection opens with expectAuth (authAccepted implies an earlier challenge), so its live REQs
  // are held until a challenge; AUTH refused or no challenge then recovers (see OK and holdTimer).
  needsAuth(){return this.closedStreak>0&&this.authAccepted;}
  // Every trusted filter, whether or not its live REQ is still installed, needs an EOSE on this
  // connection; once AUTH is expected, on a REQ sent after AUTH OK.
  liveConfirmed(){
    if(this.authExpected()&&!this.status.authenticated)return false;
    for(const id of this.filters.keys()){
      if(soft(id))continue;
      const e=this.live.get(this.liveSub.get(id));
      if(!e?.eose||(this.status.authenticated&&!e.authed))return false;
    }
    return true;
  }
  // In a strict round a trusted live REQ left unanswered for 20s fails the round, like a silent history read.
  liveLate(){
    if(!this.strict())return false;
    for(const [id,sub] of this.liveSub){const e=this.live.get(sub);if(!soft(id)&&e?.sentAt&&!e.eose&&this.late(e))return true;}
    return false;
  }
  // R3-6/P-3: past its deadline (PAGE_TIMEOUT_MS plus the capped busy + lag credit, see above).
  late(r){return Date.now()-r.sentAt>this.pageTimeoutMs+Math.min(this.pageCreditMaxMs,Math.max(0,busy-(r.busyAt??busy))+Math.max(0,this.lag-(r.lagAt??this.lag)));}
  // A one-shot deadline that tolerates our own lag (P-3), within the same credit cap.
  after(fn,ms){return lagTimeout(fn,ms,{thresholdMs:this.lagThresholdMs,maxCreditMs:this.pageCreditMaxMs});}
  // One warning per prefix, replaced by the latest relay reason.
  warn(prefix,why){this.status.warnings=[...this.status.warnings.filter(w=>!w.startsWith(prefix)),`${prefix} (${why}); polling`];}
  updateComplete(){
    const ok=!!this.opened&&this.status.connected&&!this.status.error&&(!this.readsRoster||this.rosterKey!==null)&&(!this.strict()||this.liveConfirmed())&&!(this.windowMode&&this.windowSince===undefined);
    const done=ok&&!this.blockingPending();
    this.status.historyComplete=done;
    this.status.windowComplete=ok&&!this.windowBlockingPending();
    this.status.pollInProgress=this.trustedPending()&&!this.blockingPending();
    // The stable period restarts when the connection stops being healthy (an error, or a trusted
    // live REQ no longer confirmed), with or without a socket close. A routine poll page in
    // flight is not a failure and does not restart it.
    if(!this.healthy()){clearTimeout(this.stableTimer);this.stableTimer=null;}
    else if(done&&this.closedStreak&&!this.stableTimer)this.stableTimer=setTimeout(()=>this.settle(),this.stableMs);
  }
  healthy(){return !this.dead&&this.status.connected&&!this.status.error&&(!this.strict()||this.liveConfirmed());}
  // A recovered connection stayed healthy for stableMs: reset the CLOSED backoff and recovery note.
  settle(){this.stableTimer=null;if(!this.healthy())return;this.closedStreak=0;this.retry=this.retryMs;this.status.warnings=this.status.warnings.filter(w=>!w.startsWith(RECOVERING));this.updateComplete();this.onChange();}
  // Fail closed now (error, not ready), then reconnect through the backoff for a full fresh round.
  recover(error){
    if(this.dead)return;
    this.dead=true;this.closedStreak++;this.lastClosed=error;
    Object.assign(this.status,{error,historyComplete:false,windowComplete:false,authenticated:false});
    this.onChange();this.closeSocket();
  }
  // Something needs AUTH (a challenge, or an auth-required refusal). If it neither completes nor is
  // refused within authWaitMs: while recovering, or when trusted history was refused for lack of
  // AUTH, retry on a fresh connection (never ready meanwhile). Otherwise (a relay that challenges
  // but never answers) treat it as refused and fall back to anonymous reads, as for an AUTH refusal.
  awaitAuth(reason){
    if(!this.authReason||reason.startsWith('auth-required:'))this.authReason=reason;
    this.authTimer??=this.after(()=>{this.authTimer=null;
      if(this.dead||this.status.authenticated||this.authRefused!==null)return;
      const why=`${this.authReason} (AUTH did not complete within ${this.authWaitMs/1000}s)`;
      if(this.closedStreak||this.authClosed.size)return this.recover(why);
      this.authRefused='not answered';this.warn('AUTH not answered',why);this.updateComplete();this.onChange();},this.authWaitMs);
  }
  sendNow(frame){if(this.ws?.readyState===1){this.ws.send(JSON.stringify(frame));if(frame[0]==='CLOSE')this.closing.add(frame[1]);}}
  // Queue a live REQ for one filter (or the applicant sample), replacing any earlier one under a new id.
  subscribe(id) {
    const old=this.liveSub.get(id),n=this.liveGen.get(id)??0;
    if(old){const e=this.live.get(old);this.live.delete(old);if(e?.sentAt)this.sendNow(['CLOSE',old]);else this.queue=this.queue.filter(m=>m[1]!==old);}
    const sub=n?`live-${id}~${n}`:`live-${id}`,applicants=id===APPLICANTS,filter=applicants?applicantFilter(this.buyer):this.filters.get(id);
    this.liveGen.set(id,n+1);this.liveSub.set(id,sub);this.live.set(sub,{id,filter,sentAt:0,authed:false,eose:false});
    // Window mode: a trusted live REQ starts at the window at the earliest (the backfill reads below it).
    const floor=this.windowMode&&typeof this.windowSince==='number'&&!applicants&&!soft(id)&&!id.startsWith(BOUND)?this.windowSince:0;
    this.queue.push(['REQ',sub,{...filter,since:Math.max(floor,(applicants?Math.floor(Date.now()/1000):this.newest.get(id)??0)-this.overlap)}]);
  }
  liveIds(){return [...[...this.filters.keys()].filter(id=>!this.deferred(id)),...(this.applicants?[APPLICANTS]:[])];}
  start() {
    if(this.stopped)return;
    this.status=this.freshStatus(); this.opened=false;
    this.filters=new Map([...this.initialFilters.map(f=>[String(f.kinds[0]),f]),...this.extra]);
    this.authClosed=new Map(); this.pages=new Map(); this.queue=[]; this.newest=new Map(); this.read=new Set();
    // live: per live REQ id {id, filter, sentAt, authed (sent after AUTH OK), eose}; liveSub: filter id -> current REQ id.
    // closing: our own CLOSEs, whose empty-reason CLOSED acknowledgement is not a refusal.
    this.live=new Map(); this.liveSub=new Map(); this.liveGen=new Map(); this.closing=new Set();
    this.dead=false; this.authRefused=null; this.challenged=false; this.authReason=null; this.holdExpired=false;
    const ws=this.ws=new this.WebSocketImpl(this.url);
    // Every teardown goes through close() and ends the connection exactly once (end()), without
    // waiting for the socket's own close event. Node 22's WebSocket, closed while still CONNECTING,
    // dispatches 'error' synchronously from close() and never dispatches 'close': the old
    // error -> ws.close() handler recursed until the stack overflowed, and a connect timeout could
    // never reach the reconnect. `closing` stops the re-entry; end() is idempotent, so a late close
    // event (Node 24) is ignored.
    let closing=false,ended=false,timeout;
    const end=()=>{
      if(ended)return;ended=true;
      timeout?.clear();clearInterval(this.queueTimer);clearInterval(this.pollTimer);this.clearRecoveryTimers();this.dead=true;
      this.status.connected=false;this.status.authenticated=false;this.status.historyComplete=false;this.status.windowComplete=false;this.onChange();
      if(!this.stopped){this.timer=setTimeout(()=>this.start(),this.retry);this.retry=Math.min(this.retry*2,this.closedStreak?this.closedMaxRetryMs:this.maxRetryMs);}
    };
    const close=()=>{if(!closing){closing=true;try{ws.close();}catch{}}end();};
    this.closeSocket=close;
    const send=frame=>{if(ws.readyState===1){ws.send(JSON.stringify(frame));if(frame[0]==='CLOSE')this.closing.add(frame[1]);}};
    // Once AUTH is expected, live REQs wait for its outcome: an anonymous live REQ would only be
    // replaced (or refused) after AUTH OK. History pages go out at once either way.
    // A relay that challenged an earlier connection is given authWaitMs to challenge this one first.
    const held=f=>f[0]==='REQ'&&this.live.has(f[1])&&!this.status.authenticated&&(this.authExpected()||this.expectAuth&&!this.challenged&&!this.holdExpired);
    // P-3: a tick that runs more than lagThresholdMs past its interval measures time this thread was
    // blocked; it is credited to every deadline still pending (late()).
    this.lastTick=now();this.tickBusy=busy;
    this.queueTimer=setInterval(()=>{const t=now(),lost=t-this.lastTick-this.queueMs-(busy-this.tickBusy);this.lastTick=t;this.tickBusy=busy;if(lost>this.lagThresholdMs)this.lag+=lost;
      const i=this.next(held);if(i>=0&&ws.readyState===1){const [frame]=this.queue.splice(i,1);send(frame);const p=this.pages.get(frame[1]),e=this.live.get(frame[1]);
        // Whether each REQ left before or after AUTH decides what a later auth-required CLOSED means.
        if(p){p.sentAt=Date.now();p.busyAt=busy;p.lagAt=this.lag;p.authed=this.status.authenticated;}
        // A live REQ is unconfirmed from the moment subscribe() creates it (every caller then runs
        // updateComplete), so readiness already dropped when it was queued, before this send.
        if(e)Object.assign(e,{sentAt:Date.now(),busyAt:busy,lagAt:this.lag,authed:this.status.authenticated});}
      const late=[...this.pages].filter(([,p])=>p.sentAt&&this.late(p));
      // A silent display-only read truncates that display; never the trusted history.
      for(const [id] of late)if(soft(id)){this.softFailure(id);this.updateComplete();this.onChange();}
      if(late.some(([id])=>!soft(id))){this.status.error='Relay history timed out';close();}
      else if(this.liveLate())this.recover('Relay live subscription timed out (no EOSE within 20s)');},this.queueMs);
    timeout=this.after(close,this.connectMs);
    this.pollTimer=setInterval(()=>{
      // Finish the entire trusted round before polling again, including member profiles.
      // Otherwise a large history can keep refilling faster than it drains.
      // A failed trusted history (truncated, refused) stays failed until a reconnect re-reads
      // it in full; polling it again would only resend the same requests every interval.
      if(this.status.error||this.trustedPending())return;
      // Poll also repairs late/backdated writes even when streaming succeeds.
      for(const [id,f] of this.filters)if(!this.pages.has(id)&&!this.deferred(id))this.page(id,{...f,since:Math.max(0,(this.newest.get(id)??0)-this.overlap)},{poll:true});
      this.updateComplete();
      if(this.applicants&&!this.pages.has(APPLICANTS))this.pageApplicants();
    },this.pollMs);
    ws.addEventListener('open',()=>{
      if(ended)return; // closed while connecting (stop, connect timeout): never a live round
      // A CLOSED-driven reconnect keeps its backoff until the recovered connection settles.
      timeout?.clear();this.status.connected=true;if(!this.closedStreak)this.retry=this.retryMs;this.opened=true;
      // History must work even if AUTH is absent or explicitly refused.
      for(const [id,f] of this.filters)if(!this.deferred(id))this.firstPage(id,f);
      if(this.applicants)this.pageApplicants();
      for(const id of this.liveIds())this.subscribe(id);
      // No challenge within authWaitMs: live REQs fall back to anonymous and the relay's posture
      // becomes anonymous, unless recovery needs AUTH OK; then this recovers like an unanswered
      // challenge (expectAuth stays set for the retry).
      if(this.expectAuth)this.holdTimer=this.after(()=>{this.holdTimer=null;if(this.challenged||this.dead)return;
        if(this.needsAuth())return this.recover(`Relay sent no AUTH challenge within ${this.authWaitMs/1000}s while recovering`);
        this.holdExpired=true;this.expectAuth=false;this.authAccepted=false;},this.authWaitMs);
      this.updateComplete();this.onChange();
    });
    // Every frame's handling time (verify, store, projection callbacks) counts toward busy (R3-6).
    ws.addEventListener('message',ev=>{const t0=now();try{handle(ev);}finally{busy+=now()-t0;}});
    const handle=({data})=>{
      if(this.dead||ws!==this.ws)return;
      let m; try {m=JSON.parse(String(data));}catch{return;}
      if(!Array.isArray(m))return;
      if(m[0]==='AUTH'&&typeof m[1]==='string') {
        // From here on AUTH is expected: no trusted round completes until it does (or is refused).
        if(!this.challenged){this.challenged=this.expectAuth=true;this.awaitAuth('Relay did not answer AUTH');}
        const e=signAuth(this.key,this.url,m[1]);this.authId=e.id;send(['AUTH',e]);
        this.updateComplete();this.onChange();
      } else if(m[0]==='OK'&&m[1]===this.authId) {
        this.status.authenticated=m[2]===true;
        this.authTimer?.clear();this.authTimer=null;
        if(this.status.authenticated) {
          this.authAccepted=true;
          // A late OK after the no-answer fallback still counts: AUTH is expected again.
          if(this.authRefused==='not answered'){this.authRefused=null;this.status.warnings=this.status.warnings.filter(w=>!w.startsWith('AUTH not answered'));}
          if(this.authClosed.size) {
            if(this.status.error?.startsWith('auth-required:'))this.status.error=null;
            for(const [id,p] of this.authClosed)this.page(id,p.filter,{phase:p.phase});
            this.authClosed.clear();
          }
          // A live REQ that already left anonymously is replaced (an open-read relay gave it
          // history only); one still queued simply leaves authenticated.
          for(const id of this.liveIds()){const e=this.live.get(this.liveSub.get(id));if(!e||e.sentAt&&!e.authed)this.subscribe(id);}
        } else {
          this.authRefused=m[3]||'no reason given';
          // The relay demanded AUTH for trusted history, then refused it: retry on a fresh connection.
          if(this.authClosed.size)return this.recover(`${this.status.error??'auth-required'}; AUTH refused: ${this.authRefused}`);
          // Recovering, and either the streak needs AUTH OK (even with every live REQ still installed)
          // or a trusted live REQ was refused for lack of AUTH: nothing can complete this round here.
          if(this.needsAuth()||this.closedStreak&&[...this.filters.keys()].some(id=>!soft(id)&&!this.liveSub.has(id)))
            return this.recover(`${this.authReason??'auth-required'}; AUTH refused: ${this.authRefused}`);
          this.authAccepted=false; // refused outside recovery: anonymous posture from now on
          this.warn('AUTH refused',this.authRefused); // anonymous reads, like the no-answer fallback
        }
        this.updateComplete();this.onChange();
      } else if(m[0]==='EVENT') {
        const p=this.pages.get(m[1]),l=this.live.get(m[1]),f=p?.filter??l?.filter,e=m[2];
        // Saturation counts every frame the relay spent its page limit on, including events
        // outside the requested window: a relay ignoring since/until must not look complete.
        if(p)p.received++;
        if(!f||!verify(e)||!matches(e,f))return;
        const id=p?m[1]:l.id;
        if(id===APPLICANTS) {
          if(validMembership(e,{pubkey:this.buyer})&&this.store.addApplicant(e))this.onChange();
          return;
        }
        if(p)p.oldest=Math.min(p.oldest,e.created_at);
        this.newest.set(id,Math.max(this.newest.get(id)??0,e.created_at));
        if(e.kind===MEMBERSHIP&&!validMembership(e,{pubkey:this.buyer}))return;
        // Decisions first: the roster is applied once this relay's decision history is read,
        // then on every later (live/poll) decision.
        if(this.store.add(e)){if(e.kind===DECISION&&!this.pages.has(String(DECISION)))this.onRoster();this.onChange();}
      } else if(m[0]==='EOSE'&&this.pages.has(m[1])) {
        const p=this.pages.get(m[1]);this.pages.delete(m[1]);
        if(m[1]===APPLICANTS) {
          // One newest-first page per round, never paged: a flood truncates, it cannot stall.
          this.status.applicantsIncomplete=p.received>=APPLICANT_LIMIT;
        } else if(p.received>=PAGE_LIMIT&&metadataId(m[1])) {
          this.metaIncomplete(); // one author's replaceable metadata; never worth failing history
        } else if(p.received>=PAGE_LIMIT) {
          // Re-read the oldest second (inclusive until): a second that may still hold unread
          // events is never skipped. The cursor must move strictly older or history fails closed.
          if(p.oldest===Infinity)this.status.error='History truncated: full page without an in-range event';
          else if(p.filter.until!==undefined&&p.oldest>=p.filter.until)this.status.error='History truncated: 500+ events in one second';
          else this.page(m[1],{...p.filter,until:p.oldest},{poll:p.poll,phase:p.phase});
        }
        // A trusted filter's history is read once its last page ends without an error (R3-4).
        // Window mode: a window read in full goes on to the history below it (backfill).
        if(!soft(m[1])&&m[1]!==APPLICANTS&&!this.pages.has(m[1])&&!this.status.error) {
          if(p.phase==='window'){this.page(m[1],{...this.filters.get(m[1]),until:this.windowSince},{phase:'backfill'});if(m[1]===String(OFFER))this.bindWindow();}
          else this.read.add(m[1]);
        }
        if(!this.pages.has(m[1]))send(['CLOSE',m[1]]);
        if(m[1]===String(DECISION)&&!this.pages.has(m[1]))this.onRoster();
        this.updateComplete();this.onChange();
      } else if(m[0]==='EOSE'&&this.live.get(m[1])?.sentAt) {
        this.live.get(m[1]).eose=true;
        this.updateComplete();this.onChange();
      } else if(m[0]==='CLOSED') {
        const sub=m[1],reason=typeof m[2]==='string'?m[2]:'',why=reason||'no reason given',auth=reason.startsWith('auth-required:');
        // Buzz acknowledges our own CLOSE with an empty-reason CLOSED; it never refuses a newer REQ.
        if(!reason&&this.closing.delete(sub))return;
        const l=this.live.get(sub);
        if(l) {
          const id=l.id,early=auth&&!l.authed; // auth-required for a REQ sent before AUTH OK: AUTH OK reinstalls it
          if(soft(id)) {
            // Display-only feeds (applicants, profiles/heartbeats) never gate readiness; polls cover them.
            this.live.delete(sub);this.liveSub.delete(id);
            if(!early)this.warn(id===APPLICANTS?'Applicant live feed closed by relay':'Profile/heartbeat live feed closed by relay',why);
          } else if(early&&!this.status.authenticated) {
            this.live.delete(sub);this.liveSub.delete(id);
            if(this.strict())this.awaitAuth(reason);
            this.updateComplete();
          }
          // Authenticated live feed lost, or any trusted live CLOSED in a strict round: only a new
          // connection can re-AUTH and resubscribe.
          else if(this.status.authenticated||this.strict())return this.recover(`Relay closed live subscription ${id}: ${why}`);
          // Anonymous (no AUTH offered, or refused): the overlapping polls keep history complete.
          else {this.live.delete(sub);this.liveSub.delete(id);this.warn(`Live subscription ${id} closed by relay`,why);}
        } else if(soft(sub)&&this.pages.has(sub)) {this.softFailure(sub);this.updateComplete();}
        else if(this.pages.has(sub)) {
          const p=this.pages.get(sub);
          this.read.delete(sub); // a refused read must be re-read in full before history counts again
          // Refused trusted history: fail closed now and re-read it in full on a fresh connection.
          if(!auth||(p.authed??this.status.authenticated))return this.recover(`Relay history subscription refused (${sub}): ${why}`);
          this.pages.delete(sub);this.status.historyComplete=false;this.status.windowComplete=false;
          if(this.status.authenticated)this.page(sub,p.filter,{phase:p.phase}); // refused before AUTH; AUTH already succeeded
          // AUTH was already refused on this connection: retry it on a fresh one.
          else if(this.authRefused!==null)return this.recover(`${reason}; AUTH refused: ${this.authRefused}`);
          else {
            this.authClosed.set(sub,{filter:p.filter,phase:p.phase});
            if(!this.status.error||this.status.error.startsWith('auth-required:'))this.status.error=reason;
            this.awaitAuth(reason); // AUTH OK re-reads it; otherwise a fresh connection retries
            this.updateComplete(); // the error also restarts the stable period now
          }
        }
        this.onChange();
      }
    };
    ws.addEventListener('error',close);
    ws.addEventListener('close',end);
  }
  // Accepted sellers (buyer-signed decisions only) drive roster membership reads and
  // profile/heartbeat reads. Re-subscribes whenever the accepted set changes.
  setRoster(accepted) {
    const sorted=[...new Set(accepted)].sort(),key=sorted.join(',');
    if(key===this.rosterKey)return;
    this.rosterKey=key;
    if(this.readsRoster) {
      // Ids derive from each chunk's author list: unchanged chunks keep their subscription, and
      // a late EOSE for an older author set can never complete a different one.
      this.replaceExtra(id=>id.startsWith('roster-'),new Map(rosterFilters(this.buyer,sorted).map(f=>[`roster-${bytesToHex(sha256(f.authors.join(','))).slice(0,16)}`,f])));
    }
    if(this.readsMetadata) {
      const selected=sorted.slice(0,METADATA_CAP);
      this.members=new Set(selected);
      // Display only: a seller without profile/heartbeat stays eligible and assignable.
      this.metaWarnings=sorted.length>METADATA_CAP?[`Profile cap: metadata shown for ${METADATA_CAP} of ${sorted.length} accepted sellers`]:[];
      // Replace only the cap warning; keep any other (e.g. an incomplete profile/heartbeat read).
      this.status.warnings=[...this.metaWarnings,...this.status.warnings.filter(w=>!w.startsWith('Profile cap:'))];
      this.replaceExtra(metadataId,new Map(selected.flatMap(pk=>[PROFILE,HEARTBEAT].map(kind=>[`${kind}-${pk}`,{kinds:[kind],authors:[pk]}]))));
    }
  }
  replaceExtra(owns,entries) {
    for(const id of [...this.extra.keys()])if(owns(id)&&!entries.has(id)){this.extra.delete(id);this.dropFilter(id);}
    for(const [id,f] of entries)if(!this.extra.has(id)){this.extra.set(id,f);this.addFilter(id,f);}
    if(this.pages)this.updateComplete();
  }
  addFilter(id,filter) {
    if(!this.filters)return;
    this.filters.set(id,filter);
    if(!this.opened||this.deferred(id))return; // the open handler (or setWindow) pages and subscribes it
    this.firstPage(id,filter);this.subscribe(id);
  }
  dropFilter(id) {
    if(!this.filters)return;
    const sub=this.liveSub.get(id)??`live-${id}`;
    this.filters.delete(id);this.pages.delete(id);this.read?.delete(id);this.live.delete(sub);this.liveSub.delete(id);this.newest.delete(id);this.authClosed.delete(id);
    this.queue=this.queue.filter(m=>m[1]!==id&&m[1]!==sub);
    for(const s of [id,sub])this.sendNow(['CLOSE',s]);
  }
  page(id,filter,{poll=false,phase}={}) {const p={filter,received:0,oldest:Infinity,poll,phase};this.pages.set(id,p);if(this.blockingPending())this.status.historyComplete=false;if(this.windowBlockingPending())this.status.windowComplete=false;this.queue.push(['REQ',id,{...filter,limit:PAGE_LIMIT}]);}
  pageApplicants() {const filter=applicantFilter(this.buyer);this.pages.set(APPLICANTS,{filter,received:0,oldest:Infinity});this.queue.push(['REQ',APPLICANTS,{...filter,limit:APPLICANT_LIMIT}]);}
  clearRecoveryTimers(){this.authTimer?.clear();this.authTimer=null;this.holdTimer?.clear();this.holdTimer=null;clearTimeout(this.stableTimer);this.stableTimer=null;}
  stop(){this.stopped=true;clearTimeout(this.timer);clearInterval(this.queueTimer);clearInterval(this.pollTimer);this.clearRecoveryTimers();this.closeSocket?.();}
}

// Canonical hostname denylist; the Rust sidecar reads this JSON literal at compile time.
export const CHALLENGE_RELAY_DENYLIST = ["relay.maxplayer.ai"];
export const DEFAULT_CHALLENGE_RELAYS = ['wss://nos.lol','wss://relay.primal.net','wss://offchain.pub'];
// Challenge relay quorum (run 2 #13): how many challenge relays must complete a read, and must
// acknowledge a publish (sidecar), for it to count. The same number is used for both, and it must
// be a strict majority of the configured relays: every quorum read then shares at least one relay
// with every quorum publish, so a read still sees every published leave, deny or close as long as
// that shared relay does not hide it. Default 2 (for the three default relays), raised to a
// majority for longer lists and lowered to the list size for one relay. Our trade relay is never
// part of the quorum and is always required on top of it.
export const DEFAULT_CHALLENGE_QUORUM = 2;
export const majority = n => Math.floor(n/2)+1;
export function parseChallengeQuorum(value,relayCount) {
  if(!Number.isSafeInteger(relayCount)||relayCount<1)throw Error('At least one challenge relay is required');
  if(value===undefined||value===null||value==='')return Math.min(relayCount,Math.max(DEFAULT_CHALLENGE_QUORUM,majority(relayCount)));
  const q=typeof value==='number'?value:/^[0-9]+$/.test(String(value).trim())?Number(String(value).trim()):NaN;
  if(!Number.isSafeInteger(q)||q<majority(relayCount)||q>relayCount)
    throw Error(`Invalid challenge relay quorum ${JSON.stringify(String(value))}: must be an integer from ${majority(relayCount)} (a majority) to ${relayCount} (every relay)`);
  return q;
}
export function parseChallengeRelays(value=DEFAULT_CHALLENGE_RELAYS) {
  const urls=[...new Set((Array.isArray(value)?value:value.split(',')).map(url=>url.trim()).filter(Boolean))];
  if(!urls.length)throw Error('At least one challenge relay is required');
  for(const url of urls) {
    const parsed=new URL(url);
    if(!['ws:','wss:'].includes(parsed.protocol))throw Error('Invalid challenge relay URL');
    if(CHALLENGE_RELAY_DENYLIST.includes(parsed.hostname.toLowerCase().replace(/\.+$/,'')))throw Error('production relay forbidden for challenges');
  }
  return urls;
}
// Each transport validates signatures and scope; EventStore unions/deduplicates IDs
// and resolves replaceable events across all sources, independent of arrival order.
// The accepted roster is recomputed from the shared store whenever a buyer decision arrives
// on any relay and pushed to every reader (roster memberships + trade metadata).
export class ChallengeReader {
  constructor({url,challengeRelays=DEFAULT_CHALLENGE_RELAYS,buyer,store,onChange=()=>{},...rest}) {
    // quorum: challenge relays that must complete (default: parseChallengeQuorum). The rest are RelayReader options.
    // tradeWindowMarginS (the /swarm page only; default off = today's single full read): the trade
    // relay reads the current run first. Once a quorum of challenge relays has read the challenge
    // events, the window starts tradeWindowMarginS before the current run's start (no run: the whole
    // history), and older trade history backfills afterwards (RelayReader window mode).
    const {quorum,tradeWindowMarginS,...options}=rest;
    if(tradeWindowMarginS!==undefined&&!(Number.isSafeInteger(tradeWindowMarginS)&&tradeWindowMarginS>=0))throw Error('Invalid trade window margin');
    const filters=challengeFilters(buyer);
    const onRoster=()=>{const accepted=acceptedSellers(store.events.values(),buyer);for(const r of [this.trade,...this.challenges])r.setRoster(accepted);};
    const changed=tradeWindowMarginS===undefined?onChange:()=>{this.chooseWindow();onChange();};
    const common={...options,buyer,store,onChange:changed,onRoster};
    this.store=store;this.buyer=buyer;this.windowMargin=tradeWindowMarginS;
    this.trade=new RelayReader({...common,url,roster:false,metadata:true,applicants:false,window:tradeWindowMarginS!==undefined,filters:filters.filter(f=>!CHALLENGE_KINDS.includes(f.kinds[0]))});
    const urls=parseChallengeRelays(challengeRelays);
    this.quorum=parseChallengeQuorum(quorum,urls.length);
    this.challenges=urls.map(url=>new RelayReader({...common,url,roster:true,metadata:false,applicants:true,
      filters:filters.filter(f=>CHALLENGE_KINDS.includes(f.kinds[0]))}));
  }
  // Ready (historyComplete) needs our trade relay complete AND at least `quorum` challenge relays
  // complete. The store unions every relay's events, so a relay that is still connected keeps
  // contributing. Below quorum, or with the trade relay incomplete, the view is never ready: the
  // runner posts and publishes nothing. Quorum met with a relay missing is "degraded": ready, with a
  // visible warning (page, runner log) naming the missing relays.
  get status() {
    const relays=[{role:'trade',url:this.trade.url,...this.trade.status},...this.challenges.map(r=>({role:'challenge',url:r.url,...r.status}))];
    const available=this.challenges.filter(r=>r.status.connected&&!r.status.error);
    const complete=available.filter(r=>r.status.historyComplete);
    const missingRelays=this.challenges.filter(r=>!complete.includes(r)).map(r=>r.url);
    const quorum={required:this.quorum,complete:complete.length,total:this.challenges.length};
    const quorumMet=complete.length>=this.quorum,degraded=quorumMet&&missingRelays.length>0;
    const failedRelays=relays.filter(r=>!r.connected||r.error).map(r=>r.url);
    const degradedWarning=`Degraded: ${complete.length} of ${quorum.total} challenge relays complete (quorum ${this.quorum}); missing: ${missingRelays.join(', ')}`;
    return {connected:this.trade.status.connected&&available.length>0,
      authenticated:this.trade.status.authenticated,
      historyComplete:this.trade.status.historyComplete&&quorumMet,
      // A poll round re-reading history already held (R3-4); never part of readiness.
      pollInProgress:[this.trade,...this.challenges].some(r=>r.status.pollInProgress),
      warnings:[...new Set([...relays.flatMap(r=>r.warnings),...(degraded?[degradedWarning]:[])])],
      // Display only; never part of readiness or the runner's exhaustion stop.
      applicantsIncomplete:this.challenges.some(r=>r.status.applicantsIncomplete)||!!this.store.applicantsTruncated,
      // An error only once the quorum cannot be met by the relays still up (a relay still catching
      // up is not a failure; historyComplete stays false meanwhile).
      error:this.trade.status.error??(available.length<this.quorum?`Swarm history incomplete; quorum not met (${available.length} of ${quorum.total} challenge relays available, need ${this.quorum}); relay failed: ${failedRelays.filter(u=>u!==this.trade.url).join(', ')}`:null),
      failedRelays,missingRelays,quorum,degraded,relays,
      // Window mode: the trade relay's window (and the quorum) complete. windowSince: the window's
      // lower bound, null when the whole history is (or is being) read in one pass. Without window
      // mode windowComplete is historyComplete. A run's trade data is complete only when
      // historyComplete, or windowComplete with windowSince at or before the run's start.
      windowComplete:this.trade.status.windowComplete&&quorumMet,
      windowSince:typeof this.trade.windowSince==='number'?this.trade.windowSince:null};
  }
  // Window mode: choose the trade window once a quorum of challenge relays has read the challenge
  // events. The page checks that the window covers the run it shows, so a run chosen later (a newer
  // event from a slow relay) is never shown from the window alone.
  chooseWindow(){
    if(this.windowMargin===undefined||this.trade.windowSince!==undefined)return;
    const read=this.challenges.filter(r=>r.status.connected&&!r.status.error&&r.read?.has(String(CHALLENGE))).length;
    if(read<this.quorum)return;
    const c=currentChallenge(buyerChallenges([...this.store.events.values()],this.buyer));
    this.trade.setWindow(c?Math.max(0,number(tag(c,'starts'))-this.windowMargin):null);
  }
  start(){this.trade.start();for(const r of this.challenges)r.start();}
  stop(){this.trade.stop();for(const r of this.challenges)r.stop();}
}
