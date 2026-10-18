// Run 2 #13: challenge relay quorum (default 2 of 3) for the shared reader, our trade relay always
// required, the degraded warning, and the Node 22 close-while-CONNECTING recursion in relay.mjs.
import test from 'node:test';
import assert from 'node:assert/strict';
import net from 'node:net';
import { schnorr } from '@noble/curves/secp256k1';
import { RelayReader, ChallengeReader, parseChallengeQuorum, DEFAULT_CHALLENGE_QUORUM } from '../src/challenge/shared/relay.mjs';
import { EventStore } from '../src/challenge/shared/model.mjs';
import { connectionText } from '../src/challenge/live.ts';
import { relayReady } from '../../../scripts/challenges/coordinator.mjs';
const buyer=Buffer.from(schnorr.getPublicKey('01'.padStart(64,'0'))).toString('hex');
const pause=ms=>new Promise(r=>setTimeout(r,ms));
async function until(predicate,ms=4000) {const end=Date.now()+ms;while(Date.now()<end){if(predicate())return;await pause(5);}assert.ok(predicate(),'timed out');}
class Socket extends EventTarget {
  sent=[];readyState=1;
  constructor(url){super();this.url=url;}
  send(d){this.sent.push(JSON.parse(d));}
  message(m){this.dispatchEvent(new MessageEvent('message',{data:JSON.stringify(m)}));}
  close(){if(this.readyState===3)return;this.readyState=3;this.dispatchEvent(new Event('close'));}
}
function setup(t,{relays=['ws://a','ws://b','ws://c'],quorum}={}) {
  const reader=new ChallengeReader({url:'ws://trade',challengeRelays:relays,quorum,buyer,store:new EventStore(),WebSocketImpl:Socket,queueMs:1,pollMs:600000});
  reader.start();t.after(()=>reader.stop());
  for(const r of [reader.trade,...reader.challenges])r.ws.dispatchEvent(new Event('open'));
  return reader;
}
// EOSE every pending page (decisions included, so the roster is defined).
const finish=r=>{while(r.pages.size)for(const id of [...r.pages.keys()])r.ws.message(['EOSE',id]);};
const tradeUp=reader=>{reader.trade.status.authenticated=true;finish(reader.trade);};
const snap=reader=>({relay:reader.status});

test('quorum met: 2 of 3 challenge relays complete is ready and degraded, with a visible warning',t=>{
  const reader=setup(t);tradeUp(reader);
  finish(reader.challenges[0]);finish(reader.challenges[1]); // ws://c is connected but never completes
  const s=reader.status;
  assert.equal(s.historyComplete,true,'2 of 3 meets the default quorum');
  assert.equal(s.error,null);assert.equal(s.degraded,true);
  assert.deepEqual(s.quorum,{required:2,complete:2,total:3});assert.deepEqual(s.missingRelays,['ws://c']);
  assert.ok(s.warnings.includes('Degraded: 2 of 3 challenge relays complete (quorum 2); missing: ws://c'),s.warnings.join('; '));
  assert.equal(relayReady(snap(reader)),true,'the runner posts while degraded');
  // The page says so on its status line.
  assert.equal(connectionText(s,'Live'),'Live · Degraded: 2 of 3 challenge relays complete (quorum 2); missing: ws://c');
  // The third relay catches up: no longer degraded, warning gone.
  finish(reader.challenges[2]);
  assert.equal(reader.status.degraded,false);assert.ok(!reader.status.warnings.some(w=>w.startsWith('Degraded')));
  assert.equal(connectionText(reader.status,'Live'),'Live');
});

test('our trade relay is required even when the quorum is met',t=>{
  const reader=setup(t);
  for(const r of reader.challenges)finish(r);
  assert.equal(reader.status.quorum.complete,3);
  assert.equal(reader.status.historyComplete,false,'trade history not read yet');
  tradeUp(reader);assert.equal(reader.status.historyComplete,true);
  reader.trade.ws.close();
  assert.equal(reader.status.historyComplete,false);assert.equal(relayReady(snap(reader)),false);
  assert.ok(reader.status.failedRelays.includes('ws://trade'));
  // A snapshot that claims ready overall but whose trade relay is down is never ready (coordinator).
  const forged={relay:{connected:true,authenticated:true,historyComplete:true,error:null,failedRelays:[],quorum:{required:2,complete:3,total:3},degraded:false,
    relays:[{role:'trade',url:'ws://trade',connected:false,authenticated:false,historyComplete:false,error:null}]}};
  assert.equal(relayReady(forged),false);
  forged.relay.relays[0]={role:'trade',url:'ws://trade',connected:true,authenticated:true,historyComplete:true,error:null};
  assert.equal(relayReady(forged),true);
  forged.relay.failedRelays=['ws://trade'];assert.equal(relayReady(forged),false);
});

