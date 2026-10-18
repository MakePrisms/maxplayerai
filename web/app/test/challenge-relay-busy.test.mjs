// R3-6: the history-page deadline is credited with time this process spent HANDLING relay frames
// (CPU-bound verify/projection after a restart), capped; idle wall time is never credited.
import test from 'node:test';
import assert from 'node:assert/strict';
import { schnorr } from '@noble/curves/secp256k1';
import { RelayReader, relayBusyMs, PAGE_TIMEOUT_MS } from '../src/challenge/shared/relay.mjs';
import { eventId } from '../src/challenge/shared/events.mjs';
const key='01'.padStart(64,'0'),buyer=Buffer.from(schnorr.getPublicKey(key)).toString('hex');
const signed=time=>{const e={pubkey:buyer,kind:30341,created_at:time,tags:[],content:''};e.id=eventId(e);e.sig=Buffer.from(schnorr.sign(e.id,key)).toString('hex');return e;};
const frames=Array.from({length:6},(_,i)=>signed(100+i));
class Socket extends EventTarget {
  static all=[];sent=[];readyState=1;
  constructor(url){super();this.url=url;Socket.all.push(this);}
  send(d){this.sent.push(JSON.parse(d));}
  message(m){this.dispatchEvent(new MessageEvent('message',{data:JSON.stringify(m)}));}
  close(){this.readyState=3;this.dispatchEvent(new Event('close'));}
}
const pause=ms=>new Promise(r=>setTimeout(r,ms));
const spin=ms=>{const end=performance.now()+ms;while(performance.now()<end);};
// Wall time is a controlled clock (Date.now is what the reader's deadline reads), so these tests do
// not depend on scheduler latency under a loaded parallel run. Busy time is REAL handling time
// (performance.now around each frame), produced by a store whose add() burns `cost` ms of CPU per
// frame, like a cold signature check + projection.
function setup(t,{cost=40,pageTimeoutMs=60,pageCreditMaxMs=10000}={}) {
  const clock={now:1e12};t.mock.method(Date,'now',()=>clock.now);
  const store={events:new Map(),add(e){spin(cost);this.events.set(e.id,e);return true;}};
  const reader=new RelayReader({url:'ws://x',buyer,store,WebSocketImpl:Socket,queueMs:1,pollMs:600000,pageTimeoutMs,pageCreditMaxMs,
    filters:[{kinds:[30341],authors:[buyer]}],roster:false,metadata:false,applicants:false});
  reader.start();const socket=Socket.all.at(-1);socket.dispatchEvent(new Event('open'));t.after(()=>reader.stop());
  return {reader,socket,clock};
}
const sent=async r=>{for(let i=0;i<500&&!r.pages.get('30341')?.sentAt;i++)await pause(1);assert.ok(r.pages.get('30341')?.sentAt,'page sent');};
// Lets the reader's queue timer run its deadline check at least a few times.
const ticks=()=>pause(20);
test('R3-6: default page deadline is still 20 s',()=>{assert.equal(PAGE_TIMEOUT_MS,20000);});
test('R3-6: a page whose frames arrived but were CPU-bound to handle does not time out; it completes',async t=>{
  const {reader,socket,clock}=setup(t);await sent(reader);
  const before=relayBusyMs();
  for(const e of frames)socket.message(['EVENT','30341',e]); // ~240 ms of handling
  assert.ok(relayBusyMs()-before>=200,'handling time is counted');
  clock.now+=200; // past the plain 60 ms deadline, within 60 + ~240 ms credit
  await ticks();
  assert.equal(reader.status.error,null,'credited: not timed out');
  socket.message(['EOSE','30341']);
  assert.equal(reader.status.historyComplete,true);assert.equal(reader.store.events.size,6);
});
test('R3-6: a silent relay still times out at the plain deadline (idle time is never credited)',async t=>{
  const {reader,clock}=setup(t);await sent(reader);
  clock.now+=50;await ticks();assert.equal(reader.status.error,null,'within the deadline');
  clock.now+=20;await ticks();
  assert.equal(reader.status.error,'Relay history timed out');assert.equal(reader.status.historyComplete,false);
});
test('R3-6: the credit is capped; past the cap a busy read still fails closed',async t=>{
  const {reader,socket,clock}=setup(t,{pageCreditMaxMs:50});await sent(reader);
  for(const e of frames)socket.message(['EVENT','30341',e]);
  clock.now+=200; // > 60 + 50 cap, < 60 + ~240 uncapped credit
  await ticks();
  assert.equal(reader.status.error,'Relay history timed out');assert.equal(reader.status.historyComplete,false);
});
test('R3-6: busy time spent before a page was sent is not credited to it',async t=>{
  // Another reader burns ~240 ms handling frames first (process-wide busy grows) ...
  const one=setup(t);await sent(one.reader);
  const before=relayBusyMs();for(const e of frames)one.socket.message(['EVENT','30341',e]);
  assert.ok(relayBusyMs()-before>=200);
  // ... then this reader's page leaves: only busy time AFTER its send is credited, so silence times out.
  const two=setup(t);await sent(two.reader);
  two.clock.now+=200;await ticks();
  assert.equal(two.reader.status.error,'Relay history timed out');
});
