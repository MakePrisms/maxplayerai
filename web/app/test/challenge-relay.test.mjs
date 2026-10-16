import test from 'node:test';
import assert from 'node:assert/strict';
import { schnorr } from '@noble/curves/secp256k1';
import { RelayReader, ChallengeReader, parseChallengeRelays, challengeFilters } from '../src/challenge/shared/relay.mjs';
import { EventStore } from '../src/challenge/shared/model.mjs';
import { eventId, verify } from '../src/challenge/shared/events.mjs';
const key='01'.padStart(64,'0'),buyer=Buffer.from(schnorr.getPublicKey(key)).toString('hex');
const signed=(kind,time=100,tags=[])=>{const e={pubkey:buyer,kind,created_at:time,tags,content:''};e.id=eventId(e);e.sig=Buffer.from(schnorr.sign(e.id,key)).toString('hex');return e;};
class Socket extends EventTarget {
  static all=[];sent=[];readyState=1;
  constructor(){super();Socket.all.push(this);}
  send(data){this.sent.push(JSON.parse(data));}
  message(data){this.dispatchEvent(new MessageEvent('message',{data:JSON.stringify(data)}));}
  close(){this.readyState=3;this.dispatchEvent(new Event('close'));}
}
const pause=ms=>new Promise(r=>setTimeout(r,ms));
async function until(predicate) {for(let i=0;i<400;i++){if(predicate())return;await pause(10);}assert.ok(predicate(),'timed out waiting for socket state');}
function setup(t,options={}) {const store=new EventStore(),reader=new RelayReader({url:'ws://localhost:1234',buyer,store,WebSocketImpl:Socket,queueMs:1,pollMs:100000,...options});reader.start();const socket=Socket.all.at(-1);socket.dispatchEvent(new Event('open'));t.after(()=>reader.stop());return {reader,socket,store};}
test('anonymous history starts before AUTH; filters scope every kind to the buyer',async t=>{
  const {reader,socket}=setup(t);await until(()=>socket.sent.filter(m=>m[0]==='REQ'&&!m[1].startsWith('live-')).length>=8);
  assert.equal(socket.sent.filter(m=>m[0]==='REQ'&&!m[1].startsWith('live-')).length,8);
  assert.deepEqual(challengeFilters(buyer).filter(f=>f.authors).flatMap(f=>f.kinds),[30341,30343,3401,3405,3406]);
  assert.deepEqual(challengeFilters(buyer).filter(f=>f['#p']).flatMap(f=>f.kinds),[30342,3402,3403]);
  for(const [id] of [...reader.pages])socket.message(['EOSE',id]);
  assert.equal(reader.status.historyComplete,true);assert.equal(reader.status.authenticated,false);
});
test('AUTH uses an ephemeral key and reinstalls live subscriptions after OK',async t=>{
  const {reader,socket}=setup(t);socket.message(['AUTH','nonce']);const auth=socket.sent.find(m=>m[0]==='AUTH')[1];
  assert.ok(verify(auth));assert.notEqual(auth.pubkey,buyer);assert.equal(auth.kind,22242);
  socket.message(['OK',auth.id,true,'']);assert.equal(reader.status.authenticated,true);
  await until(()=>socket.sent.filter(m=>m[0]==='REQ'&&m[1].startsWith('live-')).length>=8);assert.ok(socket.sent.filter(m=>m[0]==='REQ'&&m[1].startsWith('live-')).length>=8);
  assert.ok(socket.sent.every(m=>['AUTH','REQ','CLOSE'].includes(m[0])));
  const other=setup(t);other.socket.message(['AUTH','nonce']);assert.notEqual(other.socket.sent.find(m=>m[0]==='AUTH')[1].pubkey,auth.pubkey);
});
test('AUTH refusal preserves history and polls since newest minus overlap',async t=>{
  const {reader,socket,store}=setup(t,{pollMs:80,overlap:30});await until(()=>socket.sent.filter(m=>m[0]==='REQ').length>=8);
  socket.message(['AUTH','nonce']);socket.message(['OK',reader.authId,false,'restricted']);
  socket.message(['EVENT','30341',signed(30341,100)]);
  for(const [id] of [...reader.pages])socket.message(['EOSE',id]);
  assert.equal(store.events.size,1);assert.equal(reader.status.historyComplete,true);assert.equal(reader.status.authenticated,false);
  await until(()=>socket.sent.some(m=>m[0]==='REQ'&&m[1]==='30341'&&m[2].since===70));const poll=socket.sent.filter(m=>m[0]==='REQ'&&m[1]==='30341').at(-1);assert.equal(poll[2].since,70);
});
test('rejects invalid signatures, wrong authors, wrong p scope and unsolicited frames',t=>{
  const {socket,store}=setup(t);
  socket.message(['EVENT','30341',{...signed(30341),content:'tampered'}]);
  socket.message(['EVENT','3402',signed(3402)]);
  socket.message(['EVENT','unknown',signed(30341)]);
  const other=signed(30341);other.pubkey='f'.repeat(64);socket.message(['EVENT','30341',other]);
  assert.equal(store.events.size,0);
  const e=signed(3402,100,[['p',buyer]]);socket.message(['EVENT','3402',e]);socket.message(['EVENT','3402',e]);assert.equal(store.events.size,1);
});
test('pagination overlaps timestamp boundary and flags saturated second',t=>{
  const {reader,socket}=setup(t);const e=signed(30341);
  for(let n=0;n<500;n++)socket.message(['EVENT','30341',e]);
  socket.message(['EOSE','30341']);assert.equal(reader.pages.get('30341').filter.until,100);
  assert.equal(socket.sent.some(m=>m[0]==='CLOSE'&&m[1]==='30341'),false,'replacement REQ must not receive an old CLOSE acknowledgement');
  for(let n=0;n<500;n++)socket.message(['EVENT','30341',e]);
  socket.message(['EOSE','30341']);assert.match(reader.status.error,/History truncated/);assert.equal(reader.status.historyComplete,false);
});
test('post-EOSE events update the store; CLOSED live feed falls back to polling',t=>{
  const {reader,socket,store}=setup(t);socket.message(['AUTH','nonce']);socket.message(['OK',reader.authId,true,'']);
  socket.message(['EOSE','live-30341']);socket.message(['EVENT','live-30341',signed(30341)]);assert.equal(store.events.size,1);
  socket.message(['CLOSED','live-30341','restricted']);assert.equal(reader.status.authenticated,false);assert.equal(reader.status.connected,true);
});
test('reconnect uses same per-tab key and starts full history again; stop cancels retries',async t=>{
  const {reader,socket}=setup(t);socket.message(['AUTH','one']);const pubkey=socket.sent.find(m=>m[0]==='AUTH')[1].pubkey;
  reader.retry=1;socket.close();await until(()=>Socket.all.at(-1)!==socket);const next=Socket.all.at(-1);assert.notEqual(next,socket);next.dispatchEvent(new Event('open'));next.message(['AUTH','two']);
  assert.equal(next.sent.find(m=>m[0]==='AUTH')[1].pubkey,pubkey);assert.equal(reader.pages.size,8);
  reader.stop();const count=Socket.all.length;await pause(20);assert.equal(Socket.all.length,count);
});

