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
// Read-only transport. The only signed frame is NIP-42 AUTH, using an in-memory
// throwaway key (one key per relay connection). Never loads a wallet or publishes EVENT.
//
// Trusted state (historyComplete, error) covers only the fixed filters and the roster
// membership filters. The applicant sample never affects it: a flood there can only
// truncate the "waiting for approval" list (status.applicantsIncomplete).
export class RelayReader {
  constructor({url,buyer,store,onChange=()=>{},WebSocketImpl=WebSocket,pollMs=15000,queueMs=300,overlap=900,filters=challengeFilters(buyer),
    roster=filters.some(f=>f.kinds.includes(DECISION)),metadata=filters.some(f=>f.kinds.includes(OFFER)),applicants=roster,onRoster=null,key=schnorr.utils.randomPrivateKey()}) {
    Object.assign(this,{url,buyer,store,onChange,WebSocketImpl,pollMs,queueMs,overlap,readsRoster:roster,readsMetadata:metadata,applicants});
    this.onRoster=onRoster??(()=>this.setRoster(acceptedSellers(this.store.events.values(),this.buyer)));
    this.initialFilters=filters; this.extra=new Map(); this.members=new Set(); this.rosterKey=null; this.metaWarnings=[];
    this.key=key;
    this.status=this.freshStatus();
    this.stopped=false; this.retry=1000;
  }
  freshStatus(){return {connected:false,authenticated:false,historyComplete:false,error:null,warnings:[...this.metaWarnings],applicantsIncomplete:false};}
  // Applicant and profile/heartbeat reads are display-only: they never gate readiness.
  trustedPending(){for(const id of this.pages.keys())if(!soft(id))return true;return false;}
  softFailure(id){this.pages.delete(id);if(id===APPLICANTS)this.status.applicantsIncomplete=true;else this.metaIncomplete();}
  metaIncomplete(){const w='Profile/heartbeat read incomplete';if(!this.status.warnings.includes(w))this.status.warnings.push(w);}
  // A roster reader is complete only after its roster (from complete decision history) was read.
  updateComplete(){this.status.historyComplete=!!this.opened&&!this.trustedPending()&&!this.status.error&&(!this.readsRoster||this.rosterKey!==null);}
  start() {
    if(this.stopped)return;
    this.status=this.freshStatus(); this.opened=false;
    this.filters=new Map([...this.initialFilters.map(f=>[String(f.kinds[0]),f]),...this.extra]);
    this.authClosed=new Map(); this.pages=new Map(); this.queue=[]; this.newest=new Map(); this.live=new Map();
    const ws=this.ws=new this.WebSocketImpl(this.url);
    const send=frame=>{if(ws.readyState===1)ws.send(JSON.stringify(frame));};
    this.queueTimer=setInterval(()=>{if(this.queue.length&&ws.readyState===1){const frame=this.queue.shift();send(frame);const p=this.pages.get(frame[1]);if(p)p.sentAt=Date.now();}
      const late=[...this.pages].filter(([,p])=>p.sentAt&&Date.now()-p.sentAt>20000);
      // A silent display-only read truncates that display; never the trusted history.
      for(const [id] of late)if(soft(id)){this.softFailure(id);this.updateComplete();this.onChange();}
      if(late.some(([id])=>!soft(id))){this.status.error='Relay history timed out';ws.close();}},this.queueMs);
    const timeout=setTimeout(()=>ws.close(),20000);
    this.pollTimer=setInterval(()=>{
      // Finish the entire trusted round before polling again, including member profiles.
      // Otherwise a large history can keep refilling faster than it drains.
      if(this.trustedPending())return;
      // Poll also repairs late/backdated writes even when streaming succeeds.
      for(const [id,f] of this.filters)if(!this.pages.has(id))this.page(id,{...f,since:Math.max(0,(this.newest.get(id)??0)-this.overlap)});
      if(this.applicants&&!this.pages.has(APPLICANTS))this.pageApplicants();
    },this.pollMs);
    const subscribe=()=>{
      for(const [id,f] of this.filters) {
        const sub=`live-${id}`; this.live.set(sub,f);
        this.queue.push(['REQ',sub,{...f,since:Math.max(0,(this.newest.get(id)??0)-this.overlap)}]);
      }
      if(this.applicants){const f=applicantFilter(this.buyer);this.live.set(`live-${APPLICANTS}`,f);this.queue.push(['REQ',`live-${APPLICANTS}`,{...f,since:Math.max(0,Math.floor(Date.now()/1000)-this.overlap)}]);}
    };
    ws.addEventListener('open',()=>{
      clearTimeout(timeout);this.status.connected=true;this.retry=1000;this.opened=true;
      // History must work even if AUTH is absent or explicitly refused.
      for(const [id,f] of this.filters)this.page(id,f);
      if(this.applicants)this.pageApplicants();
      subscribe();
      this.updateComplete();this.onChange();
    });
    ws.addEventListener('message',({data})=>{
      let m; try {m=JSON.parse(String(data));}catch{return;}
      if(!Array.isArray(m))return;
      if(m[0]==='AUTH'&&typeof m[1]==='string') {
        const e=signAuth(this.key,this.url,m[1]);this.authId=e.id;send(['AUTH',e]);
      } else if(m[0]==='OK'&&m[1]===this.authId) {
        this.status.authenticated=m[2]===true;
        if(this.status.authenticated) {
          if(this.authClosed.size) {
            if(this.status.error?.startsWith('auth-required:'))this.status.error=null;
            for(const [id,filter] of this.authClosed)this.page(id,filter);
            this.authClosed.clear();
          }
          subscribe();
        }
        this.onChange();
      } else if(m[0]==='EVENT') {
        const p=this.pages.get(m[1]),f=p?.filter??this.live.get(m[1]),e=m[2];
        // Saturation counts every frame the relay spent its page limit on, including events
        // outside the requested window: a relay ignoring since/until must not look complete.
        if(p)p.received++;
        if(!f||!verify(e)||!matches(e,f))return;
        const id=p?m[1]:m[1].slice(5);
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
      } else if(m[0]==='CLOSED') {
        if(m[1]===`live-${APPLICANTS}`){this.live.delete(m[1]);}
        else if(this.live.has(m[1])) {this.live.delete(m[1]);this.status.authenticated=false;}
        if(soft(m[1])&&this.pages.has(m[1])) {this.softFailure(m[1]);this.updateComplete();}
        else if(this.pages.has(m[1])) {
          const auth=typeof m[2]==='string'&&m[2].startsWith('auth-required:');
          if(auth)this.authClosed.set(m[1],this.pages.get(m[1]).filter);
          this.pages.delete(m[1]);
          if(!this.status.error||this.status.error.startsWith('auth-required:'))this.status.error=auth?m[2]:'Relay history subscription refused';
          this.status.historyComplete=false;
        }
        this.onChange();
      }
    });
    ws.addEventListener('error',()=>ws.close());
    ws.addEventListener('close',()=>{
      clearTimeout(timeout);clearInterval(this.queueTimer);clearInterval(this.pollTimer);
      this.status.connected=false;this.status.authenticated=false;this.status.historyComplete=false;this.onChange();
      if(!this.stopped){this.timer=setTimeout(()=>this.start(),this.retry);this.retry=Math.min(this.retry*2,30000);}
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
      this.status.warnings=[...this.metaWarnings];
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
    this.page(id,filter);
    this.live.set(`live-${id}`,filter);this.queue.push(['REQ',`live-${id}`,filter]);
  }
  dropFilter(id) {
    if(!this.filters)return;
    this.filters.delete(id);this.pages.delete(id);this.live.delete(`live-${id}`);this.newest.delete(id);this.authClosed.delete(id);
    this.queue=this.queue.filter(m=>m[1]!==id&&m[1]!==`live-${id}`);
    if(this.ws?.readyState===1)for(const sub of [id,`live-${id}`])this.ws.send(JSON.stringify(['CLOSE',sub]));
  }
  page(id,filter) {this.status.historyComplete=false;this.pages.set(id,{filter,received:0,oldest:Infinity});this.queue.push(['REQ',id,{...filter,limit:PAGE_LIMIT}]);}
  pageApplicants() {const filter=applicantFilter(this.buyer);this.pages.set(APPLICANTS,{filter,received:0,oldest:Infinity});this.queue.push(['REQ',APPLICANTS,{...filter,limit:APPLICANT_LIMIT}]);}
  stop(){this.stopped=true;clearTimeout(this.timer);clearInterval(this.queueTimer);clearInterval(this.pollTimer);this.ws?.close();}
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
      error:this.trade.status.error??(failedRelays.length?`Challenge history incomplete; relay failed: ${failedRelays.join(', ')}`:null),failedRelays,relays};
  }
  start(){this.trade.start();for(const r of this.challenges)r.start();}
  stop(){this.trade.stop();for(const r of this.challenges)r.stop();}
}
