import test from 'node:test';
import assert from 'node:assert/strict';
import { schnorr } from '@noble/curves/secp256k1';
import { eventId,verify,signAuth,npubEncode,validChallenge,validMembership,validDecision } from '../src/events.mjs';
import { EventStore } from '../src/model.mjs';
import { config,review } from '../src/server.mjs';
const keys=[1,2,3].map(n=>n.toString(16).padStart(64,'0'));
const [buyer,seller,stranger]=keys.map(k=>Buffer.from(schnorr.getPublicKey(k)).toString('hex'));
function event(kind,who,time,tags,content='') {const e={kind,pubkey:[buyer,seller,stranger][who],created_at:time,tags:[['t','maxplayer'],['v','1'],...tags],content};e.id=eventId(e);e.sig=Buffer.from(schnorr.sign(e.id,keys[who])).toString('hex');return e;}
const c=event(30341,0,100,[['d','test'],['title','Test'],['starts','4000'],['ends','10000'],['budget','1000','sat'],['status','in_progress']]);
const app=(time=200,status='joined',minutes='60')=>event(30342,1,time,[['d',buyer],['since',String(time)],['p',buyer],['minutes',minutes],['status',status]]);
const decision=(time=300,value='accepted',who=0)=>event(30343,who,time,[['d',seller],['p',seller],['decision',value]]);
const offer=event(3401,0,4000,[['p',seller],['i','Build'],['amount','50','sat'],['param','deadline','7600']]);
const claim=event(3402,1,4010,[['e',offer.id,'','root'],['p',buyer],['status','processing']]);
const award=(who=0,time=4020)=>event(3405,who,time,[['e',claim.id],['e',offer.id,'','root'],['p',buyer],['p',seller],['status','accepted']]);
const result=(who=1,time=5940)=>event(3403,who,time,[['e',offer.id,'','root'],['p',buyer]],'Done');
const base=[c,app(),decision(),offer,claim,award()];
function project(events=base,now=6000){const store=new EventStore();for(const e of events)store.add(e);return store.snapshot(buyer,now);}
test('normal result tallies 32 of 60 minutes and public committed price',()=>{const s=project([...base,result()]);assert.equal(s.sellers[0].minutesUsed,32);assert.equal(s.sellers[0].minutesRemaining,28);assert.equal(s.sellers[0].state,'done');assert.equal(s.budget.committedSats,50);});
test('no result before deadline runs and after deadline caps',()=>{assert.equal(project(base,4080).jobs[0].usedMinutes,1);assert.equal(project(base,4080).jobs[0].running,true);assert.equal(project(base,8000).jobs[0].usedMinutes,3580/60);assert.equal(project(base,8000).jobs[0].running,false);});
test('wrong result author ignored',()=>assert.equal(project([...base,result(2)],6000).jobs[0].running,true));
test('wrong award author ignored',()=>assert.equal(project([...base.slice(0,-1),award(2)]).jobs.length,0));
test('late membership retained but ineligible, exact start boundary accepted',()=>{assert.equal(project([c,app(4001),decision()]).sellers[0].eligible,false);assert.equal(project([c,app(4000),decision()]).sellers[0].eligible,true);});
test('leave keeps awarded work but stops eligibility',()=>{const s=project([...base,app(400,'left')]);assert.equal(s.sellers[0].state,'left');assert.equal(s.sellers[0].eligible,false);assert.ok(s.sellers[0].minutesUsed>0);});
test('decision flip uses latest signed buyer decision',()=>{const s=project([...base,decision(500,'denied'),decision(600,'accepted',2)]);assert.equal(s.sellers[0].state,'denied');assert.equal(s.sellers[0].eligible,false);});
test('late result is capped at deadline, negative durations floor at zero',()=>{assert.equal(project([...base,result(1,9000)]).jobs[0].usedMinutes,3580/60);assert.equal(project([...base,result(1,4010)]).jobs[0].usedMinutes,0);});
test('ties select lowest id regardless of ingestion order',()=>{const a=app(200,'joined','30'),b=app(200,'left');const expected=a.id<b.id?a:b;for(const list of [[a,b],[b,a]]){const s=project([c,...list]);assert.equal(s.sellers[0].minutesOffered,+expected.tags.find(t=>t[0]==='minutes')[1]);assert.equal(s.sellers[0].state,expected===b?'left':'joined');}});
test('accept joins offer and claim, not result; fake buyer cannot mark paid',()=>{const accept=who=>event(3406,who,6100,[['e',claim.id],['e',offer.id,'','root'],['p',buyer],['p',seller],['status','accepted']]);assert.equal(project([...base,result(),accept(2)]).budget.paidSats,0);const b=project([...base,result(),accept(0)]).budget;assert.equal(b.committedSats,0);assert.equal(b.paidSats,50);assert.equal(b.estimated,true);});
test('missing claim or mismatched offer cannot start work; duplicate awards count once',()=>{assert.equal(project(base.filter(e=>e!==claim)).jobs.length,0);assert.equal(project([...base,award(0,4030)]).jobs.length,1);});
test('local verification rejects tampered id signature content timestamp and tags',()=>{assert.ok(verify(c));for(const mutation of [{id:'0'.repeat(64)},{sig:'0'.repeat(128)},{content:'changed'},{created_at:1.1},{tags:[['d',3]]}])assert.equal(new EventStore().add({...c,...mutation}),false);});
test('validation rejects malformed required tags, unknown values and numbers',()=>{assert.ok(validChallenge(c,buyer));assert.equal(validChallenge(c,stranger),false);for(const key of ['d','title','starts','ends','budget','status'])assert.equal(validChallenge({...c,tags:c.tags.filter(t=>t[0]!==key)},buyer),false);for(const v of ['0','481','-1','1.5','NaN','9007199254740992'])assert.equal(validMembership(app(200,'joined',v),c),false);assert.equal(validMembership(app(200,'other'),c),false);assert.equal(validDecision(decision(300,'other'),c),false);assert.equal(validDecision(decision(300,'accepted',2),c),false);});
test('out-of-order ingestion is recomputed after challenge arrives',()=>assert.equal(project([...base].reverse()).sellers.length,1));
test('NIP42 auth is signed with app key and has no EVENT payload',()=>{const e=signAuth(keys[2],'wss://relay.maxplayer.ai','nonce');assert.ok(verify(e));assert.equal(e.kind,22242);assert.equal(e.pubkey,stranger);});
test('config supports hex and npub; malformed keys fail',()=>{assert.equal(config({CHALLENGE_BUYER_PUBKEY:npubEncode(buyer)}).buyer,buyer);assert.throws(()=>config({CHALLENGE_BUYER_PUBKEY:'nsec1bad'}));});
test('review escapes seller text and offers exact scoped commands',()=>{const s=project();s.sellers[0].name='<script>alert(1)</script>';const html=review(s);assert.ok(!html.includes('<script>'));assert.ok(html.includes(`maxplayer challenge decide --seller ${npubEncode(seller)} --accept`));});
test('missing/malformed deadline and ambiguous root references never tally',()=>{
  const bad={...offer,tags:offer.tags.filter(t=>t[0]!=='param')};bad.id=eventId(bad);bad.sig=Buffer.from(schnorr.sign(bad.id,keys[0])).toString('hex');assert.equal(project([c,app(),decision(),bad,claim,award()]).jobs.length,0);
  const a=award();a.tags=[...a.tags,['e','f'.repeat(64),'','root']];a.id=eventId(a);a.sig=Buffer.from(schnorr.sign(a.id,keys[0])).toString('hex');assert.equal(project([...base.slice(0,-1),a]).jobs.length,0);
});
test('challenge replacement ties choose lowest id and invalid replacement cannot hide valid one',()=>{const other=event(30341,0,c.created_at,c.tags.filter(t=>!['t','v'].includes(t[0])), 'Other goal');assert.equal(project([c,other]).challenge.eventId,[c.id,other.id].sort()[0]);const invalid=event(30341,0,101,c.tags.filter(t=>!['t','v','status'].includes(t[0])));assert.equal(project([c,invalid]).challenge.eventId,c.id);});
test('unknown amount is explicit, no-payment award is zero',()=>{const build=payment=>{const o=event(3401,0,4000,[['p',seller],['param','deadline','7600'],...(payment?[['param','payment','none']]:[])]);const cl=event(3402,1,4010,[['e',o.id,'','root'],['p',buyer],['status','processing']]);const a=event(3405,0,4020,[['e',cl.id],['e',o.id,'','root'],['p',buyer],['p',seller],['status','accepted']]);return project([c,app(),o,cl,a]);};assert.equal(build(false).budget.unknownAmounts,1);assert.equal(build(true).budget.unknownAmounts,0);assert.equal(build(true).budget.committedSats,0);});
test('review history includes completed jobs from older known challenges',()=>{const old=event(30341,0,99,[['d','old'],['title','Old'],['starts','3900'],['ends','4001'],['budget','100','sat'],['status','completed']]);const s=project([...base,old,result()]);assert.equal(s.sellers[0].pastJobs.length,1,'same offer in overlapping windows is deduplicated');assert.equal(s.sellers[0].pastJobs[0].challengeId,'test');});
// Real buyer MCP uses v2 for PUBLIC jobs, while challenge kinds remain v1.
function publicV2(kind,who,time,tags,content='') {
  const e=event(kind,who,time,tags,content);
  e.tags=e.tags.map(t=>t[0]==='v'?['v','2']:t).concat([['visibility','public'],['job','a'.repeat(64)]]);
  e.id=eventId(e);e.sig=Buffer.from(schnorr.sign(e.id,keys[who])).toString('hex');return e;
}
test('existing public-v2 free trade tallies running, result and accept with v1 challenge',()=>{
  const o=publicV2(3401,0,4000,[['p',seller],['i','Free task'],['amount','0','sat'],['param','payment','none'],['param','deadline','7600']]);
  const cl=publicV2(3402,1,4010,[['e',o.id,'','root'],['p',buyer],['p',seller],['status','processing'],['payment','none']]);
  const aw=publicV2(3405,0,4020,[['e',o.id,'','root'],['e',cl.id],['p',buyer],['p',seller],['status','accepted']]);
  const r=publicV2(3403,1,4080,[['e',o.id,'','root'],['p',buyer]],'Answer');
  const ac=publicV2(3406,0,4090,[['e',o.id,'','root'],['e',cl.id],['p',buyer],['p',seller],['status','accepted']]);
  const events=[c,app(),decision(),o,cl,aw];
  const running=project(events,4050);assert.equal(running.jobs.length,1);assert.equal(running.sellers[0].state,'working');assert.equal(running.sellers[0].minutesUsed,.5);
  const done=project([...events,r,ac],4100);assert.equal(done.sellers[0].state,'done');assert.equal(done.sellers[0].minutesUsed,1);assert.equal(done.jobs[0].accepted,true);assert.equal(done.budget.unknownAmounts,0);assert.equal(done.budget.committedSats+done.budget.paidSats,0);
});
test('public-v2 admission does not relax challenge-v1 or admit private/malformed envelopes',async()=>{
  const {tradeNamespace}=await import('../src/events.mjs');
  const e=publicV2(3401,0,4000,[['p',seller]]);assert.ok(tradeNamespace(e));
  for(const tags of [e.tags.filter(t=>t[0]!=='job'),e.tags.filter(t=>t[0]!=='visibility'),e.tags.map(t=>t[0]==='visibility'?['visibility','private']:t),e.tags.map(t=>t[0]==='v'?['v','3']:t),[...e.tags,['v','1']]])assert.equal(tradeNamespace({...e,tags}),false);
  const bad=publicV2(30341,0,100,c.tags.filter(t=>!['t','v'].includes(t[0])));assert.equal(validChallenge(bad,buyer),false);assert.equal(project([bad,app()]).challenge,null);
});

