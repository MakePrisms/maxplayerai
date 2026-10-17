// A relay CLOSED must never leave the reader stuck "unavailable" on a healthy relay.
// Live-run evidence: the trade reader lost an authenticated live subscription to CLOSED, kept
// the socket with authenticated=false and never re-AUTHed, resubscribed or reconnected.
import test from 'node:test';
import assert from 'node:assert/strict';
import { schnorr } from '@noble/curves/secp256k1';
import { RelayReader, challengeFilters } from '../src/challenge/shared/relay.mjs';
import { EventStore } from '../src/challenge/shared/model.mjs';
const buyer=Buffer.from(schnorr.getPublicKey('01'.padStart(64,'0'))).toString('hex');
const tradeFilters=challengeFilters(buyer).filter(f=>![30341,30342,30343].includes(f.kinds[0]));
const pause=ms=>new Promise(r=>setTimeout(r,ms));
async function until(predicate,ms=5000){const end=Date.now()+ms;while(Date.now()<end){if(predicate())return;await pause(5);}assert.ok(predicate(),'timed out');}

// Fake relay: challenges AUTH on connect and tracks AUTH per connection. `req` may return a
// CLOSED reason instead of EOSE; `auth` returns true/false for OK, or null to never answer.
function relayNet({req=()=>null,auth=()=>true}={}) {
  const net={sockets:[],opened:[],frames:[]};
  net.Socket=class extends EventTarget {
    constructor(){super();this.readyState=0;this.authed=false;this.n=net.sockets.push(this);
      setTimeout(()=>{this.readyState=1;net.opened.push(performance.now());this.dispatchEvent(new Event('open'));this.push(['AUTH',`nonce-${this.n}`]);},0);}
    push(m){if(this.readyState===1)this.dispatchEvent(new MessageEvent('message',{data:JSON.stringify(m)}));}
    send(data){const m=JSON.parse(data);net.frames.push([this.n,m]);
      if(m[0]==='AUTH')setTimeout(()=>{const ok=auth(this);if(ok===null)return;this.authed=ok;this.push(['OK',m[1].id,ok,ok?'':'restricted: denied']);},0);
      if(m[0]==='REQ')setTimeout(()=>{const closed=req(this,m[1]);this.push(closed?['CLOSED',m[1],closed]:['EOSE',m[1]]);},0);}
    close(){if(this.readyState===3)return;this.readyState=3;this.dispatchEvent(new Event('close'));}
  };
  net.sent=n=>net.frames.filter(([k])=>k===n).map(([,m])=>m);
  return net;
}
// The trade-relay reader of ChallengeReader (jobs + member metadata, no roster/applicants).
function tradeReader(t,net,options={}) {
  const reader=new RelayReader({url:'ws://trade',buyer,store:new EventStore(),WebSocketImpl:net.Socket,queueMs:1,pollMs:600000,
    retryMs:20,stableMs:100,authWaitMs:100,filters:tradeFilters,roster:false,metadata:true,applicants:false,...options});
  reader.start();t.after(()=>reader.stop());
  return reader;
}
// relayReady() in scripts/challenges/coordinator.mjs, for one reader.
const ready=r=>r.status.connected&&r.status.authenticated&&r.status.historyComplete&&!r.status.error;

test('CLOSED of an authenticated live subscription after ready: reconnect, re-AUTH, full round, ready again; reason surfaced',async t=>{
  const net=relayNet();
  const reader=tradeReader(t,net);
  await until(()=>ready(reader));
  assert.equal(net.sockets.length,1);
  net.sockets[0].push(['CLOSED','live-3401','error: subscription dropped by relay']);
  assert.equal(reader.status.historyComplete,false,'fail closed immediately');
  assert.match(reader.status.error??'',/live subscription 3401: error: subscription dropped by relay/,'CLOSED reason surfaced as error');
  await until(()=>net.sockets.length===2&&ready(reader));
  assert.ok(reader.status.warnings.some(w=>/^Recovering after relay CLOSED .*subscription dropped by relay/.test(w)),'reason stays visible while recovering');
  const second=net.sent(2);
  assert.ok(second.some(m=>m[0]==='AUTH'),'re-AUTH on the fresh connection');
  for(const f of tradeFilters) {
    const id=String(f.kinds[0]);
    assert.ok(second.some(m=>m[0]==='REQ'&&m[1]===id&&m[2].limit===500&&m[2].since===undefined),`full history re-read for ${id}`);
    assert.ok(second.some(m=>m[0]==='REQ'&&m[1]===`live-${id}`),`live ${id} resubscribed`);
  }
  // A recovered connection that stays ready resets the backoff and clears the recovery note.
  await until(()=>reader.closedStreak===0);
  assert.ok(!reader.status.warnings.some(w=>w.startsWith('Recovering')));
  assert.equal(reader.retry,20);assert.equal(net.sockets.length,2,'no extra reconnect');
});

