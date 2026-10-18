import test from 'node:test';
import assert from 'node:assert/strict';
import { schnorr } from '@noble/curves/secp256k1';
import { RelayReader, ChallengeReader, parseChallengeRelays, challengeFilters, rosterFilters, matches } from '../src/challenge/shared/relay.mjs';
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
const memberTags=[['t','maxplayer'],['v','1'],['d',buyer],['p',buyer],['since','100'],['minutes','60'],['status','joined']];
const pause=ms=>new Promise(r=>setTimeout(r,ms));
async function until(predicate) {for(let i=0;i<400;i++){if(predicate())return;await pause(10);}assert.ok(predicate(),'timed out waiting for socket state');}
function setup(t,options={}) {const store=new EventStore(),reader=new RelayReader({url:'ws://localhost:1234',buyer,store,WebSocketImpl:Socket,queueMs:1,pollMs:100000,...options});reader.start();const socket=Socket.all.at(-1);socket.dispatchEvent(new Event('open'));t.after(()=>reader.stop());return {reader,socket,store};}
test('anonymous history starts before AUTH; filters scope every kind to the buyer',async t=>{
  const {reader,socket}=setup(t);await until(()=>socket.sent.filter(m=>m[0]==='REQ'&&!m[1].startsWith('live-')).length>=8);
  assert.equal(socket.sent.filter(m=>m[0]==='REQ'&&!m[1].startsWith('live-')).length,8);
  assert.deepEqual(challengeFilters(buyer).filter(f=>f.authors).flatMap(f=>f.kinds),[30341,30343,3401,3405,3406]);
  assert.deepEqual(challengeFilters(buyer).filter(f=>f['#p']).flatMap(f=>f.kinds),[3402,3403],'memberships are never read unscoped by the trusted reader');
  assert.ok(socket.sent.some(m=>m[0]==='REQ'&&m[1]==='applicants'&&m[2].limit===256),'applicant sample is one capped newest-first page');
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
test('post-EOSE events update the store; anonymous CLOSED live feed falls back to polling',t=>{
  const {reader,socket,store}=setup(t);
  for(const id of [...reader.pages.keys()])socket.message(['EOSE',id]);
  socket.message(['EOSE','live-30341']);socket.message(['EVENT','live-30341',signed(30341)]);assert.equal(store.events.size,1);
  socket.message(['CLOSED','live-30341','restricted']);assert.equal(reader.status.connected,true);
  assert.equal(reader.status.historyComplete,true,'anonymous: history stays complete through polls');
  assert.ok(reader.status.warnings.some(w=>/live-?30341|30341 closed by relay \(restricted\)/.test(w)),'reason surfaced');
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
  for(const r of reader.challenges)assert.deepEqual([...r.filters.values()].flatMap(f=>f.kinds),[30341,30343]);
  const e=signed(30341);
  for(const r of reader.challenges.slice(0,2))r.ws.message(['EVENT','30341',e]);
  reader.trade.ws.message(['EVENT','30341',signed(30341,101)]);
  reader.challenges[0].ws.message(['EVENT','3401',signed(3401)]);
  assert.equal(store.events.size,1);
  // A membership on the applicant feed never reaches the trusted store or trade metadata.
  reader.challenges[0].ws.message(['EVENT','applicants',signed(30342,100,memberTags)]);
  assert.equal(store.events.size,1);
  assert.ok(!reader.trade.filters.has(`30340-${buyer}`),'metadata only for buyer-accepted sellers');
  for(const r of [reader.trade,...reader.challenges.slice(0,2)])for(const id of [...r.pages.keys()])r.ws.message(['EOSE',id]);
  reader.challenges[2].ws.close();
  assert.equal(reader.status.connected,true);assert.equal(reader.status.historyComplete,false);
  assert.deepEqual(reader.status.failedRelays,['ws://bad']);assert.match(reader.status.error,/ws:\/\/bad/);
  reader.challenges[0].ws.close();reader.challenges[1].ws.close();
  assert.equal(reader.status.connected,false);assert.match(reader.status.error,/incomplete/);
});
test('public relays stream post-EOSE without AUTH and use distinct per-relay ephemeral identities',t=>{
  const store=new EventStore(),reader=new ChallengeReader({url:'ws://trade',challengeRelays:['ws://a'],buyer,store,WebSocketImpl:Socket});
  reader.start();t.after(()=>reader.stop());
  for(const r of [reader.trade,...reader.challenges])r.ws.dispatchEvent(new Event('open'));
  const publicRelay=reader.challenges[0];
  publicRelay.ws.message(['EOSE','live-30341']);publicRelay.ws.message(['EVENT','live-30341',signed(30341)]);
  assert.equal(store.events.size,1);
  for(const r of [reader.trade,publicRelay])r.ws.message(['AUTH','nonce']);
  assert.notEqual(reader.trade.ws.sent.find(m=>m[0]==='AUTH')[1].pubkey,publicRelay.ws.sent.find(m=>m[0]==='AUTH')[1].pubkey);
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

test('auth-required CLOSED before AUTH OK reissues history and clears error',async t=>{
  const {reader,socket}=setup(t);await until(()=>socket.sent.some(m=>m[0]==='REQ'&&m[1]==='3401'));
  socket.message(['CLOSED','3401','auth-required: authentication required']);
  socket.message(['AUTH','nonce']);socket.message(['OK',reader.authId,true,'']);
  assert.equal(reader.status.error,null);assert.ok(reader.pages.has('3401'));
  await until(()=>socket.sent.filter(m=>m[0]==='REQ'&&m[1]==='3401').length>=2);
  for(const id of [...reader.pages.keys()])socket.message(['EOSE',id]);
  // AUTH was offered and succeeded: the round also needs the live REQs sent after AUTH OK.
  await answerAll(reader,socket);
  assert.equal(reader.status.historyComplete,true);
});
// EOSE every REQ the reader sends (history pages and live, any generation) until it is complete.
async function answerAll(reader,socket){const done=new Set();await until(()=>{for(const m of socket.sent)if(m[0]==='REQ'&&!done.has(m)){done.add(m);socket.message(['EOSE',m[1]]);}return reader.status.historyComplete;});}
const ns=[['t','maxplayer'],['v','1']];
const dayTags=[...ns,['d','day'],['title','Day'],['starts','200'],['ends','1000'],['budget','0','sat'],['status','in_progress']];
const acceptTags=[...ns,['d',buyer],['p',buyer],['decision','accepted']];
const rosterId=r=>[...r.pages.keys()].find(id=>id.startsWith('roster-'));
const trusted=store=>[...store.events.values()].filter(e=>e.kind===30342);
function finish(reader,socket) { while(reader.pages.size)for(const id of [...reader.pages.keys()])socket.message(['EOSE',id]); }
function signedBy(privateKey,kind,time,tags,content='') {
  const e={kind,pubkey:Buffer.from(schnorr.getPublicKey(privateKey)).toString('hex'),created_at:time,tags,content};
  e.id=eventId(e);e.sig=Buffer.from(schnorr.sign(e.id,privateKey)).toString('hex');return e;
}
// Decisions first: an accepted decision's EOSE installs the authors-scoped roster read.
function acceptSelf(socket) {socket.message(['EVENT','30343',signed(30343,100,acceptTags)]);socket.message(['EOSE','30343']);}

test('roster filters use authors = accepted sellers, chunked, and honor since/until',()=>{
  const pks=Array.from({length:250},(_,i)=>(i+2).toString(16).padStart(64,'0'));
  const chunks=rosterFilters(buyer,pks);
  assert.equal(chunks.length,3);assert.deepEqual(chunks.flatMap(f=>f.authors),[...pks].sort());
  assert.ok(chunks.every(f=>f.kinds.length===1&&f.kinds[0]===30342&&f['#p'][0]===buyer));
  const e=signed(30342,100,memberTags);
  assert.equal(matches(e,{kinds:[30342],authors:[buyer]}),true);
  assert.equal(matches(e,{kinds:[30342],authors:[pks[0]]}),false);
  assert.equal(matches(e,{kinds:[30342],until:99}),false);assert.equal(matches(e,{kinds:[30342],since:101}),false);
  assert.equal(matches(e,{kinds:[30342],since:100,until:100}),true);
});

test('metadata subscriptions cover accepted sellers only, capped at 256 in pubkey order',t=>{
  const {reader}=setup(t);
  const pks=Array.from({length:3000},(_,i)=>(i+2).toString(16).padStart(64,'0')).reverse();
  reader.setRoster(pks);
  assert.equal(reader.members.size,256);assert.deepEqual([...reader.members],[...pks].sort().slice(0,256));
  assert.match(reader.status.warnings[0],/256 of 3000/);
  assert.equal([...reader.filters.keys()].filter(id=>id.startsWith('roster-')).length,30);
  assert.ok(reader.filters.size<=7+30+512);
  reader.setRoster(pks.slice(0,10));
  assert.equal(reader.members.size,10);assert.deepEqual(reader.status.warnings,[]);
  assert.equal([...reader.filters.keys()].filter(id=>id.startsWith('roster-')).length,1,'roster re-subscribes when decisions change');
});

test('a roster change keeps an earlier incomplete profile/heartbeat warning',t=>{
  const {reader,socket}=setup(t);
  const pks=Array.from({length:300},(_,i)=>(i+2).toString(16).padStart(64,'0'));
  reader.setRoster(pks.slice(0,1));
  socket.message(['CLOSED',`0-${pks[0]}`,'restricted: no']);
  assert.deepEqual(reader.status.warnings,['Profile/heartbeat read incomplete']);
  reader.setRoster(pks);
  assert.ok(reader.status.warnings.includes('Profile/heartbeat read incomplete'),'incomplete-read warning kept');
  assert.match(reader.status.warnings[0],/256 of 300/);
  reader.setRoster(pks.slice(0,2));
  assert.deepEqual(reader.status.warnings,['Profile/heartbeat read incomplete'],'only the cap warning is replaced');
});

test('relay A hiding newer left while down blocks swarm despite relay B old joined',async t=>{
  const {Swarm}=await import('../../../scripts/challenges/swarm.mjs');
  const store=new EventStore(),reader=new ChallengeReader({url:'ws://trade',challengeRelays:['ws://a','ws://b'],buyer,store,WebSocketImpl:Socket});
  reader.start();t.after(()=>reader.stop());
  for(const r of [reader.trade,...reader.challenges])r.ws.dispatchEvent(new Event('open'));
  const joined=signed(30342,100,[...ns,['d',buyer],['p',buyer],['since','100'],['minutes','60'],['status','joined']]);
  const left=signed(30342,300,[...joined.tags.filter(t=>t[0]!=='status'),['status','left']]);
  assert.ok(left.created_at>joined.created_at); // only unavailable A has this replacement
  const b=reader.challenges[1];
  b.ws.message(['EVENT','30343',signed(30343,101,acceptTags)]);
  b.ws.message(['EVENT','30341',signed(30341,102,[...ns,['d','day'],['title','Day'],['starts','200'],['ends','1000'],['budget','0','sat'],['status','in_progress']])]);
  b.ws.message(['EOSE','30343']);
  b.ws.message(['EVENT',rosterId(b),joined]);
  reader.trade.status.authenticated=true;
  for(const r of [reader.trade,b])finish(r,r.ws);
  reader.challenges[0].ws.close();
  const snap={...store.snapshot(buyer,400),relay:reader.status};assert.equal(snap.sellers[0].eligible,true);
  let posts=0;
  const io={now:()=>400,snapshot:async()=>snap,mcp:async tool=>{if(tool==='post_job')posts++;return {job_id:'fixture-job'};},
    plan:async input=>input.mode==='analyze'?{approach:'split',workstreams:[{id:'piece',description:'piece',needs:{skills:[]},estMinutes:1,dependsOn:[]}],verification:'check'}:
      {assignments:[{seller:buyer,workstream:'piece',task:'piece',durationSeconds:30}]},publish:async()=>{}};
  await new Swarm({minDuration:30,maxDuration:60,updateEvery:180},io,{state:{jobs:[],days:{}},save(){}}).tick();
  assert.equal(posts,0);assert.equal(snap.relay.historyComplete,false);assert.match(snap.relay.error,/ws:\/\/a/);
});

test('challenge relay hostname denylist covers case trailing dot and encoded dot',()=>{
  for(const url of ['wss://relay.maxplayer.ai','wss://RELAY.MAXPLAYER.AI./','wss://relay.maxplayer.ai%2e']) {
    assert.throws(()=>parseChallengeRelays(url),/production relay/);
    assert.throws(()=>new ChallengeReader({url:'ws://trade',buyer,store:new EventStore(),challengeRelays:[url],WebSocketImpl:Socket}),/production relay/);
  }
});

test('stranger membership flood stays in the capped applicant sample, never the trusted store or metadata',t=>{
  const {reader,socket,store}=setup(t);
  for(let i=2;i<=261;i++)socket.message(['EVENT','applicants',signedBy(i.toString(16).padStart(64,'0'),30342,100,memberTags)]);
  assert.equal(reader.members.size,0);assert.equal(trusted(store).length,0);
  assert.equal(store.applicants.size,256);assert.equal(store.applicantsTruncated,true);
  assert.equal(reader.filters.size,7);assert.equal(reader.status.error,null);
  finish(reader,socket);
  assert.equal(reader.status.historyComplete,true);
});

test('applicant read refused or silent never blocks trusted history',async t=>{
  const {reader,socket}=setup(t,{queueMs:1});
  socket.message(['CLOSED','applicants','restricted: no']);
  finish(reader,socket);
  assert.equal(reader.status.historyComplete,true);assert.equal(reader.status.error,null);assert.equal(reader.status.applicantsIncomplete,true);
});

test('AUTH recovery cannot clear a different hard history refusal',t=>{
  const {reader,socket}=setup(t);
  socket.message(['CLOSED','3401','restricted: not permitted']);
  socket.message(['CLOSED','3402','auth-required: authentication required']);
  socket.message(['AUTH','nonce']);socket.message(['OK',reader.authId,true,'']);
  for(const id of [...reader.pages.keys()])socket.message(['EOSE',id]);
  assert.equal(reader.status.historyComplete,false);assert.match(reader.status.error,/refused/);
});

test('roster reader is incomplete until decision history defines the roster',t=>{
  const {reader,socket}=setup(t);
  for(const id of [...reader.pages.keys()].filter(id=>id!=='30343'))socket.message(['EOSE',id]);
  assert.equal(reader.status.historyComplete,false);
  acceptSelf(socket);
  assert.ok(rosterId(reader));assert.equal(reader.status.historyComplete,false,'accepted memberships not read yet');
  finish(reader,socket);assert.equal(reader.status.historyComplete,true);
});

test('oversized membership alongside valid history does not poison EOSE',t=>{
  const {reader,socket,store}=setup(t);
  socket.message(['EVENT','30341',signed(30341,100,dayTags)]);acceptSelf(socket);
  const huge={...signed(30342,100,[['p',buyer]]),content:'x'.repeat(4097)};
  huge.id=eventId(huge);huge.sig=Buffer.from(schnorr.sign(huge.id,key)).toString('hex');
  socket.message(['EVENT',rosterId(reader),huge]);
  socket.message(['EVENT',rosterId(reader),signed(30342,100,memberTags)]);
  finish(reader,socket);
  assert.equal(reader.status.historyComplete,true);assert.equal(reader.status.error,null);
  assert.equal(trusted(store).length,1);assert.equal(store.snapshot(buyer,300).sellers.length,1);
});

test('accepted seller junk saturating one second re-reads it, then fails closed (never skips it)',t=>{
  const {reader,socket}=setup(t);acceptSelf(socket);
  const rid=rosterId(reader),junk=Array.from({length:500},(_,i)=>signed(30342,101,[['p',buyer],['junk',String(i)]]));
  for(const e of junk)socket.message(['EVENT',rid,e]);
  socket.message(['EOSE',rid]);
  assert.equal(reader.pages.get(rid).filter.until,101,'inclusive re-read of the saturated second, not until-1');
  for(const e of junk)socket.message(['EVENT',rid,e]);
  socket.message(['EOSE',rid]);
  assert.match(reader.status.error,/History truncated/);assert.equal(reader.status.historyComplete,false);
  assert.equal(reader.pages.has(rid),false,'no third request for a cursor that did not move');
});

test('a full page of out-of-window events fails closed instead of looking complete',t=>{
  const {reader,socket}=setup(t);acceptSelf(socket);
  const rid=rosterId(reader);
  for(let i=0;i<500;i++)socket.message(['EVENT',rid,signed(30342,101,[['p',buyer],['junk',String(i)]])]);
  socket.message(['EOSE',rid]);
  for(let i=0;i<500;i++)socket.message(['EVENT',rid,signed(30342,150,[['p',buyer],['late',String(i)]])]); // relay ignored until:101
  socket.message(['EOSE',rid]);
  assert.match(reader.status.error,/History truncated/);assert.equal(reader.status.historyComplete,false);
});

for(const oversized of ['content','tags'])test(`oversized ${oversized} replacement preserves left state and minutes; store keeps one bounded copy`,t=>{
  const {reader,socket,store}=setup(t);
  socket.message(['EVENT','30341',signed(30341,100,dayTags)]);acceptSelf(socket);
  socket.message(['EVENT',rosterId(reader),signed(30342,100,memberTags)]);
  const left=signed(30342,101,memberTags.map(t=>t[0]==='status'?['status','left']:t[0]==='minutes'?['minutes','20']:t));
  left.content=oversized==='content'?'x'.repeat(4097):'short note';
  if(oversized==='tags')left.tags.push(['extra','x'.repeat(16385)]);
  left.id=eventId(left);left.sig=Buffer.from(schnorr.sign(left.id,key)).toString('hex');
  socket.message(['EVENT',rosterId(reader),left]);finish(reader,socket);
  const seller=store.snapshot(buyer,300).sellers[0];
  assert.equal(seller.state,'left');assert.equal(seller.eligible,false);assert.equal(seller.minutesOffered,20);assert.equal(seller.note,'');
  assert.deepEqual(trusted(store).map(e=>e.id),[left.id]);
  assert.equal(store.events.get(left.id).content,'');assert.deepEqual(store.events.get(left.id).tags,left.tags);
});

test('split reader waits for decisions on every relay, pushes the roster to all, and deduplicates warnings',t=>{
  const store=new EventStore(),reader=new ChallengeReader({url:'ws://trade',challengeRelays:['ws://a','ws://b'],buyer,store,WebSocketImpl:Socket});
  reader.start();t.after(()=>reader.stop());
  for(const r of [reader.trade,...reader.challenges])r.ws.dispatchEvent(new Event('open'));
  const [a,b]=reader.challenges;
  finish(a,a.ws);finish(reader.trade,reader.trade.ws);
  assert.equal(reader.status.historyComplete,false,'other relay still resolving decisions');
  assert.equal(reader.trade.members.size,0);
  b.ws.message(['EVENT','30343',signed(30343,100,acceptTags)]);b.ws.message(['EOSE','30343']);
  assert.ok(reader.trade.members.has(buyer));assert.ok(rosterId(a),'roster pushed to every challenge relay');
  assert.equal(reader.status.historyComplete,false);
  a.ws.message(['EVENT',rosterId(a),signed(30342,100,memberTags)]);
  finish(a,a.ws);finish(b,b.ws);finish(reader.trade,reader.trade.ws);
  assert.equal(reader.status.historyComplete,true);assert.equal(trusted(store).length,1);
  a.status.warnings=['same warning'];b.status.warnings=['same warning'];
  assert.deepEqual(reader.status.warnings,['same warning']);
});

test('mixed accepted and discarded saturated boundary remains incomplete',t=>{
  const {reader,socket}=setup(t);acceptSelf(socket);
  const rid=rosterId(reader),valid=signed(30342,100,memberTags),junk=signed(30342,100,[['p',buyer]]);
  for(let page=0;page<2;page++) {
    socket.message(['EVENT',rid,valid]);
    for(let i=0;i<499;i++)socket.message(['EVENT',rid,junk]);
    socket.message(['EOSE',rid]);
  }
  assert.equal(reader.status.historyComplete,false);
  assert.match(reader.status.error,/History truncated/,'never skip unseen valid ties in a mixed page');
});
