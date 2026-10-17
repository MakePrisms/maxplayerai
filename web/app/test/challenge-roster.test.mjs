// Advisor r4 regressions, driven only through the public reader/store/swarm API and a fake relay
// that honors kinds/authors/#p/since/until/limit (newest first) like a real relay.
import test from 'node:test';
import assert from 'node:assert/strict';
import { schnorr } from '@noble/curves/secp256k1';
import { ChallengeReader } from '../src/challenge/shared/relay.mjs';
import { EventStore } from '../src/challenge/shared/model.mjs';
import { eventId } from '../src/challenge/shared/events.mjs';
import { coordinate } from '../../../scripts/challenges/coordinator.mjs';
import { Swarm } from '../../../scripts/challenges/swarm.mjs';
const hexKey=n=>n.toString(16).padStart(64,'0');
const pub=k=>Buffer.from(schnorr.getPublicKey(k)).toString('hex');
const BUYER_KEY=hexKey(1),SELLER_KEY=hexKey(2),buyer=pub(BUYER_KEY),seller=pub(SELLER_KEY);
function sign(k,kind,created_at,tags,content='') {const e={pubkey:pub(k),kind,created_at,tags,content};e.id=eventId(e);e.sig=Buffer.from(schnorr.sign(e.id,k)).toString('hex');return e;}
const ns=[['t','maxplayer'],['v','1']];
const member=(k,t,status='joined',minutes='60',since=String(t),content='')=>sign(k,30342,t,[...ns,['d',buyer],['p',buyer],['since',since],['minutes',minutes],['status',status]],content);
const accept=(pk,t=90)=>sign(BUYER_KEY,30343,t,[...ns,['d',pk],['p',pk],['decision','accepted']]);
const day=sign(BUYER_KEY,30341,150,[...ns,['d','day'],['title','Day'],['starts','200'],['ends','100000'],['budget','0','sat'],['status','in_progress']]);
// Junk: structurally invalid memberships (no minutes/status) that mention the buyer.
const junk=(k,t,i)=>sign(k,30342,t,[['p',buyer],['junk',String(i)]]);
let strangerJunk;const strangers=()=>strangerJunk??=Array.from({length:500},(_,i)=>junk(hexKey(1000+i),100,i));

class FakeRelay {
  constructor(events,{first=new Set(),ignoreUntil=false}={}) {Object.assign(this,{events,first,ignoreUntil});this.reqs=[];}
  matches(e,f){return f.kinds.includes(e.kind)&&(!f.authors||f.authors.includes(e.pubkey))&&(!f['#p']||e.tags.some(t=>t[0]==='p'&&f['#p'].includes(t[1])))&&
    (f.since===undefined||e.created_at>=f.since)&&(this.ignoreUntil||f.until===undefined||e.created_at<=f.until);}
  answer(f){return this.events.filter(e=>this.matches(e,f)).sort((a,b)=>b.created_at-a.created_at||Number(this.first.has(b.id))-Number(this.first.has(a.id))||a.id.localeCompare(b.id)).slice(0,f.limit??500);}
}
function network(relays) {
  return class extends EventTarget {
    constructor(url){super();this.relay=relays[url];this.readyState=0;setTimeout(()=>{this.readyState=1;this.dispatchEvent(new Event('open'));this.push(['AUTH','nonce']);},0);}
    push(m){if(this.readyState===1)this.dispatchEvent(new MessageEvent('message',{data:JSON.stringify(m)}));}
    send(data){const m=JSON.parse(data);this.relay.reqs.push(m);
      if(m[0]==='AUTH')setTimeout(()=>this.push(['OK',m[1].id,true,'']),0);
      if(m[0]==='REQ')setTimeout(()=>{for(const e of this.relay.answer(m[2]))this.push(['EVENT',m[1],e]);this.push(['EOSE',m[1]]);},0);}
    close(){this.readyState=3;this.dispatchEvent(new Event('close'));}
  };
}
const pause=ms=>new Promise(r=>setTimeout(r,ms));
async function until(predicate,ms=20000){const end=Date.now()+ms;while(Date.now()<end){if(predicate())return;await pause(10);}assert.ok(predicate(),'timed out');}
function open(t,{challenge=[],trade=[],options={}}={}) {
  const relays={'ws://a':new FakeRelay(challenge,options),'ws://trade':new FakeRelay(trade)};
  const store=new EventStore(),reader=new ChallengeReader({url:'ws://trade',challengeRelays:['ws://a'],buyer,store,WebSocketImpl:network(relays),queueMs:1,pollMs:600000});
  reader.start();t.after(()=>reader.stop());
  return {store,reader,relays};
}
const analysis={approach:'split',workstreams:[{id:'piece',description:'piece',needs:{skills:[]},estMinutes:1,dependsOn:[]}],verification:'check'};
async function tick(snap,now) {
  const publishes=[],inputs=[];
  const io={now:()=>now,snapshot:async()=>snap,mcp:async()=>({job_id:'fixture'}),plan:async x=>{inputs.push(x);return x.mode==='analyze'?analysis:{wait:true};},publish:async(...a)=>publishes.push(a)};
  await new Swarm({minDuration:30,maxDuration:60,updateEvery:100000},io,{state:{jobs:[],days:{}},save(){}}).tick();
  return {publishes,inputs};
}