test('membership and decision persist, daily minutes reset, late join waits for next day',()=>{
  const tomorrow=event(30341,0,101,[['d','tomorrow'],['title','Next'],['starts','90400'],['ends','96400'],['budget','1000','sat'],['status','in_progress']]);
  const today=project([...base,result()]);
  const next=project([...base,result(),tomorrow],90401);
  assert.equal(today.sellers[0].eligible,true);assert.equal(next.sellers[0].eligible,true);
  assert.equal(next.sellers[0].minutesUsed,0);assert.equal(next.sellers[0].minutesRemaining,60);
  assert.equal(next.sellers[0].pastJobs.length,1);
  const late=app(4001);
  assert.equal(project([c,late,decision()]).sellers[0].eligible,false);
  assert.equal(project([c,tomorrow,late,decision()],90401).sellers[0].eligible,true);
  const left=app(95000,'left');
  assert.equal(project([...base,tomorrow,left],95001).sellers[0].eligible,false);
  assert.equal(project([...base,left],95001).sellers[0].minutesUsed,3580/60);
});
test('membership replacement preserves since independently of event timestamp and rejects challenge-scoped records',()=>{
  const changed=event(30342,1,9000,[['d',buyer],['p',buyer],['minutes','120'],['since','200'],['status','joined']]);
  const s=project([...base,changed]);assert.equal(s.sellers[0].eligible,true);assert.equal(s.sellers[0].minutesOffered,120);
  for(const key of ['d','p','minutes','since','status']) {
    assert.equal(validMembership({...changed,tags:changed.tags.filter(t=>t[0]!==key)},c),false);
    assert.equal(validMembership({...changed,tags:[...changed.tags,changed.tags.find(t=>t[0]===key)]},c),false);
  }
  assert.equal(validMembership({...changed,tags:[...changed.tags,['a',`30341:${buyer}:test`]]},c),false);
  const d=decision();assert.equal(validDecision({...d,tags:[...d.tags,['a',`30341:${buyer}:test`]]},c),false);
});

