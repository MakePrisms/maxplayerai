import { CHALLENGE, MEMBERSHIP, DECISION, OFFER, CLAIM, RESULT, AWARD, ACCEPT, HEARTBEAT, PROFILE } from '../../model/kinds.ts';
import { schnorr } from '@noble/curves/secp256k1';
import { sha256 } from '@noble/hashes/sha256';
import { bytesToHex } from '@noble/hashes/utils';
import { signAuth, verify, mentions, validMembership } from './events.mjs';
import { acceptedSellers } from './model.mjs';

// Per-REQ page size, applicant sample size, metadata budget and authors chunk size.
export const PAGE_LIMIT=500, APPLICANT_LIMIT=256, METADATA_CAP=256, AUTHOR_CHUNK=100;
const APPLICANTS='applicants';
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
  (f.since===undefined || e.created_at>=f.since) && (f.until===undefined || e.created_at<=f.until);
const metadataId=id=>id.startsWith(`${PROFILE}-`)||id.startsWith(`${HEARTBEAT}-`);
const soft=id=>id===APPLICANTS||metadataId(id);
const RECOVERING='Recovering after relay CLOSED';
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
// Live REQ ids: `live-<id>` for a filter's first live REQ on a connection, `live-<id>~<n>` for
// a replacement, so an EOSE, EVENT or CLOSED of a replaced REQ never counts for the current one.
export class RelayReader {
  constructor({url,buyer,store,onChange=()=>{},WebSocketImpl=WebSocket,pollMs=15000,queueMs=300,overlap=900,
    retryMs=1000,maxRetryMs=30000,closedMaxRetryMs=300000,authWaitMs=15000,stableMs=60000,filters=challengeFilters(buyer),
    roster=filters.some(f=>f.kinds.includes(DECISION)),metadata=filters.some(f=>f.kinds.includes(OFFER)),applicants=roster,onRoster=null,key=schnorr.utils.randomPrivateKey()}) {
    Object.assign(this,{url,buyer,store,onChange,WebSocketImpl,pollMs,queueMs,overlap,retryMs,maxRetryMs,closedMaxRetryMs,authWaitMs,stableMs,readsRoster:roster,readsMetadata:metadata,applicants});
    this.onRoster=onRoster??(()=>this.setRoster(acceptedSellers(this.store.events.values(),this.buyer)));
    this.initialFilters=filters; this.extra=new Map(); this.members=new Set(); this.rosterKey=null; this.metaWarnings=[];
    this.key=key; this.closedStreak=0; this.lastClosed=null; this.expectAuth=false; // expectAuth: an earlier connection was challenged
    this.authAccepted=false; // the relay accepted our AUTH and has not since refused it or stopped challenging outside recovery
    this.status=this.freshStatus();
    this.stopped=false; this.retry=retryMs;
  }
  freshStatus(){return {connected:false,authenticated:false,historyComplete:false,error:null,
    warnings:[...this.metaWarnings,...(this.closedStreak?[`${RECOVERING} (${this.lastClosed}); reconnect ${this.closedStreak}`]:[])],applicantsIncomplete:false};}
  // Applicant and profile/heartbeat reads are display-only: they never gate readiness.
  trustedPending(){for(const id of this.pages.keys())if(!soft(id))return true;return false;}
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
    for(const [id,sub] of this.liveSub){const e=this.live.get(sub);if(!soft(id)&&e?.sentAt&&!e.eose&&Date.now()-e.sentAt>20000)return true;}
    return false;
  }
  // One warning per prefix, replaced by the latest relay reason.
  warn(prefix,why){this.status.warnings=[...this.status.warnings.filter(w=>!w.startsWith(prefix)),`${prefix} (${why}); polling`];}
  updateComplete(){
    const done=!!this.opened&&this.status.connected&&!this.trustedPending()&&!this.status.error&&(!this.readsRoster||this.rosterKey!==null)&&(!this.strict()||this.liveConfirmed());
    this.status.historyComplete=done;
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
    Object.assign(this.status,{error,historyComplete:false,authenticated:false});
    this.onChange();this.ws.close();
  }
  // Something needs AUTH (a challenge, or an auth-required refusal). If it neither completes nor is
  // refused within authWaitMs: while recovering, or when trusted history was refused for lack of
  // AUTH, retry on a fresh connection (never ready meanwhile). Otherwise (a relay that challenges
  // but never answers) treat it as refused and fall back to anonymous reads, as for an AUTH refusal.
  awaitAuth(reason){
    if(!this.authReason||reason.startsWith('auth-required:'))this.authReason=reason;
    this.authTimer??=setTimeout(()=>{this.authTimer=null;
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
    this.queue.push(['REQ',sub,{...filter,since:Math.max(0,(applicants?Math.floor(Date.now()/1000):this.newest.get(id)??0)-this.overlap)}]);
  }
  liveIds(){return [...this.filters.keys(),...(this.applicants?[APPLICANTS]:[])];}
  start() {
    if(this.stopped)return;
    this.status=this.freshStatus(); this.opened=false;
    this.filters=new Map([...this.initialFilters.map(f=>[String(f.kinds[0]),f]),...this.extra]);
    this.authClosed=new Map(); this.pages=new Map(); this.queue=[]; this.newest=new Map();
    // live: per live REQ id {id, filter, sentAt, authed (sent after AUTH OK), eose}; liveSub: filter id -> current REQ id.
    // closing: our own CLOSEs, whose empty-reason CLOSED acknowledgement is not a refusal.
    this.live=new Map(); this.liveSub=new Map(); this.liveGen=new Map(); this.closing=new Set();
    this.dead=false; this.authRefused=null; this.challenged=false; this.authReason=null; this.holdExpired=false;
    const ws=this.ws=new this.WebSocketImpl(this.url);
    const send=frame=>{if(ws.readyState===1){ws.send(JSON.stringify(frame));if(frame[0]==='CLOSE')this.closing.add(frame[1]);}};
    // Once AUTH is expected, live REQs wait for its outcome: an anonymous live REQ would only be
    // replaced (or refused) after AUTH OK. History pages go out at once either way.
    // A relay that challenged an earlier connection is given authWaitMs to challenge this one first.
    const held=f=>f[0]==='REQ'&&this.live.has(f[1])&&!this.status.authenticated&&(this.authExpected()||this.expectAuth&&!this.challenged&&!this.holdExpired);
    this.queueTimer=setInterval(()=>{const i=this.queue.findIndex(f=>!held(f));if(i>=0&&ws.readyState===1){const [frame]=this.queue.splice(i,1);send(frame);const p=this.pages.get(frame[1]),e=this.live.get(frame[1]);
        // Whether each REQ left before or after AUTH decides what a later auth-required CLOSED means.
        if(p){p.sentAt=Date.now();p.authed=this.status.authenticated;}
        // A live REQ is unconfirmed from the moment subscribe() creates it (every caller then runs
        // updateComplete), so readiness already dropped when it was queued, before this send.
        if(e)Object.assign(e,{sentAt:Date.now(),authed:this.status.authenticated});}
      const late=[...this.pages].filter(([,p])=>p.sentAt&&Date.now()-p.sentAt>20000);
      // A silent display-only read truncates that display; never the trusted history.
      for(const [id] of late)if(soft(id)){this.softFailure(id);this.updateComplete();this.onChange();}
      if(late.some(([id])=>!soft(id))){this.status.error='Relay history timed out';ws.close();}
      else if(this.liveLate())this.recover('Relay live subscription timed out (no EOSE within 20s)');},this.queueMs);
    const timeout=setTimeout(()=>ws.close(),20000);
    this.pollTimer=setInterval(()=>{
      // Finish the entire trusted round before polling again, including member profiles.
      // Otherwise a large history can keep refilling faster than it drains.
      // A failed trusted history (truncated, refused) stays failed until a reconnect re-reads
      // it in full; polling it again would only resend the same requests every interval.
      if(this.status.error||this.trustedPending())return;
      // Poll also repairs late/backdated writes even when streaming succeeds.
      for(const [id,f] of this.filters)if(!this.pages.has(id))this.page(id,{...f,since:Math.max(0,(this.newest.get(id)??0)-this.overlap)});
      if(this.applicants&&!this.pages.has(APPLICANTS))this.pageApplicants();
    },this.pollMs);
    ws.addEventListener('open',()=>{
      // A CLOSED-driven reconnect keeps its backoff until the recovered connection settles.
      clearTimeout(timeout);this.status.connected=true;if(!this.closedStreak)this.retry=this.retryMs;this.opened=true;
      // History must work even if AUTH is absent or explicitly refused.
      for(const [id,f] of this.filters)this.page(id,f);
      if(this.applicants)this.pageApplicants();
      for(const id of this.liveIds())this.subscribe(id);
      // No challenge within authWaitMs: live REQs fall back to anonymous and the relay's posture
      // becomes anonymous, unless recovery needs AUTH OK; then this recovers like an unanswered
      // challenge (expectAuth stays set for the retry).
      if(this.expectAuth)this.holdTimer=setTimeout(()=>{this.holdTimer=null;if(this.challenged||this.dead)return;
        if(this.needsAuth())return this.recover(`Relay sent no AUTH challenge within ${this.authWaitMs/1000}s while recovering`);
        this.holdExpired=true;this.expectAuth=false;this.authAccepted=false;},this.authWaitMs);
      this.updateComplete();this.onChange();
    });
    ws.addEventListener('message',({data})=>{
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
        clearTimeout(this.authTimer);this.authTimer=null;
        if(this.status.authenticated) {
          this.authAccepted=true;
          // A late OK after the no-answer fallback still counts: AUTH is expected again.
          if(this.authRefused==='not answered'){this.authRefused=null;this.status.warnings=this.status.warnings.filter(w=>!w.startsWith('AUTH not answered'));}
          if(this.authClosed.size) {
            if(this.status.error?.startsWith('auth-required:'))this.status.error=null;
            for(const [id,filter] of this.authClosed)this.page(id,filter);
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
          else this.page(m[1],{...p.filter,until:p.oldest});
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
          // Refused trusted history: fail closed now and re-read it in full on a fresh connection.
          if(!auth||(p.authed??this.status.authenticated))return this.recover(`Relay history subscription refused (${sub}): ${why}`);
          this.pages.delete(sub);this.status.historyComplete=false;
          if(this.status.authenticated)this.page(sub,p.filter); // refused before AUTH; AUTH already succeeded
          // AUTH was already refused on this connection: retry it on a fresh one.
          else if(this.authRefused!==null)return this.recover(`${reason}; AUTH refused: ${this.authRefused}`);
          else {
            this.authClosed.set(sub,p.filter);
            if(!this.status.error||this.status.error.startsWith('auth-required:'))this.status.error=reason;
            this.awaitAuth(reason); // AUTH OK re-reads it; otherwise a fresh connection retries
            this.updateComplete(); // the error also restarts the stable period now
          }
        }
        this.onChange();
      }
    });
    ws.addEventListener('error',()=>ws.close());
    ws.addEventListener('close',()=>{
      clearTimeout(timeout);clearInterval(this.queueTimer);clearInterval(this.pollTimer);this.clearRecoveryTimers();this.dead=true;
      this.status.connected=false;this.status.authenticated=false;this.status.historyComplete=false;this.onChange();
      if(!this.stopped){this.timer=setTimeout(()=>this.start(),this.retry);this.retry=Math.min(this.retry*2,this.closedStreak?this.closedMaxRetryMs:this.maxRetryMs);}
    });
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
    if(!this.opened)return; // the open handler pages and subscribes every filter
    this.page(id,filter);this.subscribe(id);
  }
  dropFilter(id) {
    if(!this.filters)return;
    const sub=this.liveSub.get(id)??`live-${id}`;
    this.filters.delete(id);this.pages.delete(id);this.live.delete(sub);this.liveSub.delete(id);this.newest.delete(id);this.authClosed.delete(id);
    this.queue=this.queue.filter(m=>m[1]!==id&&m[1]!==sub);
    for(const s of [id,sub])this.sendNow(['CLOSE',s]);
  }
  page(id,filter) {this.status.historyComplete=false;this.pages.set(id,{filter,received:0,oldest:Infinity});this.queue.push(['REQ',id,{...filter,limit:PAGE_LIMIT}]);}
  pageApplicants() {const filter=applicantFilter(this.buyer);this.pages.set(APPLICANTS,{filter,received:0,oldest:Infinity});this.queue.push(['REQ',APPLICANTS,{...filter,limit:APPLICANT_LIMIT}]);}
  clearRecoveryTimers(){clearTimeout(this.authTimer);this.authTimer=null;clearTimeout(this.holdTimer);this.holdTimer=null;clearTimeout(this.stableTimer);this.stableTimer=null;}
  stop(){this.stopped=true;clearTimeout(this.timer);clearInterval(this.queueTimer);clearInterval(this.pollTimer);this.clearRecoveryTimers();this.ws?.close();}
}

// Canonical hostname denylist; the Rust sidecar reads this JSON literal at compile time.
export const CHALLENGE_RELAY_DENYLIST = ["relay.maxplayer.ai"];
export const DEFAULT_CHALLENGE_RELAYS = ['wss://nos.lol','wss://relay.primal.net','wss://offchain.pub'];
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
  constructor({url,challengeRelays=DEFAULT_CHALLENGE_RELAYS,buyer,store,onChange=()=>{},...options}) {
    const filters=challengeFilters(buyer);
    const onRoster=()=>{const accepted=acceptedSellers(store.events.values(),buyer);for(const r of [this.trade,...this.challenges])r.setRoster(accepted);};
    const common={...options,buyer,store,onChange,onRoster};
    this.store=store;
    this.trade=new RelayReader({...common,url,roster:false,metadata:true,applicants:false,filters:filters.filter(f=>!CHALLENGE_KINDS.includes(f.kinds[0]))});
    this.challenges=parseChallengeRelays(challengeRelays).map(url=>new RelayReader({...common,url,roster:true,metadata:false,applicants:true,
      filters:filters.filter(f=>CHALLENGE_KINDS.includes(f.kinds[0]))}));
  }
  get status() {
    const relays=[{role:'trade',url:this.trade.url,...this.trade.status},...this.challenges.map(r=>({role:'challenge',url:r.url,...r.status}))];
    const available=this.challenges.filter(r=>r.status.connected&&!r.status.error);
    const ready=available.length===this.challenges.length&&available.every(r=>r.status.historyComplete);
    const failedRelays=relays.filter(r=>!r.connected||r.error).map(r=>r.url);
    return {connected:this.trade.status.connected&&available.length>0,
      authenticated:this.trade.status.authenticated,
      historyComplete:this.trade.status.historyComplete&&ready,
      warnings:[...new Set(relays.flatMap(r=>r.warnings))],
      // Display only; never part of readiness or the runner's exhaustion stop.
      applicantsIncomplete:this.challenges.some(r=>r.status.applicantsIncomplete)||!!this.store.applicantsTruncated,
      error:this.trade.status.error??(failedRelays.length?`Swarm history incomplete; relay failed: ${failedRelays.join(', ')}`:null),failedRelays,relays};
  }
  start(){this.trade.start();for(const r of this.challenges)r.start();}
  stop(){this.trade.stop();for(const r of this.challenges)r.stop();}
}