test('a relay that CLOSEs every authenticated live subscription: bounded reconnects with growing backoff, never ready',async t=>{
  const net=relayNet({req:(socket,id)=>id.startsWith('live-')&&socket.authed?'error: live subscriptions disabled':null});
  let readyWhileRecovering=0;
  const reader=tradeReader(t,net,{onChange:()=>{if(reader?.closedStreak&&reader.status.historyComplete)readyWhileRecovering++;}});
  await pause(1300);
  const opens=net.opened.length;
  // retryMs 20 doubling: reconnects at ≥20, 40, 80, 160, 320, 640 ms. A tight loop would make hundreds.
  assert.ok(opens>=4,`keeps retrying on fresh connections (opened ${opens})`);
  assert.ok(opens<=8,`bounded by backoff (opened ${opens})`);
  const gaps=net.opened.slice(1).map((at,i)=>at-net.opened[i]);
  gaps.forEach((gap,i)=>assert.ok(gap>=20*2**i*0.9,`reconnect ${i+1} waited ${gap.toFixed(1)}ms, backoff ${20*2**i}ms`));
  assert.equal(readyWhileRecovering,0,'historyComplete never true while every live subscription is refused');
  assert.equal(reader.status.historyComplete,false);
  assert.ok(reader.status.error?.includes('live subscriptions disabled')||reader.status.warnings.some(w=>w.includes('live subscriptions disabled')),'reason surfaced');
  // Trusted history itself is still read in full on every fresh connection.
  for(let n=1;n<=opens;n++)assert.ok(net.sent(n).some(m=>m[0]==='REQ'&&m[1]==='3401'&&m[2].limit===500),`connection ${n} re-read history`);
});

test('auth-required CLOSED of a live subscription after AUTH (session expired) re-AUTHs on a fresh connection',async t=>{
  const net=relayNet();
  const reader=tradeReader(t,net);
  await until(()=>ready(reader));
  net.sockets[0].authed=false;
  net.sockets[0].push(['CLOSED','live-3402','auth-required: session expired']);
  assert.match(reader.status.error??'',/auth-required: session expired/);
  await until(()=>net.sockets.length===2&&ready(reader));
  assert.ok(net.sent(2).some(m=>m[0]==='AUTH'),'fresh AUTH');
});

test('auth-required history refusal is re-read in place after AUTH OK, without reconnecting',async t=>{
  // Buzz without open read: every REQ before AUTH gets auth-required CLOSED.
  const net=relayNet({req:socket=>socket.authed?null:'auth-required: not authenticated'});
  const reader=tradeReader(t,net);
  await until(()=>ready(reader));
  await pause(150); // longer than authWaitMs: the AUTH timer was cancelled by OK
  assert.equal(net.sockets.length,1);assert.equal(reader.status.error,null);assert.ok(ready(reader));
});

for(const [name,auth,reason] of [
  ['never completes',socket=>socket.n===1?null:true,/AUTH did not complete/],
  ['is refused',socket=>socket.n>1,/AUTH refused: restricted: denied/],
])test(`auth-required history refusal where AUTH ${name}: fresh connection retries AUTH`,async t=>{
  const net=relayNet({req:socket=>socket.authed?null:'auth-required: not authenticated',auth});
  const reasons=[];
  const reader=tradeReader(t,net,{onChange:()=>{if(reader?.status.error)reasons.push(reader.status.error);}});
  await until(()=>net.sockets.length===2&&ready(reader));
  assert.ok(reasons.some(r=>reason.test(r)),`reason surfaced: ${reasons.at(-1)}`);
  assert.ok(net.sent(2).some(m=>m[0]==='AUTH'));
});

test('display-only and anonymous live CLOSEDs never reconnect; our own CLOSE acknowledgement is ignored',async t=>{
  const net=relayNet({auth:()=>null}); // anonymous for the whole test: AUTH never completes
  const reader=tradeReader(t,net,{authWaitMs:100000});
  await until(()=>reader.status.historyComplete);
  const s=net.sockets[0];
  s.push(['CLOSED','live-3401','restricted: anonymous readers get no live feed']);
  reader.setRoster([buyer]);await until(()=>reader.status.historyComplete);
  s.push(['CLOSED',`live-0-${buyer}`,'error: too many subscriptions']);
  reader.setRoster([]);await pause(20);s.push(['CLOSED',`live-0-${buyer}`,'']);
  await pause(50);
  assert.equal(net.sockets.length,1);assert.equal(reader.status.error,null);assert.equal(reader.status.historyComplete,true);
  assert.ok(reader.status.warnings.some(w=>/3401 closed by relay \(restricted: anonymous/.test(w)));
  assert.ok(reader.status.warnings.some(w=>/Profile\/heartbeat live feed closed by relay \(error: too many/.test(w)));
});
