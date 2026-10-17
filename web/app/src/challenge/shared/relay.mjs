import { CHALLENGE, MEMBERSHIP, DECISION, OFFER, CLAIM, RESULT, AWARD, ACCEPT, HEARTBEAT, PROFILE } from '../../model/kinds.ts';
import { schnorr } from '@noble/curves/secp256k1';
import { signAuth, verify, mentions, validMembership, validDecision, newer, tag } from './events.mjs';

export function challengeFilters(buyer) {
  return [CHALLENGE,MEMBERSHIP,DECISION,OFFER,CLAIM,AWARD,RESULT,ACCEPT].map(kind =>
    [MEMBERSHIP,CLAIM,RESULT].includes(kind) ? {kinds:[kind],'#p':[buyer]} : {kinds:[kind],authors:[buyer]});
}
const matches = (e,f) => f.kinds.includes(e.kind) && (!f.authors || f.authors.includes(e.pubkey)) && (!f['#p'] || f['#p'].some(pk=>mentions(e,pk)));
// Read-only transport. The only signed frame is NIP-42 AUTH, using an in-memory
// throwaway key (one key per relay connection). Never loads a wallet or publishes EVENT.
export class RelayReader {
  constructor({url,buyer,store,onChange=()=>{},WebSocketImpl=WebSocket,pollMs=15000,queueMs=300,overlap=900,filters=challengeFilters(buyer),onMember=null,key=schnorr.utils.randomPrivateKey()}) {
    Object.assign(this,{url,buyer,store,onChange,WebSocketImpl,pollMs,queueMs,overlap,onMember});
    this.initialFilters=filters; this.members=new Set();
    this.key=key; this.excludedMembers=new Set();
    this.status={connected:false,authenticated:false,historyComplete:false,error:null,warnings:[...this.excludedMembers].map(pk=>`Membership profile cap: excluded seller ${pk}`),excludedMembers:[...this.excludedMembers]};
    this.stopped=false; this.retry=1000;
  }
  start() {
    if(this.stopped)return;
    this.status={connected:false,authenticated:false,historyComplete:false,error:null,warnings:[...this.excludedMembers].map(pk=>`Membership profile cap: excluded seller ${pk}`),excludedMembers:[...this.excludedMembers]};
    this.filters=new Map(this.initialFilters.map(f=>[String(f.kinds[0]),f]));
    for(const pk of this.members)for(const kind of [PROFILE,HEARTBEAT])this.filters.set(`${kind}-${pk}`,{kinds:[kind],authors:[pk]});
    this.authClosed=new Map(); this.pages=new Map(); this.queue=[]; this.newest=new Map(); this.live=new Map();
    const ws=this.ws=new this.WebSocketImpl(this.url);
    const send=frame=>{if(ws.readyState===1)ws.send(JSON.stringify(frame));};
    this.queueTimer=setInterval(()=>{if(this.queue.length&&ws.readyState===1){const frame=this.queue.shift();send(frame);const p=this.pages.get(frame[1]);if(p)p.sentAt=Date.now();}
      if([...this.pages.values()].some(p=>p.sentAt&&Date.now()-p.sentAt>20000)){this.status.error='Relay history timed out';ws.close();}},this.queueMs);
    const timeout=setTimeout(()=>ws.close(),20000);
    this.pollTimer=setInterval(()=>{
      // Finish the entire round before polling again, including member profiles.
      // Otherwise a large history can keep refilling faster than it drains.
      if(this.pages.size)return;
      // Poll also repairs late/backdated writes even when streaming succeeds.
      for(const [id,f] of this.filters) if(!this.pages.has(id))this.page(id,{...f,since:Math.max(0,(this.newest.get(id)??0)-this.overlap)});
    },this.pollMs);
    const subscribe=()=>{
      for(const [id,f] of this.filters) {
        const sub=`live-${id}`; this.live.set(sub,f);
        this.queue.push(['REQ',sub,{...f,since:Math.max(0,(this.newest.get(id)??0)-this.overlap)}]);
      }
    };
    ws.addEventListener('open',()=>{
      clearTimeout(timeout);this.status.connected=true;this.retry=1000;
      // History must work even if AUTH is absent or explicitly refused.
      for(const [id,f] of this.filters)this.page(id,f);
      subscribe();
      this.onChange();
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
        if(!f||!verify(e)||!matches(e,f))return;
        if(p){p.received++;p.oldest=Math.min(p.oldest,e.created_at);}
        const id=p?m[1]:m[1].slice(5);
        this.newest.set(id,Math.max(this.newest.get(id)??0,e.created_at));
        if(e.kind===MEMBERSHIP&&!validMembership(e,{pubkey:this.buyer}))return;
        if(p)p.events.push(e);
        const added=this.store.add(e);
        if(added){if(!this.pages.size)this.refreshMembers();this.onChange();}
      } else if(m[0]==='EOSE'&&this.pages.has(m[1])) {
        const p=this.pages.get(m[1]);this.pages.delete(m[1]);
        if(p.received>=500 && p.events.length<500) {
          if(p.oldest>0)this.page(m[1],{...p.filter,until:p.oldest-1});
        } else if(p.events.length>=500) {
          const oldest=p.oldest;
          if(p.filter.until===oldest)this.status.error='History truncated: 500+ events in one second';
          else this.page(m[1],{...p.filter,until:oldest});
        }
        if(!this.pages.has(m[1]))send(['CLOSE',m[1]]);
        this.refreshMembers();
        this.status.historyComplete=this.pages.size===0&&!this.status.error;this.onChange();
      } else if(m[0]==='CLOSED') {
        if(this.live.has(m[1])) {this.live.delete(m[1]);this.status.authenticated=false;}
        if(this.pages.has(m[1])) {
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
  refreshMembers() {
    if(this.onMember){this.onMember();return;}
    if([...this.pages.keys()].some(id=>[String(MEMBERSHIP),String(DECISION),String(CHALLENGE)].includes(id)))return;
    this.selectMembers();
  }
  selectMembers() {
    const apps=new Map(),decisions=new Map(),c={pubkey:this.buyer};
    for(const e of this.store.events.values()) {
      if(validMembership(e,c)&&newer(e,apps.get(e.pubkey)))apps.set(e.pubkey,e);
      if(validDecision(e,c)&&newer(e,decisions.get(tag(e,'p'))))decisions.set(tag(e,'p'),e);
    }
    const keys=[...apps.keys()].sort((a,b)=>Number(tag(decisions.get(b)??{tags:[]},'decision')==='accepted')-Number(tag(decisions.get(a)??{tags:[]},'decision')==='accepted')||a.localeCompare(b));
    const selected=new Set(keys.slice(0,256));
    // This budget limits metadata requests only, never membership or assignment.
    this.status.warnings=keys.length>256?['Membership profile cap: some profiles/heartbeats unavailable']:[];
    for(const pk of this.members)if(!selected.has(pk)) {
      this.members.delete(pk);
      for(const kind of [PROFILE,HEARTBEAT]) {
        const id=`${kind}-${pk}`;
        this.filters?.delete(id);this.pages?.delete(id);this.live?.delete(`live-${id}`);
        if(this.queue)this.queue=this.queue.filter(m=>m[1]!==id&&m[1]!==`live-${id}`);
        if(this.ws?.readyState===1)for(const sub of [id,`live-${id}`])this.ws.send(JSON.stringify(['CLOSE',sub]));
      }
    }
    for(const pk of selected)this.addMember(pk);
  }
  addMember(pk) {
    if(!this.members.has(pk)&&this.members.size>=256)return false;
    this.members.add(pk);
    if(!this.filters)return true;
    for(const kind of [PROFILE,HEARTBEAT]) {
      const id=`${kind}-${pk}`,filter={kinds:[kind],authors:[pk]};
      if(this.filters.has(id))continue;
      this.filters.set(id,filter);this.page(id,filter);
      this.live.set(`live-${id}`,filter);this.queue.push(['REQ',`live-${id}`,filter]);
    }
    return true;
  }
  page(id,filter) {this.status.historyComplete=false;this.pages.set(id,{filter,events:[],received:0,oldest:Infinity});this.queue.push(['REQ',id,{...filter,limit:500}]);}
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
export class ChallengeReader {
  constructor({url,challengeRelays=DEFAULT_CHALLENGE_RELAYS,buyer,store,onChange=()=>{},...options}) {
    const filters=challengeFilters(buyer);
    const changed=()=>{
      if(this.challenges?.every(r=>r.status.historyComplete))this.trade.selectMembers();
      onChange();
    };
    const common={...options,buyer,store,onChange:changed};
    this.trade=new RelayReader({...common,url,onMember:()=>{},filters:filters.filter(f=>![CHALLENGE,MEMBERSHIP,DECISION].includes(f.kinds[0]))});
    this.challenges=parseChallengeRelays(challengeRelays).map(url=>new RelayReader({...common,url,
      filters:filters.filter(f=>[CHALLENGE,MEMBERSHIP,DECISION].includes(f.kinds[0])),
      onMember:()=>{}}));
  }
  get status() {
    const relays=[{role:'trade',url:this.trade.url,...this.trade.status},...this.challenges.map(r=>({role:'challenge',url:r.url,...r.status}))];
    const available=this.challenges.filter(r=>r.status.connected&&!r.status.error);
    const ready=available.length===this.challenges.length&&available.every(r=>r.status.historyComplete);
    const failedRelays=relays.filter(r=>!r.connected||r.error).map(r=>r.url);
    return {connected:this.trade.status.connected&&available.length>0,
      authenticated:this.trade.status.authenticated,
      historyComplete:this.trade.status.historyComplete&&ready,
      warnings:[...new Set(relays.flatMap(r=>r.warnings))],excludedMembers:[...new Set(relays.flatMap(r=>r.excludedMembers))],
      error:this.trade.status.error??(failedRelays.length?`Challenge history incomplete; relay failed: ${failedRelays.join(', ')}`:null),failedRelays,relays};
  }
  start(){this.trade.start();for(const r of this.challenges)r.start();}
  stop(){this.trade.stop();for(const r of this.challenges)r.stop();}
}
