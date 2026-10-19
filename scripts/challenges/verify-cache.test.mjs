// R3-6: persisted verified-signature cache (verify-cache-file.mjs) and the runner's cheap relay callback.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, writeFileSync, readFileSync, existsSync, readdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { schnorr } from '../../web/app/node_modules/@noble/curves/esm/secp256k1.js';
import { eventId, verify, clearVerifyCache, verifyCacheKeys, verifyCacheSize, primeVerifyCache } from '../../web/app/src/challenge/shared/events.mjs';
import { EventStore } from '../../web/app/src/challenge/shared/model.mjs';
import { encodeVerifyCache, decodeVerifyCache, loadVerifyCache, saveVerifyCache, persistVerifyCache, verifyCacheMaxBytes, VERIFY_CACHE_FILE } from './verify-cache-file.mjs';
const key='01'.padStart(64,'0'),other='02'.padStart(64,'0'),pub=k=>Buffer.from(schnorr.getPublicKey(k)).toString('hex');
const sign=(content,k=key,created_at=100)=>{const e={pubkey:pub(k),kind:1,created_at,tags:[],content};e.id=eventId(e);e.sig=Buffer.from(schnorr.sign(e.id,k)).toString('hex');return e;};
const events=[sign('a'),sign('b'),sign('c',other)];
const k=e=>e.id+e.pubkey+e.sig;
function home(t){const d=mkdtempSync(join(tmpdir(),'swarm-vc-'));t.after(()=>rmSync(d,{recursive:true,force:true}));clearVerifyCache();t.after(clearVerifyCache);return join(d,VERIFY_CACHE_FILE);}
const spy=t=>t.mock.method(schnorr,'verify');

