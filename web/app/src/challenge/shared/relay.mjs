import { CHALLENGE, MEMBERSHIP, DECISION, OFFER, CLAIM, RESULT, AWARD, ACCEPT, HEARTBEAT, PROFILE } from '../../model/kinds.ts';
import { schnorr } from '@noble/curves/secp256k1';
import { signAuth, verify, mentions } from './events.mjs';

export function challengeFilters(buyer) {
  return [CHALLENGE,MEMBERSHIP,DECISION,OFFER,CLAIM,AWARD,RESULT,ACCEPT].map(kind =>
    [MEMBERSHIP,CLAIM,RESULT].includes(kind) ? {kinds:[kind],'#p':[buyer]} : {kinds:[kind],authors:[buyer]});
}
const matches = (e,f) => f.kinds.includes(e.kind) && (!f.authors || f.authors.includes(e.pubkey)) && (!f['#p'] || f['#p'].some(pk=>mentions(e,pk)));
// Read-only transport. The only signed frame is NIP-42 AUTH, using an in-memory
// throwaway key (one reader per tab). Never loads a wallet or publishes EVENT.
export class RelayReader {
  constructor({url,buyer,store,onChange=()=>{},WebSocketImpl=WebSocket,pollMs=15000,queueMs=300,overlap=900}) {
    Object.assign(this,{url,buyer,store,onChange,WebSocketImpl,pollMs,queueMs,overlap});
    this.key=schnorr.utils.randomPrivateKey();
    this.status={connected:false,authenticated:false,historyComplete:false,error:null};
    this.stopped=false; this.retry=1000;
  }
  start() {
    if(this.stopped)return;
    this.status={connected:false,authenticated:false,historyComplete:false,error:null};
    this.filters=new Map(challengeFilters(this.buyer).map(f=>[String(f.kinds[0]),f]));
    this.pages=new Map(); this.queue=[]; this.newest=new Map(); this.live=new Map();
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
      this.onChange();
    });
    ws.addEventListener('message',({data})=>{
      let m; try {m=JSON.parse(String(data));}catch{return;}
      if(!Array.isArray(m))return;
      if(m[0]==='AUTH'&&typeof m[1]==='string') {
        const e=signAuth(this.key,this.url,m[1]);this.authId=e.id;send(['AUTH',e]);
      } else if(m[0]==='OK'&&m[1]===this.authId) {
        this.status.authenticated=m[2]===true;
        if(this.status.authenticated)subscribe();
        this.onChange();
      } else if(m[0]==='EVENT') {
        const p=this.pages.get(m[1]),f=p?.filter??this.live.get(m[1]),e=m[2];
        if(!f||!verify(e)||!matches(e,f))return;
        if(p)p.events.push(e);
        const id=p?m[1]:m[1].slice(5);
        this.newest.set(id,Math.max(this.newest.get(id)??0,e.created_at));
        const added=this.store.add(e);
        if(e.kind===MEMBERSHIP)for(const kind of [PROFILE,HEARTBEAT]) {
          const profile=`${kind}-${e.pubkey}`;
          if(!this.filters.has(profile)) {
            const filter={kinds:[kind],authors:[e.pubkey]};this.filters.set(profile,filter);this.page(profile,filter);
            if(this.status.authenticated){this.live.set(`live-${profile}`,filter);this.queue.push(['REQ',`live-${profile}`,filter]);}
          }
        }
        if(added)this.onChange();
      } else if(m[0]==='EOSE'&&this.pages.has(m[1])) {
        const p=this.pages.get(m[1]);this.pages.delete(m[1]);
        if(p.events.length>=500) {
          const oldest=Math.min(...p.events.map(e=>e.created_at));
          if(p.filter.until===oldest)this.status.error='History truncated: 500+ events in one second';
          else this.page(m[1],{...p.filter,until:oldest});
        }
        if(!this.pages.has(m[1]))send(['CLOSE',m[1]]);
        this.status.historyComplete=this.pages.size===0&&!this.status.error;this.onChange();
      } else if(m[0]==='CLOSED') {
        if(this.live.has(m[1])) {this.live.delete(m[1]);this.status.authenticated=false;}
        if(this.pages.has(m[1])) {this.pages.delete(m[1]);this.status.error='Relay history subscription refused';this.status.historyComplete=false;}
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
  page(id,filter) {this.status.historyComplete=false;this.pages.set(id,{filter,events:[]});this.queue.push(['REQ',id,{...filter,limit:500}]);}
  stop(){this.stopped=true;clearTimeout(this.timer);clearInterval(this.queueTimer);clearInterval(this.pollTimer);this.ws?.close();}
}
