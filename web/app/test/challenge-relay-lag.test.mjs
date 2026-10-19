// P-3 (run-start feedback): a busy event loop must not turn into fake relay failures. While this
// thread was blocked (projection, cold signature checks), frames the relay had already sent sat
// unread, then the deadline timer ran before them and declared a healthy relay dead -> recover() ->
// reconnect -> full history re-read -> more CPU. The reader now measures its own lag by timer drift
// and credits it to pending deadlines (capped); a silent relay still fails within a bound.
import test from 'node:test';
import assert from 'node:assert/strict';
import { schnorr } from '@noble/curves/secp256k1';
import { RelayReader, lagTimeout, LAG_THRESHOLD_MS } from '../src/challenge/shared/relay.mjs';
import { EventStore } from '../src/challenge/shared/model.mjs';
const key='01'.padStart(64,'0'),buyer=Buffer.from(schnorr.getPublicKey(key)).toString('hex');
class Socket extends EventTarget {
  static all=[];sent=[];readyState=1;
  constructor(url){super();this.url=url;Socket.all.push(this);}
  send(d){this.sent.push(JSON.parse(d));}
  message(m){this.dispatchEvent(new MessageEvent('message',{data:JSON.stringify(m)}));}
  close(){this.readyState=3;this.dispatchEvent(new Event('close'));}
}
const pause=ms=>new Promise(r=>setTimeout(r,ms));
const spin=ms=>{const end=performance.now()+ms;while(performance.now()<end);};
async function until(predicate,ms=5000){const end=performance.now()+ms;while(performance.now()<end){if(predicate())return;await pause(2);}assert.ok(predicate(),'timed out waiting');}
// Blocks this thread for `ms`, then delivers what arrived meanwhile the way a real socket does: in a
// later event-loop phase than the timers that fell due during the block (Node: check after timers;
// a browser runs the overdue timer task first too). The block starts in a check phase, so the next
// loop iteration runs every overdue timer (the reader's deadline checks) BEFORE `deliver`.
const blockThen=(ms,deliver=()=>{})=>new Promise(resolve=>setImmediate(()=>{spin(ms);setImmediate(()=>{deliver();resolve();});}));
function setup(t,{strict=false,open=true,...options}={}) {
  const reader=new RelayReader({url:'ws://x',buyer,store:new EventStore(),WebSocketImpl:Socket,queueMs:5,pollMs:600000,retryMs:600000,
    pageTimeoutMs:200,pageCreditMaxMs:5000,lagThresholdMs:50,filters:[{kinds:[30341],authors:[buyer]}],roster:false,metadata:false,applicants:false,...options});
  if(strict)reader.closedStreak=1; // recovering after a CLOSED: live EOSE is part of readiness
  reader.start();const socket=Socket.all.at(-1);t.after(()=>reader.stop());
  if(open)socket.dispatchEvent(new Event('open'));
  return {reader,socket,sockets:Socket.all.length};
}
const pageSent=r=>until(()=>r.pages.get('30341')?.sentAt>0);
test('P-3: default lag threshold is 250 ms',()=>assert.equal(LAG_THRESHOLD_MS,250));
test('P-3: a thread blocked 4x past the history deadline does not fail a relay whose EOSE already arrived; no reconnect',async t=>{
  const {reader,socket,sockets}=setup(t);await pageSent(reader);await pause(20);
  await blockThen(800,()=>socket.message(['EOSE','30341']));
  assert.equal(reader.status.error,null,'not "Relay history timed out"');
  assert.equal(reader.status.historyComplete,true);assert.equal(reader.status.connected,true);
  assert.equal(Socket.all.length,sockets,'no reconnect');assert.equal(reader.closedStreak,0);assert.equal(socket.readyState,1);
});
test('P-3: in a strict round, a blocked thread does not fail a live REQ whose EOSE already arrived; no recover()',async t=>{
  const {reader,socket,sockets}=setup(t,{strict:true});await pageSent(reader);
  socket.message(['EOSE','30341']);
  await until(()=>reader.live.get('live-30341')?.sentAt>0);await pause(20);
  assert.equal(reader.status.historyComplete,false,'waiting for the live EOSE');
  await blockThen(800,()=>socket.message(['EOSE','live-30341']));
  assert.equal(reader.status.error,null,'not "Relay live subscription timed out"');
  assert.equal(reader.status.historyComplete,true);
  assert.equal(Socket.all.length,sockets,'no reconnect');assert.equal(reader.closedStreak,1,'no recover()');
});
test('P-3: a socket that opened while the thread was blocked past connectMs is not torn down',async t=>{
  const {reader,socket,sockets}=setup(t,{open:false,connectMs:200});socket.readyState=0;await pause(20);
  await blockThen(800,()=>{socket.readyState=1;socket.dispatchEvent(new Event('open'));});
  assert.equal(reader.status.connected,true);assert.equal(socket.readyState,1,'not closed by the connect timeout');
  assert.equal(Socket.all.length,sockets,'no reconnect');
});
test('P-3: a silent relay on an idle thread still times out at the plain deadline',async t=>{
  const {reader}=setup(t,{pageCreditMaxMs:1000});await pageSent(reader);const sentAt=reader.pages.get('30341').sentAt;
  await until(()=>reader.status.error);
  assert.equal(reader.status.error,'Relay history timed out');
  // Idle: no credit beyond scheduler noise; the bound is deadline + cap either way.
  assert.ok(Date.now()-sentAt<200+1000,`failed after ${Date.now()-sentAt} ms`);
});
test('P-3: a relay that stays silent while the thread is blocked over and over still fails within deadline + credit cap',async t=>{
  const {reader}=setup(t,{pageCreditMaxMs:600});await pageSent(reader);const sentAt=reader.pages.get('30341').sentAt;
  // Every block is lag; without the cap each one would push the deadline out again forever.
  while(!reader.status.error&&Date.now()-sentAt<5000)await blockThen(150);
  assert.equal(reader.status.error,'Relay history timed out');
  const elapsed=Date.now()-sentAt;assert.ok(elapsed<=200+600+150+400,`failed after ${elapsed} ms`);
});
test('P-3: the same bound holds for a silent live REQ in a strict round',async t=>{
  const {reader,socket}=setup(t,{strict:true,pageCreditMaxMs:600});await pageSent(reader);socket.message(['EOSE','30341']);
  await until(()=>reader.live.get('live-30341')?.sentAt>0);const sentAt=reader.live.get('live-30341').sentAt;
  while(!reader.status.error&&Date.now()-sentAt<5000)await blockThen(150);
  assert.match(reader.status.error??'',/Relay live subscription timed out/);
  const elapsed=Date.now()-sentAt;assert.ok(elapsed<=200+600+150+400,`failed after ${elapsed} ms`);
});
test('P-3 lagTimeout: fires on time on an idle thread',async()=>{
  const t0=performance.now();let fired=0;lagTimeout(()=>fired=performance.now(),50,{thresholdMs:30,maxCreditMs:400});
  await until(()=>fired);assert.ok(fired-t0>=45&&fired-t0<50+300,`fired after ${fired-t0} ms`);
});
test('P-3 lagTimeout: late only because this thread was blocked, it lets queued frames run first, then fires',async()=>{
  // Like the connect timeout or the AUTH wait: the answer arrived during the block and cancels it.
  let fired=false,h=lagTimeout(()=>fired=true,100,{thresholdMs:30,maxCreditMs:1000});
  await blockThen(400,()=>h.clear());
  assert.equal(fired,false,'not fired by our own lag before the frame was handled');await pause(50);assert.equal(fired,false,'cancelled');
  // With nothing arriving, it fires on the next turn (the lost time is credited, not doubled).
  const t0=performance.now();let at=0;h=lagTimeout(()=>at=performance.now(),100,{thresholdMs:30,maxCreditMs:1000});
  await blockThen(400);assert.equal(at,0);
  await until(()=>at);assert.ok(at-t0<400+300,`fired after ${at-t0} ms`);
});
test('P-3 lagTimeout: repeated blocks cannot postpone it past ms + maxCreditMs; clear() cancels it',async()=>{
  const t0=performance.now();let fired=0;lagTimeout(()=>fired=performance.now(),100,{thresholdMs:30,maxCreditMs:400});
  while(!fired&&performance.now()-t0<5000)await blockThen(150);
  assert.ok(fired,'fired');assert.ok(fired-t0<=100+400+150+400,`fired after ${fired-t0} ms`);
  let cleared=false;const h=lagTimeout(()=>cleared=true,20);h.clear();await pause(60);assert.equal(cleared,false);
});