test('below quorum: never ready; once the relays still up cannot meet it, an error names the failed ones',t=>{
  const reader=setup(t);tradeUp(reader);
  finish(reader.challenges[0]);
  assert.equal(reader.status.historyComplete,false,'1 of 3');assert.equal(reader.status.error,null,'others still catching up');
  assert.equal(reader.status.degraded,false);assert.equal(relayReady(snap(reader)),false);
  reader.challenges[1].ws.close();reader.challenges[2].ws.close();
  assert.equal(reader.status.historyComplete,false);
  assert.match(reader.status.error,/^Challenge history incomplete; quorum not met \(1 of 3 challenge relays available, need 2\); relay failed: ws:\/\/b, ws:\/\/c$/);
  assert.equal(relayReady(snap(reader)),false);
  // A degraded-looking snapshot below quorum is refused by the coordinator too.
  assert.equal(relayReady({relay:{connected:true,authenticated:true,historyComplete:true,error:null,failedRelays:['ws://b','ws://c'],quorum:{required:2,complete:1,total:3},relays:[{role:'trade',url:'ws://trade',connected:true,authenticated:true,historyComplete:true,error:null}]}}),false);
});

test('quorum setting: default 2, explicit values must be a majority and at most every relay',t=>{
  assert.equal(DEFAULT_CHALLENGE_QUORUM,2);
  for(const [n,q] of [[1,1],[2,2],[3,2],[4,3],[5,3]])assert.equal(parseChallengeQuorum(undefined,n),q,`${n} relays`);
  assert.equal(parseChallengeQuorum('3',3),3);assert.equal(parseChallengeQuorum(' 2 ',3),2);assert.equal(parseChallengeQuorum('',3),2);
  for(const bad of ['1','4','0','x','2.5','-2'])assert.throws(()=>parseChallengeQuorum(bad,3),/Invalid challenge relay quorum/,bad);
  assert.throws(()=>parseChallengeQuorum(2,4),/majority/);
  assert.throws(()=>new ChallengeReader({url:'ws://trade',challengeRelays:['ws://a','ws://b','ws://c'],quorum:1,buyer,store:new EventStore(),WebSocketImpl:Socket}),/quorum/);
  // quorum 3: every relay, as before this change.
  const reader=setup(t,{quorum:3});tradeUp(reader);finish(reader.challenges[0]);finish(reader.challenges[1]);
  assert.equal(reader.status.historyComplete,false);
  finish(reader.challenges[2]);assert.equal(reader.status.historyComplete,true);
});

// Node 22's WebSocket, closed while CONNECTING, dispatches 'error' synchronously from close() and
// never dispatches 'close'. The old error -> ws.close() handler recursed until the stack overflowed.
class Node22Connecting extends EventTarget {
  static all=[];readyState=0;closes=0;
  constructor(url){super();this.url=url;Node22Connecting.all.push(this);}
  send(){throw Error('not open');}
  close(){this.closes++;this.readyState=2;this.dispatchEvent(new Event('error'));}
}
test('stop while CONNECTING (Node 22 semantics) ends the connection once, without recursion',()=>{
  const reader=new RelayReader({url:'ws://x',buyer,store:new EventStore(),WebSocketImpl:Node22Connecting,queueMs:1,pollMs:600000});
  reader.start();const ws=Node22Connecting.all.at(-1);
  assert.doesNotThrow(()=>reader.stop());
  assert.equal(ws.closes,1,'close() called exactly once');
  assert.equal(reader.status.connected,false);assert.equal(reader.dead,true);
});
test('a connect timeout on a socket that never closes still reconnects',async t=>{
  Node22Connecting.all=[];
  const reader=new RelayReader({url:'ws://x',buyer,store:new EventStore(),WebSocketImpl:Node22Connecting,queueMs:1,pollMs:600000,connectMs:20,retryMs:10});
  reader.start();t.after(()=>reader.stop());
  await until(()=>Node22Connecting.all.length>=2,2000);
  assert.equal(Node22Connecting.all[0].closes,1);
});
test('real WebSocket: stop while the TCP connection never completes the handshake',async t=>{
  const server=net.createServer(()=>{}).listen(0,'127.0.0.1');await new Promise(r=>server.once('listening',r));
  t.after(()=>server.close());
  const reader=new RelayReader({url:`ws://127.0.0.1:${server.address().port}`,buyer,store:new EventStore(),queueMs:1,pollMs:600000});
  reader.start();await pause(50);
  assert.equal(reader.ws.readyState,0,'still CONNECTING');
  assert.doesNotThrow(()=>reader.stop());
  await pause(100);
  assert.equal(reader.status.connected,false);assert.equal(reader.dead,true);
});
