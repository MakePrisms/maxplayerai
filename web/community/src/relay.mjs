import { signAuth } from './events.mjs';
// Each kind has an independent history cursor; live subscription is installed first.
export class RelayReader {
  constructor({url,key,buyer,store,onChange=()=>{},WebSocketImpl=WebSocket}) { Object.assign(this,{url,key,buyer,store,onChange,WebSocketImpl}); this.status={connected:false,authenticated:false,historyComplete:false,error:null}; this.stopped=false; this.retry=1000; }
  start() {
    if(this.stopped)return;
    this.status={connected:false,authenticated:false,historyComplete:false,error:null}; this.pages=new Map(); this.pending=0; this.profileAuthors=new Set(); this.queue=[]; clearInterval(this.queueTimer); this.queueTimer=setInterval(()=>{if(ws.readyState===undefined||ws.readyState===1){const frame=this.queue.shift();if(frame)ws.send(JSON.stringify(frame));}},300);
    const ws=this.ws=new this.WebSocketImpl(this.url);
    const timeout=setTimeout(()=>ws.close(),20000);
    ws.addEventListener('open',()=>{this.status.connected=true;this.onChange();});
    ws.addEventListener('message',({data})=>{
      try {
        const m=JSON.parse(String(data));
        if(m[0]==='AUTH') {const e=signAuth(this.key,this.url,m[1]);this.authId=e.id;ws.send(JSON.stringify(['AUTH',e]));}
        else if(m[0]==='OK'&&m[1]===this.authId) {
          if(!m[2]) {this.status.error='Relay authentication refused';ws.close();return;}
          clearTimeout(timeout);this.status.authenticated=true;this.retry=1000;
          const filters=[];
          for(const kind of [30341,30342,30343,3401,3402,3405,3403,3406]) {
            const filter={kinds:[kind]};
            if([30341,30343,3401,3405,3406].includes(kind))filter.authors=[this.buyer];
            if([30342,3402,3403].includes(kind))filter['#p']=[this.buyer];
            filters.push({...filter,since:Math.floor(Date.now()/1000)});
            this.pending++;this.page(`history-${kind}`,filter);
          }
          this.queue.unshift(['REQ','live',...filters]);
          console.log('relay authenticated (NIP-42); read subscriptions installed');this.onChange();
        } else if(m[0]==='EVENT') {
          const page=this.pages.get(m[1]);if(page)page.events.push(m[2]);
          const added=this.store.add(m[2]);
          const e=this.store.events.get(m[2]?.id);
          if(e&&e.kind===30342&&!this.profileAuthors.has(e.pubkey)) {
            this.profileAuthors.add(e.pubkey);this.status.historyComplete=false;
            for(const kind of [0,30340]){const filter={kinds:[kind],authors:[e.pubkey]};this.pending++;this.page(`profile-${kind}-${e.pubkey}`,filter);}
          }
          if(added)this.onChange();
        } else if(m[0]==='EOSE'&&this.pages.has(m[1])) {
          const p=this.pages.get(m[1]);this.pages.delete(m[1]);
          if(p.events.length>=500) {
            const oldest=Math.min(...p.events.map(e=>e.created_at));
            // Overlap the boundary second to avoid dropping equal-timestamp events.
            if(p.filter.until===oldest) {this.status.error='History truncated: 500+ events in one second';this.pending--;}
            else this.page(m[1],{...p.filter,until:oldest});
          } else {this.pending--;if(!m[1].startsWith('profile-'))this.queue.push(['CLOSE',m[1]]);}
          this.status.historyComplete=this.pending===0&&!this.status.error;this.onChange();
        } else if(m[0]==='CLOSED'&&(m[1]==='live'||this.pages.has(m[1])||m[1]?.startsWith('profile-'))) {this.status.error=`Subscription closed: ${String(m[2]).slice(0,160)}`;console.warn(this.status.error);this.onChange();ws.close();}
      } catch { /* malformed relay frames are never trusted */ }
    });
    ws.addEventListener('error',()=>{this.status.error??='Relay connection error';console.warn(this.status.error);ws.close();});
    ws.addEventListener('close',()=>{clearTimeout(timeout);clearInterval(this.queueTimer);this.status.connected=false;this.status.authenticated=false;this.status.historyComplete=false;this.onChange();if(!this.stopped){this.timer=setTimeout(()=>this.start(),this.retry);this.retry=Math.min(this.retry*2,30000);}});
  }
  page(id,filter) {this.pages.set(id,{filter,events:[]});this.queue.push(['REQ',id,{...filter,limit:500}]);}
  stop(){this.stopped=true;clearTimeout(this.timer);clearInterval(this.queueTimer);this.ws?.close();}
}
