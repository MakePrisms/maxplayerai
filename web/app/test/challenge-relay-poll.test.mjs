import test from 'node:test';
import assert from 'node:assert/strict';
import { schnorr } from '@noble/curves/secp256k1';
import { RelayReader, ChallengeReader } from '../src/challenge/shared/relay.mjs';
import { EventStore } from '../src/challenge/shared/model.mjs';
import { eventId } from '../src/challenge/shared/events.mjs';
// R3-4 (round 3): historyComplete flipped false->true on every poll round as history was re-paged
// (713 "Relay change" lines in 47 min, and a relay_not_ready posting gap every round).
const key='01'.padStart(64,'0'),buyer=Buffer.from(schnorr.getPublicKey(key)).toString('hex');
const signed=(kind,time=100,tags=[])=>{const e={pubkey:buyer,kind,created_at:time,tags,content:''};e.id=eventId(e);e.sig=Buffer.from(schnorr.sign(e.id,key)).toString('hex');return e;};
class Socket extends EventTarget {
  static all=[];sent=[];readyState=1;
  constructor(url){super();this.url=url;Socket.all.push(this);}
  send(data){this.sent.push(JSON.parse(data));}
  message(data){this.dispatchEvent(new MessageEvent('message',{data:JSON.stringify(data)}));}
  close(){this.readyState=3;this.dispatchEvent(new Event('close'));}
}
const pause=ms=>new Promise(r=>setTimeout(r,ms));
async function until(predicate) {for(let i=0;i<400;i++){if(predicate())return;await pause(5);}assert.ok(predicate(),'timed out waiting for reader state');}
function setup(t,options={}) {
  const store=new EventStore(),reader=new RelayReader({url:'ws://localhost:1234',buyer,store,WebSocketImpl:Socket,queueMs:1,pollMs:20,...options});
  reader.start();const socket=Socket.all.at(-1);socket.dispatchEvent(new Event('open'));t.after(()=>reader.stop());
  return {reader,socket};
}
const eoseAll=(reader,socket)=>{for(const id of [...reader.pages.keys()])socket.message(['EOSE',id]);};
// A poll round, seen from outside: a trusted history page pending again after the initial read.
const pollRound=reader=>reader.pages.has('30341');
async function ready(t,options) {
  const r=setup(t,options);
  await until(()=>r.reader.pages.size>=8&&r.socket.sent.filter(m=>m[0]==='REQ'&&!m[1].startsWith('live-')).length>=8);
  eoseAll(r.reader,r.socket);assert.equal(r.reader.status.historyComplete,true,'initial history complete');
  return r;
}
test('R3-4: once initial history is complete, poll rounds keep historyComplete true (pollInProgress instead); fail-closed reads still drop it',async t=>{
  const {reader,socket}=await ready(t);
  const flips=[];let last=reader.status.historyComplete;
  const onChange=reader.onChange;reader.onChange=()=>{onChange();if(reader.status.historyComplete!==last){last=reader.status.historyComplete;flips.push(last);}};
  for(let round=0;round<5;round++) {
    await until(()=>pollRound(reader));
    assert.equal(reader.status.historyComplete,true,`round ${round}: a poll of history already read does not drop readiness`);
    assert.equal(reader.status.pollInProgress,true,'tracked separately');
    // A re-read page that saturates is paginated, still without dropping readiness.
    if(round===1){for(let n=0;n<500;n++)socket.message(['EVENT','30341',signed(30341,100+n)]);socket.message(['EOSE','30341']);
      assert.equal(reader.pages.get('30341').filter.until,100,'paginated from the oldest second');assert.equal(reader.status.historyComplete,true);}
    eoseAll(reader,socket);
    assert.equal(reader.status.pollInProgress,false);assert.equal(reader.status.historyComplete,true);
  }
  assert.deepEqual(flips,[],'no readiness transition across five poll rounds');
  // Fail closed 1: a poll page truncated (500+ events in one second) sets an error; not ready.
  await until(()=>pollRound(reader));
  const e=signed(30341,5000);
  for(let n=0;n<500;n++)socket.message(['EVENT','30341',e]);socket.message(['EOSE','30341']);
  for(let n=0;n<500;n++)socket.message(['EVENT','30341',e]);socket.message(['EOSE','30341']);
  assert.match(reader.status.error,/History truncated/);assert.equal(reader.status.historyComplete,false);assert.deepEqual(flips,[false]);
});
test('R3-4: a refused poll page recovers (not ready); a filter never read on this connection still blocks readiness',async t=>{
  const a=await ready(t);
  await until(()=>pollRound(a.reader));
  a.socket.message(['CLOSED','30341','error: overloaded']);
  assert.equal(a.reader.status.historyComplete,false);assert.match(a.reader.status.error??'',/refused \(30341\)/);
  // A new trusted filter (a roster chunk for a newly accepted seller) is not a re-read: it blocks.
  const b=await ready(t);
  b.reader.setRoster(['ab'.repeat(32)]);
  const roster=[...b.reader.pages.keys()].find(id=>id.startsWith('roster-'));
  assert.ok(roster);assert.equal(b.reader.status.historyComplete,false,'unread roster filter blocks');
  b.socket.message(['EOSE',roster]);assert.equal(b.reader.status.historyComplete,true);
  // A fresh connection starts over: nothing counts as read until its initial history completes.
  b.reader.retry=1;b.socket.close();await until(()=>Socket.all.at(-1)!==b.socket);
  const next=Socket.all.at(-1);next.dispatchEvent(new Event('open'));
  assert.equal(b.reader.status.historyComplete,false);assert.equal(b.reader.status.pollInProgress,false);
  // The split reader exposes pollInProgress (display only) next to the readiness it never affects.
  const s=new ChallengeReader({url:'ws://trade',challengeRelays:['ws://a'],buyer,store:new EventStore(),WebSocketImpl:Socket,queueMs:1});
  assert.equal(s.status.pollInProgress,false);
});
