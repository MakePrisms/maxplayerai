// Shared browser/Node projection; no HTTP service or persisted authentication key.
import { EventStore } from '../../web/app/src/challenge/shared/model.mjs';
import { ChallengeReader } from '../../web/app/src/challenge/shared/relay.mjs';
import { writeFileSync, renameSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { resolve } from 'node:path';
// Per-URL socket activity, observed through the reader's own WebSocketImpl option (relay.mjs is
// not modified): when each socket opened/closed and when it last delivered a frame. A connected,
// "ready" relay whose last frame is minutes old is a silent stall (run 2 #4), which readiness
// fields alone cannot show.
export function trackSockets(Base=globalThis.WebSocket,{clock=Date.now}={}) {
  const activity=new Map();
  const entry=url=>{let a=activity.get(url);if(!a)activity.set(url,a={sockets:0,openedAt:null,closedAt:null,lastMessageAt:null,messages:0,socket:null});return a;};
  class Tracked extends Base {
    constructor(url,...rest) {
      super(url,...rest);const a=entry(String(url));a.sockets++;a.socket=this;a.openedAt=null;
      this.addEventListener('open',()=>{a.openedAt=clock();});
      this.addEventListener('message',()=>{a.lastMessageAt=clock();a.messages++;});
      this.addEventListener('close',()=>{a.closedAt=clock();});
    }
  }
  const view=()=>Object.fromEntries([...activity].map(([url,a])=>[url,{sockets:a.sockets,openedAt:a.openedAt,closedAt:a.closedAt,lastMessageAt:a.lastMessageAt,messages:a.messages,readyState:a.socket?.readyState??null}]));
  return {WebSocketImpl:Tracked,activity:view};
}
export function openSnapshot({url,challengeRelays,buyer,onChange=()=>{},WebSocketImpl=globalThis.WebSocket,clock=Date.now}) {
  const store=new EventStore(),sockets=trackSockets(WebSocketImpl,{clock});
  const reader=new ChallengeReader({url,challengeRelays,buyer,store,WebSocketImpl:sockets.WebSocketImpl,onChange:()=>onChange(snapshot())});
  const snapshot=()=>({...store.snapshot(buyer),relay:{...reader.status}});
  reader.start();
  const timer=setInterval(()=>onChange(snapshot()),1000);
  // Diagnostics only: the reader's exposed status plus socket activity. Never used for readiness.
  const diagnostics=()=>({status:reader.status,sockets:sockets.activity()});
  return {snapshot,diagnostics,stop:()=>{clearInterval(timer);reader.stop();}};
}
// E2E consumes an atomic local artifact, exercising the same reader as the page.
if(process.argv[1]&&resolve(process.argv[1])===fileURLToPath(import.meta.url)) {
  const [url,buyer,path,challengeRelays]=process.argv.slice(2);
  if(!url||!buyer||!path)throw Error('Usage: relay-snapshot.mjs relay-url buyer-pubkey output-file [challenge-relays-comma-list]');
  const source=openSnapshot({url,challengeRelays,buyer,onChange:s=>{writeFileSync(path+'.tmp',JSON.stringify(s),{mode:0o600});renameSync(path+'.tmp',path);console.log(JSON.stringify(s));}});
  for(const signal of ['SIGINT','SIGTERM'])process.on(signal,()=>{source.stop();process.exit(0);});
}
