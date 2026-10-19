// P-4 (run-start feedback): every snapshot re-projected every past challenge of the buyer from all
// trade events. Past challenges' job facts are now cached per challenge event id and invalidated by
// any trade event of that challenge; the output must stay identical to the uncached projection.
import test from 'node:test';
import assert from 'node:assert/strict';
import { schnorr } from '@noble/curves/secp256k1';
import { eventId } from '../src/challenge/shared/events.mjs';
import { EventStore, snapshot, challengeProjections, PAST_CHALLENGE_LIMIT } from '../src/challenge/shared/model.mjs';
const keys=[1,2].map(n=>n.toString(16).padStart(64,'0'));
const [buyer,seller]=keys.map(k=>Buffer.from(schnorr.getPublicKey(k)).toString('hex'));
const event=(kind,who,time,tags,content='')=>{const e={kind,pubkey:[buyer,seller][who],created_at:time,tags:[['t','maxplayer'],['v','1'],...tags],content};e.id=eventId(e);e.sig=Buffer.from(schnorr.sign(e.id,keys[who])).toString('hex');return e;};
// Run k: challenge window [starts, starts+900); one awarded job (offer, claim, award) per run.
function run(k,{status='completed',time=100+k}={}) {
  const starts=1000*(k+1),d=`run-${k}`;
  const c=event(30341,0,time,[['d',d],['title',`Run ${k}`],['starts',String(starts)],['ends',String(starts+900)],['budget','1000','sat'],['status',status]]);
  const offer=event(3401,0,starts+10,[['p',seller],['i',`[challenge:${buyer}:${d}]\nTask ${k}`],['amount','50','sat'],['param','deadline',String(starts+800)]]);
  const claim=event(3402,1,starts+20,[['e',offer.id,'','root'],['p',buyer],['status','processing']]);
  const award=event(3405,0,starts+30,[['e',claim.id],['e',offer.id,'','root'],['p',buyer],['p',seller],['status','accepted']]);
  const result=at=>event(3403,1,at,[['e',offer.id,'','root'],['p',buyer]],`Done ${k}`);
  const accept=at=>event(3406,0,at,[['e',claim.id],['e',offer.id,'','root'],['p',buyer],['p',seller],['status','accepted']]);
  return {d,c,offer,claim,award,result,accept,events:[c,offer,claim,award]};
}
const roster=[event(30342,1,200,[['d',buyer],['since','200'],['p',buyer],['minutes','480'],['status','joined']]),event(30343,0,300,[['d',seller],['p',seller],['decision','accepted']])];
function fixture(n) {
  const runs=Array.from({length:n},(_,k)=>run(k,{status:k===n-1?'in_progress':'completed',time:100+k}));
  const store=new EventStore();for(const e of [...roster,...runs.flatMap(r=>r.events)])assert.ok(store.add(e));
  return {runs,store};
}
const uncached=(store,now)=>snapshot([...store.events.values()],buyer,now,null,true,[...store.applicants.values()]);
const past=s=>s.sellers[0].pastJobs;
test('P-4: cached pastJobs equal the uncached projection, including after late events for a past run',()=>{
  const {runs,store}=fixture(5),now=1000*5+500; // inside the current run's award window
  assert.deepEqual(store.snapshot(buyer,now),uncached(store,now));
  assert.equal(past(store.snapshot(buyer,now)).length,4,'four past runs, the current run still running');
  // Late result, then late accept for run 1 (long closed): the cached projection must pick both up.
  const late=runs[1];
  for(const e of [late.result(late.award.created_at+100),late.accept(late.award.created_at+200)]) {
    assert.ok(store.add(e));
    assert.deepEqual(store.snapshot(buyer,now),uncached(store,now));
  }
  const job=past(store.snapshot(buyer,now)).find(j=>j.challengeId===late.d);
  assert.equal(job.accepted,true);assert.equal(job.result.content,'Done 1');assert.equal(job.usedMinutes,200/60);
  // A late trade event that arrives before its offer (out of order) is still picked up.
  const store2=new EventStore(),r=run(0);
  for(const e of [...roster,run(1,{status:'in_progress',time:200}).c,r.c,r.accept(r.award.created_at+50),r.result(r.award.created_at+10)])store2.add(e);
  store2.snapshot(buyer,now);
  for(const e of [r.claim,r.award,r.offer]){store2.add(e);assert.deepEqual(store2.snapshot(buyer,now),uncached(store2,now));}
  assert.equal(past(store2.snapshot(buyer,now))[0].accepted,true);
  // A new version of a past challenge (new event id, different window) re-projects it.
  const moved=event(30341,0,late.c.created_at+1,[['d',late.d],['title','Moved'],['starts','1'],['ends','2'],['budget','1000','sat'],['status','completed']]);
  store.add(moved);assert.deepEqual(store.snapshot(buyer,now),uncached(store,now));
  assert.equal(past(store.snapshot(buyer,now)).some(j=>j.challengeId===late.d),false,'job outside the moved window');
});
test('P-4: an unchanged past challenge is projected once, not on every snapshot; a late event re-projects only its run',()=>{
  const {runs,store}=fixture(8),now=1000*9+500;
  let before=challengeProjections();store.snapshot(buyer,now);
  assert.equal(challengeProjections()-before,8,'first snapshot: every challenge once');
  before=challengeProjections();for(let i=0;i<10;i++)store.snapshot(buyer,now+i);
  assert.equal(challengeProjections()-before,0,'ten more ticks: nothing re-projected (uncached: 80)');
  before=challengeProjections();uncached(store,now);
  assert.equal(challengeProjections()-before,8,'the uncached projection re-projects all 8');
  before=challengeProjections();store.add(runs[2].result(runs[2].award.created_at+60));store.snapshot(buyer,now);
  assert.equal(challengeProjections()-before,1,'late result: only run 2 re-projected');
  before=challengeProjections();store.add(runs[7].result(runs[7].award.created_at+60));store.snapshot(buyer,now);
  assert.equal(challengeProjections()-before,1,'current run activity: only the current run re-projected');
});
test(`P-4: pastJobs cover at most the newest ${PAST_CHALLENGE_LIMIT} past challenges`,()=>{
  assert.equal(PAST_CHALLENGE_LIMIT,50);
  const {store}=fixture(PAST_CHALLENGE_LIMIT+3),now=1000*(PAST_CHALLENGE_LIMIT+3)+500;
  const ids=past(store.snapshot(buyer,now)).map(j=>j.challengeId);
  assert.equal(ids.length,PAST_CHALLENGE_LIMIT);
  assert.deepEqual(new Set(ids),new Set(Array.from({length:PAST_CHALLENGE_LIMIT},(_,i)=>`run-${PAST_CHALLENGE_LIMIT+1-i}`)),'newest past runs kept, oldest two dropped');
  assert.deepEqual(store.snapshot(buyer,now),uncached(store,now));
});