test('required tags have exact arity and unique namespace in every challenge event',()=>{
  for(const [e,validate] of [[c,e=>validChallenge(e,buyer)],[app(),e=>validMembership(e,c)],[decision(),e=>validDecision(e,c)]]) {
    for(const key of ['t','v','d']) {
      assert.equal(validate({...e,tags:[...e.tags,e.tags.find(t=>t[0]===key)]}),false);
      assert.equal(validate({...e,tags:e.tags.map(t=>t[0]===key?[...t,'extra']:t)}),false);
    }
  }
});

test('challenge requires ends after starts and snapshot exposes only supported fields',()=>{
  for(const end of ['4000','3999']) assert.equal(validChallenge({...c,tags:c.tags.map(t=>t[0]==='ends'?['ends',end]:t)},buyer),false);
  assert.deepEqual(Object.keys(project().challenge).sort(),['id','eventId','buyer','buyerNpub','title','goal','starts','ends','status','result'].sort());
});

test('seller heartbeat pricing survives projection for the buyer planner',()=>{
  const beat=event(30340,1,3900,[['d','maxplayer-seller'],['rate','5'],['takes_payment','none']]);
  const profile=project([c,app(),decision(),beat]).sellers[0];
  assert.ok(profile.capabilities.some(t=>t[0]==='rate'&&t[1]==='5'));
  assert.ok(profile.capabilities.some(t=>t[0]==='takes_payment'&&t[1]==='none'));
});