for(const variant of ['join','leave'])test(`R4-1 ${variant}: 500 junk memberships at the accepted seller's second cannot hide it`,async t=>{
  const real=variant==='join'?[member(SELLER_KEY,100)]:[member(SELLER_KEY,50),member(SELLER_KEY,100,'left','60','50')];
  const flood=strangers();
  // The relay returns all 500 junk events before the real one at t=100, on history and live REQs.
  const {store,reader}=open(t,{challenge:[day,accept(seller),...flood,...real],options:{first:new Set(flood.map(e=>e.id))}});
  await until(()=>reader.status.historyComplete);
  const snap={...store.snapshot(buyer,300),relay:reader.status};
  const own=snap.sellers.find(s=>s.pubkey===seller);
  assert.ok(own,'accepted seller is in the snapshot');
  if(variant==='join') {
    assert.equal(own.eligible,true);
    assert.equal(coordinate(snap,{now:300,durationSeconds:30}).sellers.find(s=>s.seller_pubkey===seller)?.postingAllowed,true);
    const {publishes,inputs}=await tick(snap,300);
    assert.ok(!publishes.some(p=>p[2]==='completed'),'swarm must not complete the day');
    assert.ok(inputs.some(x=>x.sellers.some(s=>s.pubkey===seller)),'planner sees the accepted seller');
  } else {
    assert.equal(own.membershipStatus,'left');assert.equal(own.eligible,false,'left at the flooded second wins');
    assert.equal(coordinate(snap,{now:300,durationSeconds:30}).sellers.length,0);
  }
});

test('R4-2: a relay repeating an identical full page gets at most two requests, then fails closed',async t=>{
  // Junk from the accepted seller itself (the only author the trusted read can see); relay ignores until.
  const own=Array.from({length:500},(_,i)=>junk(SELLER_KEY,100,i));
  const {reader,relays}=open(t,{challenge:[day,accept(seller),...own],options:{ignoreUntil:true}});
  const history=()=>relays['ws://a'].reqs.filter(m=>m[0]==='REQ'&&m[2].kinds[0]===30342&&m[2].limit===500);
  await until(()=>history().length>=2);
  await pause(400);
  assert.equal(history().length,2,'no third identical REQ');
  const a=reader.status.relays.find(r=>r.url==='ws://a');
  assert.match(a.error,/History truncated/);assert.equal(reader.status.historyComplete,false);
  assert.match(reader.status.error,/ws:\/\/a/);
  assert.equal(relays['ws://a'].reqs.filter(m=>m[0]==='REQ'&&m[1]==='applicants').length,1,'applicant sample: one page per round');
});

