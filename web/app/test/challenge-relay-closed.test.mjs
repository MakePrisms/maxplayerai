// A relay CLOSED must never leave the reader stuck "unavailable" on a healthy relay.
// Live-run evidence: the trade reader lost an authenticated live subscription to CLOSED, kept
// the socket with authenticated=false and never re-AUTHed, resubscribed or reconnected.
import test from 'node:test';
import assert from 'node:assert/strict';
import { schnorr } from '@noble/curves/secp256k1';
import { RelayReader, ChallengeReader, challengeFilters } from '../src/challenge/shared/relay.mjs';
import { EventStore } from '../src/challenge/shared/model.mjs';
const buyer=Buffer.from(schnorr.getPublicKey('01'.padStart(64,'0'))).toString('hex');
const tradeFilters=challengeFilters(buyer).filter(f=>![30341,30342,30343].includes(f.kinds[0]));
const pause=ms=>new Promise(r=>setTimeout(r,ms));
async function until(predicate,ms=5000){const end=Date.now()+ms;while(Date.now()<end){if(predicate())return;await pause(5);}assert.ok(predicate(),'timed out');}

// Fake relay with realistic timing: AUTH OK arrives authDelay ms after the AUTH frame (later than
// the reader's queue step, so early REQs can leave before AUTH OK) and every REQ is answered after
// reqDelay ms. Per connection it sends an AUTH challenge challenge(socket) ms after open (null: never)
// and tracks AUTH. `req` may return a CLOSED reason instead of EOSE; `auth` returns true/false for
// OK, or null to never answer. Every frame in either direction is logged with its time, and
// net.check (if set) runs after each one: it sees the reader state at every relay-visible step.
function relayNet({req=()=>null,auth=()=>true,challenge=()=>0,authDelay=()=>40,reqDelay=()=>3}={}) {
  const net={sockets:[],opened:[],frames:[],check:null};
  net.Socket=class extends EventTarget {
    constructor(url){super();this.url=url;this.readyState=0;this.authed=false;this.log=[];this.n=net.sockets.push(this);
      this.k=net.sockets.filter(s=>s.url===url).length; // connection number for this relay URL
      setTimeout(()=>{this.readyState=1;net.opened.push(performance.now());this.dispatchEvent(new Event('open'));
        const delay=challenge(this);if(delay!==null)setTimeout(()=>{this.challenged=true;this.push(['AUTH',`nonce-${this.n}`]);},delay);},0);}
    push(m,answers){if(this.readyState===1){this.log.push({at:performance.now(),dir:'in',m,answers});this.dispatchEvent(new MessageEvent('message',{data:JSON.stringify(m)}));net.check?.();}}
    send(data){const m=JSON.parse(data),entry={at:performance.now(),dir:'out',m,authed:this.authed};net.frames.push([this.n,m]);this.log.push(entry);net.check?.();
      if(m[0]==='AUTH')setTimeout(()=>{const ok=auth(this);if(ok===null)return;this.authed=ok;this.push(['OK',m[1].id,ok,ok?'':'restricted: denied']);},authDelay(this));
      if(m[0]==='REQ'){const delay=reqDelay(this,m[1],entry.authed);if(delay!==null)setTimeout(()=>this.answer(entry),delay);}}
    // Answer one REQ (a log entry): EOSE, or CLOSED when `req` refuses it. Also used for held REQs.
    answer(entry){const closed=req(this,entry.m[1],entry.authed);this.push(closed?['CLOSED',entry.m[1],closed]:['EOSE',entry.m[1]],entry);}
    reqs(match){return this.log.filter(f=>f.dir==='out'&&f.m[0]==='REQ'&&match(f.m[1]));}
    close(){if(this.readyState===3)return;this.readyState=3;this.dispatchEvent(new Event('close'));}
  };
  net.sent=n=>net.frames.filter(([k])=>k===n).map(([,m])=>m);
  return net;
}
const isLive=(id,sub)=>sub===`live-${id}`||sub.startsWith(`live-${id}~`);
// Readiness invariant for one connection: every trusted filter's latest live REQ left after the
// relay's AUTH OK (when AUTH succeeded) and the relay already answered exactly that REQ with EOSE.
function liveRoundComplete(socket,ids) {
  const ok=socket.log.find(f=>f.dir==='in'&&f.m[0]==='OK'&&f.m[2]===true);
  return ids.every(id=>{
    const last=socket.log.filter(f=>f.dir==='out'&&f.m[0]==='REQ'&&isLive(id,f.m[1])).at(-1);
    if(!last||(ok&&last.at<ok.at))return false;
    // Only the relay's EOSE for that very REQ counts, never one for an earlier REQ with the same id.
    return socket.log.some(f=>f.dir==='in'&&f.m[0]==='EOSE'&&f.answers===last);
  });
}
// Watch readers: whenever one reports historyComplete on a connection where the relay asked for
// AUTH, or while it recovers from a CLOSED, its live round must be complete. Trusted ids are the
// reader's filter ids (these tests have no accepted sellers, so no display-only metadata reads).
function watch(net,readers) {
  const violations=[];
  net.check=()=>{for(const r of readers){
    const s=r.ws;
    if(!r.status.historyComplete||!s||!(s.challenged||r.closedStreak))continue;
    if(!liveRoundComplete(s,[...r.filters.keys()]))violations.push(`${r.url} conn ${s.k} ready at ${s.log.length} frames, streak ${r.closedStreak}, authed ${r.status.authenticated}`);
  }};
  return violations;
}
// The trade-relay reader of ChallengeReader (jobs + member metadata, no roster/applicants).
function tradeReader(t,net,options={}) {
  const reader=new RelayReader({url:'ws://trade',buyer,store:new EventStore(),WebSocketImpl:net.Socket,queueMs:10,pollMs:600000,
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
  const net=relayNet({challenge:()=>null}); // a public relay that never asks for AUTH
  const reader=tradeReader(t,net);
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

// Advisor review r1, finding 1. A challenge relay recovering from a CLOSED must not report ready
// until its fresh round finishes: AUTH OK, then an EOSE on a live REQ for every trusted filter.
// Here AUTH OK on the reconnect is slower than the reader's queue step, and the relay answers any
// live REQ sent before AUTH OK with `restricted:` (not auth-required).
const challengeFilterIds=['30341','30343'];
function splitReader(t,net,options={}) {
  const reader=new ChallengeReader({url:'ws://trade',challengeRelays:['ws://a'],buyer,store:new EventStore(),WebSocketImpl:net.Socket,
    queueMs:10,pollMs:600000,retryMs:20,stableMs:600000,authWaitMs:150,...options});
  reader.start();t.after(()=>reader.stop());
  return reader;
}
const aggregateReady=r=>r.status.connected&&r.status.authenticated&&r.status.historyComplete&&!r.status.error&&!r.status.failedRelays.length;
// Only reconnects refuse early trusted live REQs, so the first connection reaches ready on any reader
// version. The applicant live feed still gets its EOSE: that later EOSE is what re-ran readiness in the review probe.
const restrictedEarlyLive=(socket,id,authed)=>socket.url==='ws://a'&&socket.k>1&&id.startsWith('live-')&&id!=='live-applicants'&&!authed?'restricted: live feed needs AUTH':null;
for(const [name,auth,challenge] of [
  ['finishes late',()=>true,()=>0],
  ['never finishes',s=>s.url!=='ws://a'||s.k===1?true:null,()=>0],
  // The reconnect's challenge arrives after the reader stopped waiting for it (authWaitMs 150), so its
  // trusted live REQs leave anonymously and are refused with restricted: that recovers again.
  ['is challenged late',()=>true,s=>s.url==='ws://a'&&s.k===2?250:0],
])test(`F1: recovering challenge relay whose AUTH ${name} is never ready before a full fresh round (restricted early live CLOSED)`,async t=>{
  const net=relayNet({auth,challenge,req:restrictedEarlyLive,authDelay:s=>s.url==='ws://a'&&s.k>1?120:40});
  const reader=splitReader(t,net),violations=watch(net,[reader.trade,...reader.challenges]);
  const relayA=reader.challenges[0];
  let readyAfterClose=0,closedAt=null;
  net.check=(check=>()=>{check();if(closedAt!==null&&aggregateReady(reader)&&!liveRoundComplete(relayA.ws,challengeFilterIds))readyAfterClose++;})(net.check);
  await until(()=>aggregateReady(reader));
  // The relay drops an authenticated live subscription: the challenge relay starts recovering.
  closedAt=net.sockets.length;
  relayA.ws.push(['CLOSED','live-30341','error: subscription dropped']);
  assert.equal(relayA.closedStreak,1);assert.equal(aggregateReady(reader),false);
  if(name!=='never finishes') {
    await until(()=>relayA.ws.k>=2&&aggregateReady(reader));
    if(name==='is challenged late'){assert.equal(relayA.ws.k,3,'the refused anonymous round recovered once more');assert.match(relayA.lastClosed,/30341: restricted: live feed needs AUTH/);}
    assert.ok(liveRoundComplete(relayA.ws,challengeFilterIds),'ready only after AUTH OK and an EOSE on every trusted live REQ');
  } else {
    // AUTH never completes on the reconnect: the reader retries on fresh connections, never ready.
    await pause(900);
    assert.equal(aggregateReady(reader),false,'never ready without AUTH');
    assert.ok(relayA.ws.k>=3,`retries AUTH on a fresh connection (connection ${relayA.ws.k})`);
    assert.match(relayA.status.error??relayA.lastClosed,/AUTH did not complete/);
  }
  assert.equal(readyAfterClose,0,'aggregate never ready before the fresh round completed');
  assert.deepEqual(violations,[]);
});

// Advisor review r1, finding 1: a trusted live REQ that is no longer installed still needs its EOSE.
// The recovering relay answers every live REQ with auth-required but never sends an AUTH challenge,
// so nothing reinstalls them; history alone must not make the round complete.
test('F1: recovering relay that refuses live REQs with auth-required and never challenges is never ready',async t=>{
  const net=relayNet({challenge:s=>s.n===1?0:null,req:(s,id)=>s.n>1&&id.startsWith('live-')?'auth-required: not authenticated':null});
  let readyWhileRecovering=0;
  const reader=tradeReader(t,net,{onChange:()=>{if(reader?.closedStreak&&reader.status.historyComplete)readyWhileRecovering++;}});
  const violations=watch(net,[reader]);
  await until(()=>ready(reader));
  net.sockets[0].push(['CLOSED','live-3401','error: subscription dropped']);
  await pause(700);
  assert.ok(net.sockets.length>=3,`retried on fresh connections (${net.sockets.length})`);
  assert.equal(readyWhileRecovering,0,'history alone never completes a recovering round');
  assert.match(reader.lastClosed,/auth-required: not authenticated \(AUTH did not complete/);
  assert.deepEqual(violations,[]);
});

// Advisor review r1, finding 2. A replacement live REQ must drop readiness as soon as it is sent and
// restore it only on its own EOSE: an EOSE of the replaced REQ never counts. Realistic timing:
// queueMs 300, AUTH OK 100 ms after AUTH, every REQ answered 30 ms later.
test('F2: recovery reconnect is ready only on the EOSE of each latest live REQ (queueMs 300, slow AUTH OK)',async t=>{
  // The relay refuses any second live REQ for the same filter on a connection, as in the review probe.
  const net=relayNet({authDelay:()=>100,reqDelay:()=>30,req:(socket,id)=>id.startsWith('live-')&&socket.log.filter(f=>f.dir==='out'&&f.m[0]==='REQ'&&f.m[1].split('~')[0]===id.split('~')[0]).length>1?'error: duplicate subscription':null});
  const reader=tradeReader(t,net,{queueMs:300,stableMs:600000,retryMs:50,authWaitMs:2000});
  const violations=watch(net,[reader]);
  await until(()=>ready(reader),20000);
  net.sockets[0].push(['CLOSED','live-3401','error: subscription dropped']);
  await until(()=>net.sockets.length===2&&ready(reader),20000);
  await pause(1500); // any queued replacement REQ would have been sent and refused by now
  assert.ok(ready(reader),'stays ready: one live REQ per filter on the recovered connection');
  assert.equal(net.sockets.length,2);
  assert.deepEqual(violations,[]);
});
test('F2: a replacement live REQ (late AUTH challenge) drops ready when sent and restores it only on its own EOSE',async t=>{
  // Open-read relay: anonymous live REQs get EOSE; the AUTH challenge comes after they were answered.
  const net=relayNet({challenge:()=>3600,authDelay:()=>100,reqDelay:()=>30});
  const reader=tradeReader(t,net,{queueMs:300,authWaitMs:2000});
  const violations=watch(net,[reader]),atSend=[];
  net.check=(check=>()=>{check();const last=net.sockets[0].log.at(-1);if(last.dir==='out'&&last.m[0]==='REQ'&&last.m[1].includes('~'))atSend.push(reader.status.historyComplete);})(net.check);
  await until(()=>reader.status.historyComplete&&net.sockets[0].reqs(id=>id.startsWith('live-')).length===tradeFilters.length&&!net.sockets[0].challenged);
  assert.equal(reader.status.authenticated,false,'anonymous round is ready on history, as before');
  await until(()=>reader.status.authenticated);
  assert.equal(reader.status.historyComplete,false,'not ready once AUTH is expected until the authenticated live REQs are confirmed');
  await until(()=>ready(reader),20000);
  const replacements=net.sent(1).filter(m=>m[0]==='REQ'&&m[1].includes('~'));
  assert.equal(replacements.length,tradeFilters.length,'each anonymous live REQ replaced once under a new id');
  assert.ok(net.sent(1).some(m=>m[0]==='CLOSE'&&m[1]==='live-3401'),'replaced REQ closed');
  assert.ok(atSend.length===tradeFilters.length&&atSend.every(v=>v===false),`not ready when each replacement is sent (${atSend})`);
  // An EOSE for a replaced REQ id never confirms the current one.
  assert.deepEqual(violations,[]);
});

// Advisor review r1, finding 2: an EOSE for a replaced live REQ must never confirm its replacement.
// The relay holds every live REQ until the test answers it; the AUTH challenge comes after the first
// anonymous live REQ left, so AUTH OK replaces it. Buzz still sends EOSE for a replaced REQ's history.
test('F2: an EOSE for a replaced live REQ never confirms its replacement (queueMs 300, AUTH OK after 100 ms)',async t=>{
  const net=relayNet({challenge:()=>null,authDelay:()=>100,reqDelay:(s,id)=>id.startsWith('live-')?null:30});
  const reader=tradeReader(t,net,{queueMs:300,authWaitMs:2000});
  const violations=watch(net,[reader]);
  await until(()=>net.sockets.length===1);const s=net.sockets[0];
  await until(()=>s.reqs(id=>id==='live-3401').length===1,20000);
  s.push(['AUTH','nonce-late']);s.challenged=true;
  await until(()=>reader.status.authenticated);
  await until(()=>s.reqs(id=>isLive('3401',id)).length===2,20000);
  const [old,replacement]=s.reqs(id=>isLive('3401',id));
  assert.equal(old.authed,false);assert.equal(replacement.authed,true);
  assert.equal(reader.status.historyComplete,false,'not ready when the replacement is sent');
  await until(()=>s.reqs(id=>id.startsWith('live-')&&!isLive('3401',id)).length===tradeFilters.length-1,20000);
  for(const f of s.reqs(id=>id.startsWith('live-')&&!isLive('3401',id)))s.answer(f);
  s.answer(old); // stale EOSE for the replaced REQ, after the replacement was sent
  await pause(50);
  assert.equal(reader.status.historyComplete,false,'a stale EOSE of the replaced REQ is not the replacement\'s');
  s.answer(replacement);
  await until(()=>ready(reader));
  assert.deepEqual(violations,[]);
});

// Advisor review r1, nit: the 60 s streak reset must restart if completion drops without a close,
// even when it comes back before the timer would have fired.
test('stable period restarts when a trusted live REQ is unconfirmed without a socket close',async t=>{
  const seller=Buffer.from(schnorr.getPublicKey('02'.padStart(64,'0'))).toString('hex');
  let slowRoster=false;
  const net=relayNet({reqDelay:(s,id)=>slowRoster&&id.startsWith('live-roster-')?150:3});
  const reader=new RelayReader({url:'ws://a',buyer,store:new EventStore(),WebSocketImpl:net.Socket,queueMs:10,pollMs:600000,retryMs:20,stableMs:400,authWaitMs:150,
    filters:challengeFilters(buyer).filter(f=>[30341,30343].includes(f.kinds[0])),roster:true,metadata:false,applicants:false});
  reader.start();t.after(()=>reader.stop());
  await until(()=>ready(reader));
  net.sockets[0].push(['CLOSED','live-30341','error: dropped']);
  await until(()=>net.sockets.length===2&&ready(reader));
  await pause(100);
  // New trusted filter: its live REQ is answered ~150 ms later, so the connection is unconfirmed
  // for a while and ready again well before the first 400 ms stable period would have ended.
  slowRoster=true;reader.setRoster([seller]);
  assert.equal(reader.status.historyComplete,false);
  await until(()=>ready(reader));
  await pause(350); // past the first stable period, inside the restarted one
  assert.equal(reader.closedStreak,1,'streak kept: the connection was not ready for a whole stable period');
  await until(()=>reader.closedStreak===0,2000);
  assert.equal(net.sockets.length,2);
});