test('polling waits for the whole history round, including slower filters',async t=>{
  const {reader,socket}=setup(t,{pollMs:20});
  await until(()=>socket.sent.filter(m=>m[0]==='REQ'&&!m[1].startsWith('live-')).length>=8);
  socket.message(['EOSE','30341']);
  await pause(80);
  assert.equal(reader.pages.has('30341'),false);
  assert.equal(socket.sent.filter(m=>m[0]==='REQ'&&m[1]==='30341').length,1);
  assert.equal(reader.status.historyComplete,false);
});

test('split sources union IDs, reject cross-kind events, and tolerate a failed public relay', t=>{
  const store=new EventStore();
  const reader=new ChallengeReader({url:'ws://trade',challengeRelays:['ws://a','ws://b','ws://bad'],buyer,store,WebSocketImpl:Socket,queueMs:1});
  reader.start();t.after(()=>reader.stop());
  for(const r of [reader.trade,...reader.challenges])r.ws.dispatchEvent(new Event('open'));
  assert.deepEqual([...reader.trade.filters.values()].flatMap(f=>f.kinds),[3401,3402,3405,3403,3406]);
  for(const r of reader.challenges)assert.deepEqual([...r.filters.values()].flatMap(f=>f.kinds),[30341,30342,30343]);
  const e=signed(30341);
  for(const r of reader.challenges.slice(0,2))r.ws.message(['EVENT','30341',e]);
  reader.trade.ws.message(['EVENT','30341',signed(30341,101)]);
  reader.challenges[0].ws.message(['EVENT','3401',signed(3401)]);
  assert.equal(store.events.size,1);
  // Membership from public relays expands only trade-relay heartbeat/profile reads.
  reader.challenges[0].ws.message(['EVENT','30342',signed(30342,100,[['p',buyer]])]);
  assert.ok(reader.trade.filters.has(`30340-${buyer}`));
  assert.ok(!reader.challenges[0].filters.has(`30340-${buyer}`));
  for(const r of [reader.trade,...reader.challenges.slice(0,2)])for(const id of [...r.pages.keys()])r.ws.message(['EOSE',id]);
  reader.challenges[2].ws.close();
  assert.equal(reader.status.connected,true);assert.equal(reader.status.historyComplete,true);
  assert.deepEqual(reader.status.failedRelays,['ws://bad']);assert.equal(reader.status.error,null);
  reader.challenges[0].ws.close();reader.challenges[1].ws.close();
  assert.equal(reader.status.connected,false);assert.match(reader.status.error,/No challenge relay/);
});
test('public relays stream post-EOSE without AUTH and share the per-tab ephemeral identity',t=>{
  const store=new EventStore(),reader=new ChallengeReader({url:'ws://trade',challengeRelays:['ws://a'],buyer,store,WebSocketImpl:Socket});
  reader.start();t.after(()=>reader.stop());
  for(const r of [reader.trade,...reader.challenges])r.ws.dispatchEvent(new Event('open'));
  const publicRelay=reader.challenges[0];
  publicRelay.ws.message(['EOSE','live-30341']);publicRelay.ws.message(['EVENT','live-30341',signed(30341)]);
  assert.equal(store.events.size,1);
  for(const r of [reader.trade,publicRelay])r.ws.message(['AUTH','nonce']);
  assert.equal(reader.trade.ws.sent.find(m=>m[0]==='AUTH')[1].pubkey,publicRelay.ws.sent.find(m=>m[0]==='AUTH')[1].pubkey);
});
test('challenge relay config validates, trims and deduplicates',()=>{
  assert.deepEqual(parseChallengeRelays(' ws://a ,ws://b,ws://a '),['ws://a','ws://b']);
  assert.throws(()=>parseChallengeRelays(''));assert.throws(()=>parseChallengeRelays('https://a'));
  assert.equal(parseChallengeRelays().length,3);
});

test('latest challenge replacement wins across relays regardless of duplicate arrival order',t=>{
  const store=new EventStore(),reader=new ChallengeReader({url:'ws://trade',challengeRelays:['ws://a','ws://b'],buyer,store,WebSocketImpl:Socket});
  reader.start();t.after(()=>reader.stop());
  for(const r of [reader.trade,...reader.challenges])r.ws.dispatchEvent(new Event('open'));
  const tags=[['t','maxplayer'],['v','1'],['d','test'],['title','Test'],['starts','100'],['ends','10000'],['budget','1000','sat'],['status','in_progress']];
  const older=signed(30341,100,tags),newer=signed(30341,101,tags);
  reader.challenges[1].ws.message(['EVENT','30341',newer]);
  reader.challenges[0].ws.message(['EVENT','30341',older]);
  reader.challenges[0].ws.message(['EVENT','30341',newer]);
  assert.equal(store.events.size,2);assert.equal(store.snapshot(buyer,200).challenge.eventId,newer.id);
});