test('R3-6 cache: keys verified in one process are loaded by the next, which re-verifies nothing',t=>{
  const path=home(t);
  const first=persistVerifyCache(path,{flushMs:1e9});
  assert.equal(first.loaded.status,'missing');
  for(const e of events)assert.equal(verify(e),true);
  first.stop();
  assert.ok(existsSync(path));assert.equal(readFileSync(path,'latin1').split('\n')[0].split(' ')[2],'3');
  // "Restart": empty process cache, load, then the same history costs zero Schnorr checks.
  clearVerifyCache();
  const reports=[],second=persistVerifyCache(path,{flushMs:1e9,report:m=>reports.push(m)});t.after(second.stop);
  assert.equal(second.loaded.status,'loaded');assert.equal(second.loaded.count,3);assert.match(reports[0],/loaded 3 keys/);
  const calls=spy(t);
  for(const e of events){assert.equal(verify(structuredClone(e)),true);assert.equal(new EventStore().add(structuredClone(e)),true);}
  assert.equal(calls.mock.callCount(),0,'every signature came from the persisted cache');
});
test('R3-6 cache: a persisted key accepts only that exact id+pubkey+sig; a different sig or changed event is checked and rejected',t=>{
  const path=home(t),e=events[0];
  saveVerifyCache(path,[k(e)]);
  primeVerifyCache(loadVerifyCache(path).keys);
  const calls=spy(t);
  assert.equal(verify(e),true);assert.equal(calls.mock.callCount(),0);
  const foreign=Buffer.from(schnorr.sign(e.id,other)).toString('hex'),stolen=events[1].sig,junk='ab'.repeat(64);
  for(const sig of [foreign,stolen,junk])assert.equal(verify({...e,sig}),false,'a different sig is never a cache hit');
  assert.equal(calls.mock.callCount(),3,'each different sig was Schnorr-checked');
  for(const change of [{content:'tampered'},{created_at:101},{kind:2},{tags:[['p',pub(other)]]},{pubkey:pub(other)}])
    assert.equal(verify({...e,...change}),false,`a cached key never accepts changed ${Object.keys(change)[0]}`);
  // A key planted for a sig that does NOT verify still cannot accept a changed event (id is recomputed).
  clearVerifyCache();primeVerifyCache([e.id+e.pubkey+junk]);
  assert.equal(verify({...e,content:'x',sig:junk}),false);
});
test('R3-6 cache: missing, torn, truncated, edited, oversized or malformed files load nothing (empty cache), and are rewritten',t=>{
  const path=home(t),good=encodeVerifyCache(events.map(k));
  assert.deepEqual(decodeVerifyCache(good).keys,events.map(k));
  assert.equal(loadVerifyCache(path).status,'missing');
  const bad={
    torn:good.slice(0,-100),truncatedHeader:good.slice(0,20),empty:'',
    edited:good.replace(events[0].sig,'f'.repeat(128)),
    countLie:good.replace(' 3 ',' 2 '),
    malformed:encodeVerifyCache(['X'.repeat(256)]),
    wrongLength:encodeVerifyCache([k(events[0]).slice(0,255)+'\n'.slice(1)]),
    foreign:'{"jobs":[]}\n',
    overLimit:encodeVerifyCache(events.map(k)),
  };
  for(const [name,text] of Object.entries(bad)) {
    writeFileSync(path,text,'latin1');
    const r=loadVerifyCache(path,{limit:name==='overLimit'?2:undefined});
    assert.equal(r.status,'corrupt',name);assert.deepEqual(r.keys,[],name);
  }
  // A file above the byte bound is not even read.
  let read=0;
  assert.equal(loadVerifyCache(path,{stat:()=>({size:verifyCacheMaxBytes()+1}),read:()=>{read++;return good;}}).status,'corrupt');assert.equal(read,0);
  // Startup with a corrupt file: warned, empty, every signature verified again, then rewritten whole.
  writeFileSync(path,good.slice(0,-1),'latin1');
  const reports=[],p=persistVerifyCache(path,{flushMs:1e9,report:m=>reports.push(m)});
  assert.equal(p.loaded.status,'corrupt');assert.equal(verifyCacheSize(),0);assert.match(reports[0],/^WARNING: .*unusable .*starting empty/);
  const calls=spy(t);for(const e of events)assert.equal(verify(e),true);assert.equal(calls.mock.callCount(),3);
  p.stop();assert.equal(loadVerifyCache(path).status,'loaded');assert.equal(loadVerifyCache(path).keys.length,3);
});
test('R3-6 cache: a failed write never replaces the previous file, leaves no temp, and is retried; flush only when new keys arrived',t=>{
  const path=home(t);
  saveVerifyCache(path,[k(events[0])]);const before=readFileSync(path,'latin1');
  for(const fail of ['writeSync','fsyncSync','renameSync']) {
    assert.throws(()=>saveVerifyCache(path,events.map(k),{fs:{[fail]:()=>{throw Object.assign(Error(`${fail} failed`),{code:'EIO'});}}}),new RegExp(fail));
    assert.equal(readFileSync(path,'latin1'),before,`${fail}: previous file intact`);
    assert.deepEqual(readdirSync(join(path,'..')).filter(n=>n.endsWith('.tmp')),[],`${fail}: no temp left`);
  }
  primeVerifyCache(loadVerifyCache(path).keys);
  let broken=true;const reports=[];
  const p=persistVerifyCache(path,{flushMs:1e9,report:m=>reports.push(m),fs:{renameSync:(a,b)=>{if(broken)throw Error('disk full');return process.getBuiltinModule('node:fs').renameSync(a,b);}}});
  assert.equal(p.flush(),false,'nothing new: no write');
  verify(events[1]);
  assert.equal(p.flush(),false);assert.match(reports.at(-1),/not saved \(disk full\); will retry/);
  broken=false;assert.equal(p.flush(),true);assert.equal(p.flush(),false);
  p.stop();
  assert.deepEqual(loadVerifyCache(path).keys.sort(),[k(events[0]),k(events[1])].sort());
  // After stop no listener remains (a later verify never marks a stopped cache dirty).
  verify(events[2]);assert.equal(p.flush(),false);
});
test('R3-6 cache: the persisted file is bounded by the in-memory cache bound',async t=>{
  const path=home(t);
  const {VERIFY_CACHE_LIMIT}=await import('../../web/app/src/challenge/shared/events.mjs');
  const keys=Array.from({length:VERIFY_CACHE_LIMIT+50},(_,i)=>i.toString(16).padStart(256,'0'));
  assert.equal(primeVerifyCache(keys),VERIFY_CACHE_LIMIT+50);assert.equal(verifyCacheSize(),VERIFY_CACHE_LIMIT);
  saveVerifyCache(path,verifyCacheKeys());
  const r=loadVerifyCache(path);assert.equal(r.status,'loaded');assert.equal(r.keys.length,VERIFY_CACHE_LIMIT);
  assert.ok(readFileSync(path).length<=verifyCacheMaxBytes());
  // Malformed keys are never primed.
  clearVerifyCache();assert.equal(primeVerifyCache(['zz'.repeat(128),'0'.repeat(255),42,null]),0);
});
test('R3-6 projection: the runner callback (onRelay) gets relay status per frame without projecting history; onChange still projects',async t=>{
  const {openSnapshot}=await import('./relay-snapshot.mjs');
  const projections=t.mock.method(EventStore.prototype,'snapshot');
  class Fake extends EventTarget {readyState=1;sent=[];constructor(url){super();this.url=url;Fake.all.push(this);}send(d){this.sent.push(JSON.parse(d));}close(){this.readyState=3;}static all=[];}
  const buyerKey='03'.padStart(64,'0'),buyer=pub(buyerKey);
  const frames=Array.from({length:20},(_,i)=>{const e={pubkey:buyer,kind:30341,created_at:100+i,content:'',tags:[['t','maxplayer'],['v','1'],['d','x'+i],['title','T'],['starts','1'],['ends','2'],['budget','1','sat'],['status','in_progress']]};e.id=eventId(e);e.sig=Buffer.from(schnorr.sign(e.id,buyerKey)).toString('hex');return e;});
  const run=async opts=>{
    Fake.all=[];const before=projections.mock.callCount(),seen=[];
    const src=openSnapshot({url:'ws://trade',challengeRelays:['ws://c1'],buyer,WebSocketImpl:Fake,...opts(seen)});t.after(()=>src.stop());
    const c1=Fake.all.find(s=>s.url==='ws://c1');c1.dispatchEvent(new Event('open'));
    for(const e of frames)c1.dispatchEvent(new MessageEvent('message',{data:JSON.stringify(['EVENT','live-30341',e])}));
    await new Promise(r=>setTimeout(r,20));
    for(const e of frames)c1.dispatchEvent(new MessageEvent('message',{data:JSON.stringify(['EVENT','30341',e])}));
    src.stop();return {projections:projections.mock.callCount()-before,seen};
  };
  const runner=await run(seen=>({onRelay:r=>seen.push(r)}));
  assert.ok(runner.seen.length>=20,'relay status reported on every change');assert.ok('historyComplete' in runner.seen[0]&&'quorum' in runner.seen[0]);
  assert.equal(runner.projections,0,'no per-frame projection for the runner');
  const writer=await run(seen=>({onChange:s=>seen.push(s)}));
  assert.ok(writer.projections>=20&&'jobs' in writer.seen[0],'the E2E artifact writer still gets full snapshots');
});