test('R4-3: 257 stranger joins cannot block exhaustion of the only accepted seller',async t=>{
  const crowd=Array.from({length:257},(_,i)=>member(hexKey(5000+i),100));
  const prefix=`[challenge:${buyer}:day]\n`;
  const offer=sign(BUYER_KEY,3401,210,[...ns,['p',seller],['i',prefix+'Task'],['param','deadline','1000'],['param','payment','none']]);
  const claim=sign(SELLER_KEY,3402,211,[...ns,['e',offer.id,'','root'],['p',buyer],['status','processing']]);
  const ref=[['e',claim.id],['e',offer.id,'','root'],['p',buyer],['p',seller],['status','accepted']];
  const award=sign(BUYER_KEY,3405,212,[...ns,...ref]),done=sign(BUYER_KEY,3406,272,[...ns,...ref]);
  const {store,reader}=open(t,{challenge:[day,accept(seller),member(SELLER_KEY,100,'joined','1'),...crowd],trade:[offer,claim,award,done]});
  await until(()=>reader.status.historyComplete&&reader.status.authenticated&&store.snapshot(buyer,300).jobs.length===1);
  const snap={...store.snapshot(buyer,300),relay:reader.status};
  const own=snap.sellers.find(s=>s.pubkey===seller);
  assert.equal(own.eligible,true);assert.equal(own.minutesRemaining,0);
  assert.deepEqual(snap.relay.warnings,[]);
  const {publishes}=await tick(snap,300);
  assert.equal(publishes.at(-1)?.[2],'completed','exhausted stop must publish');assert.match(publishes.at(-1)[3],/all eligible sellers exhausted/);
});

test('R4-4: one stored membership per seller, oversized content dropped, newer tags win',()=>{
  const store=new EventStore();
  const older=member(SELLER_KEY,100),newer=member(SELLER_KEY,101,'left','20','100','x'.repeat(200000));
  for(const e of [accept(seller),day,older,newer])store.add(e);
  const kept=[...store.events.values()].filter(e=>e.kind===30342&&e.pubkey===seller);
  assert.equal(kept.length,1);assert.equal(kept[0].id,newer.id);assert.equal(kept[0].content,'');
  assert.equal(store.add(older),false,'an older version never comes back');
  const s=store.snapshot(buyer,300).sellers.find(x=>x.pubkey===seller);
  assert.equal(s.membershipStatus,'left');assert.equal(s.minutesOffered,20);assert.equal(s.note,'');
});

test('applicant list shows new joiners; a flood there never changes runner readiness',async t=>{
  const newcomer=member(hexKey(9000),120);
  const crowd=Array.from({length:300},(_,i)=>member(hexKey(6000+i),100));
  const {store,reader}=open(t,{challenge:[day,accept(seller),member(SELLER_KEY,100),newcomer,...crowd]});
  await until(()=>reader.status.historyComplete&&reader.status.authenticated&&store.applicants.size>0);
  const snap={...store.snapshot(buyer,300),relay:reader.status};
  assert.equal(snap.applicants[0].pubkey,pub(hexKey(9000)),'newest join listed first for approval');
  // The capped sample may include the accepted seller's own membership, which is not listed as an applicant.
  assert.ok(snap.applicants.length>=255&&snap.applicants.length<=256);assert.equal(store.applicants.size,256);
  assert.equal(snap.relay.applicantsIncomplete,true,'page marks the list possibly incomplete');
  assert.ok(!snap.applicants.some(a=>a.pubkey===seller),'accepted sellers are not applicants');
  assert.deepEqual(snap.sellers.map(s=>s.pubkey),[seller],'applicants are never sellers');
  assert.equal(snap.relay.historyComplete,true);assert.equal(snap.relay.error,null);assert.deepEqual(snap.relay.warnings,[]);
  assert.equal(coordinate(snap,{now:300,durationSeconds:30}).postingAllowed,true);
});

test('shared roster fixture: JS eligible set ignores strangers (crates/maxplayer-core checks the same file)',async()=>{
  const {readFileSync}=await import('node:fs');
  const f=JSON.parse(readFileSync(new URL('./fixtures/challenge-roster.json',import.meta.url),'utf8'));
  const store=new EventStore();for(const e of f.events)store.add(e);
  const s=store.snapshot(f.buyer,300);
  assert.deepEqual(s.sellers.filter(x=>x.eligible).map(x=>x.pubkey).sort(),f.eligible);
  const {acceptedSellers}=await import('../src/challenge/shared/model.mjs');
  assert.ok(acceptedSellers(f.events,f.buyer).every(pk=>s.sellers.some(x=>x.pubkey===pk)||!f.events.some(e=>e.kind===30342&&e.pubkey===pk)));
});
